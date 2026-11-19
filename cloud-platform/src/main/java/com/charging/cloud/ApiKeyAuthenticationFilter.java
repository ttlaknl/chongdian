package com.charging.cloud;

import jakarta.servlet.FilterChain;
import jakarta.servlet.ReadListener;
import jakarta.servlet.ServletException;
import jakarta.servlet.ServletInputStream;
import jakarta.servlet.http.HttpServletRequest;
import jakarta.servlet.http.HttpServletRequestWrapper;
import jakarta.servlet.http.HttpServletResponse;
import java.io.BufferedReader;
import java.io.ByteArrayInputStream;
import java.io.IOException;
import java.io.InputStreamReader;
import java.nio.charset.StandardCharsets;
import java.security.MessageDigest;
import java.time.Instant;
import java.util.HexFormat;
import java.util.concurrent.ConcurrentHashMap;
import javax.crypto.Mac;
import javax.crypto.spec.SecretKeySpec;
import org.springframework.beans.factory.annotation.Autowired;
import org.springframework.beans.factory.annotation.Value;
import org.springframework.core.Ordered;
import org.springframework.core.annotation.Order;
import org.springframework.stereotype.Component;
import org.springframework.web.filter.OncePerRequestFilter;

/**
 * 安全过滤器。
 *
 * <p>人工管理员（ADMIN/OPERATOR）通过 Session Cookie 或 Bearer Token 认证。
 * <p>边缘工控机（EDGE）通过 X-Api-Key + HMAC-SHA256 签名认证，密钥为 EDGE_API_KEY 环境变量。
 */
@Component
@Order(Ordered.HIGHEST_PRECEDENCE + 20)
public class ApiKeyAuthenticationFilter extends OncePerRequestFilter {
  private final boolean enabled;
  private final String edgeKey;
  private final TokenService tokenService;
  private final ConcurrentHashMap<String, Long> usedNonces = new ConcurrentHashMap<>();

  @Autowired
  public ApiKeyAuthenticationFilter(
      @Value("${app.security.enabled:false}") boolean enabled,
      @Value("${app.security.edge-api-key:}") String edgeKey,
      @Autowired(required = false) TokenService tokenService) {
    this.enabled = enabled;
    this.edgeKey = edgeKey == null ? "" : edgeKey;
    this.tokenService = tokenService;
    if (enabled && this.edgeKey.isBlank()) {
      throw new IllegalStateException("security is enabled but EDGE_API_KEY is empty");
    }
  }

  @Override
  protected void doFilterInternal(HttpServletRequest request, HttpServletResponse response, FilterChain chain)
      throws ServletException, IOException {
    // 安全响应头（纵深防御）
    response.setHeader("X-Content-Type-Options", "nosniff");
    response.setHeader("X-Frame-Options", "DENY");
    response.setHeader("Referrer-Policy", "no-referrer");
    response.setHeader("Content-Security-Policy",
        "default-src 'self'; script-src 'self' 'unsafe-inline' 'unsafe-eval'; style-src 'self' 'unsafe-inline'; img-src 'self' data: https:; font-src 'self'; connect-src 'self'; frame-ancestors 'none'; base-uri 'self'; form-action 'self'");
    response.setHeader("Permissions-Policy", "camera=(), microphone=(), geolocation=()");
    if (!enabled || isPublic(request)) {
      chain.doFilter(request, response);
      return;
    }
    byte[] body = request.getInputStream().readAllBytes();
    HttpServletRequest wrappedRequest = new CachedBodyRequest(request, body);
    String requiredRole = roleFor(wrappedRequest);
    // 1. 先尝试 Session / Bearer Token 认证（账号密码登录）
    String authenticatedRole = sessionRole(wrappedRequest, requiredRole);
    // 2. Edge 机器节点用 HMAC API Key 认证
    if (authenticatedRole == null && "EDGE".equals(requiredRole)) {
      authenticatedRole = authenticateEdge(wrappedRequest, body);
      if (authenticatedRole != null) {
        String nodeId = wrappedRequest.getHeader("X-Node-Id");
        if (nodeId != null && !nodeId.isBlank()) {
          wrappedRequest.setAttribute("authUsername", nodeId);
        }
      }
    }
    if (authenticatedRole == null) {
      response.setStatus(HttpServletResponse.SC_UNAUTHORIZED);
      response.setHeader("WWW-Authenticate", "Bearer realm=\"chongdian-admin\"");
      response.setContentType("application/json;charset=UTF-8");
      response.getWriter().write("{\"error\":\"unauthorized\",\"message\":\"请登录后台或使用有效的边缘节点密钥\"}");
      return;
    }
    wrappedRequest.setAttribute("authRole", authenticatedRole);
    chain.doFilter(wrappedRequest, response);
  }

  private boolean isPublic(HttpServletRequest request) {
    String path = request.getRequestURI();
    // 所有静态资源和非 API 路径直接放行
    if (!path.startsWith("/api/")) return true;
    String method = request.getMethod();
    if ("GET".equalsIgnoreCase(method)
        && (path.equals("/api/v1/health") || path.equals("/api/v1/auth/session"))) {
      return true;
    }
    // 登录/登出无需认证
    if (path.equals("/api/v1/auth/login") || path.equals("/api/v1/auth/logout")) return true;
    // 监控大屏免密公开（只读，且配置中的摄像头密码已脱敏）
    if ("GET".equalsIgnoreCase(method)
        && (path.equals("/api/v1/dashboard/stats")
            || path.equals("/api/v1/spaces/status")
            || path.equals("/api/v1/events")
            || path.equals("/api/v1/events/export")
            || path.equals("/api/v1/sites")
            || path.equals("/api/v1/sites/summary")
            || path.equals("/api/v1/sites/default/configuration")
            || path.matches("^/api/v1/sites/[^/]+/configuration$")
            || path.matches("^/api/v1/sites/[^/]+/spaces/[^/]+/snapshot$")
            || path.startsWith("/api/v1/evidences/")
            || path.equals("/api/v1/rules")
            || path.matches("^/api/v1/sites/[^/]+/rules$")
            || path.startsWith("/api/v1/firmware/")
            || path.equals("/api/v1/stream/spaces")
            || path.equals("/api/v1/stream/events"))) {
      return true;
    }
    return false;
  }

  private String roleFor(HttpServletRequest request) {
    String path = request.getRequestURI();
    String method = request.getMethod();
    // 边缘节点上报与配置/OTA拉取接口
    if ("POST".equalsIgnoreCase(method)
        && (path.startsWith("/api/v1/nodes/")
            || path.equals("/api/v1/events")
            || path.equals("/api/v1/evidences"))) {
      return "EDGE";
    }
    if ("GET".equalsIgnoreCase(method)
        && (path.equals("/api/v1/configuration") || path.equals("/api/v1/upgrade/manifest"))) {
      return "EDGE";
    }
    // 操作员可执行的接口（消警、导出、查看节点）
    if (("POST".equalsIgnoreCase(method) && path.matches("^/api/v1/events/[^/]+/acknowledge$"))
        || ("GET".equalsIgnoreCase(method) && path.equals("/api/v1/events/export"))
        || ("GET".equalsIgnoreCase(method) && path.startsWith("/api/v1/nodes/"))) {
      return "OPERATOR";
    }
    return "ADMIN";
  }

  /** 边缘节点 HMAC 认证 */
  private String authenticateEdge(HttpServletRequest request, byte[] body) {
    String apiKey = request.getHeader("X-Api-Key");
    if (apiKey == null) return null;
    if (constantTimeEquals(apiKey, edgeKey) && validEdgeSignature(request, body)) return "EDGE";
    return null;
  }

  private boolean validEdgeSignature(HttpServletRequest request, byte[] body) {
    String timestamp = request.getHeader("X-Edge-Timestamp");
    String signature = request.getHeader("X-Edge-Signature");
    String nonce = request.getHeader("X-Edge-Nonce");
    String bodyHash = request.getHeader("X-Content-SHA256");
    String nodeId = request.getHeader("X-Node-Id");
    if (timestamp == null || signature == null || nonce == null || bodyHash == null
        || nodeId == null || nodeId.isBlank() || nonce.isBlank() || nonce.length() > 128) return false;
    try {
      long seconds = Long.parseLong(timestamp);
      long now = Instant.now().getEpochSecond();
      if (seconds < now - 60 || seconds > now + 60) return false;
      String actualBodyHash = sha256Hex(body);
      if (!constantTimeEquals(bodyHash.toLowerCase(), actualBodyHash)) return false;
      if (!nodeMatchesPath(request.getRequestURI(), nodeId)) return false;
      Mac mac = Mac.getInstance("HmacSHA256");
      mac.init(new SecretKeySpec(edgeKey.getBytes(StandardCharsets.UTF_8), "HmacSHA256"));
      String payload = timestamp + "\n" + nonce + "\n" + request.getMethod() + "\n"
          + request.getRequestURI() + "\n" + actualBodyHash + "\n" + nodeId;
      String expected = HexFormat.of().formatHex(mac.doFinal(payload.getBytes(StandardCharsets.UTF_8)));
      if (!constantTimeEquals(signature.toLowerCase(), expected)) return false;
      usedNonces.entrySet().removeIf(entry -> entry.getValue() < now - 60);
      return usedNonces.putIfAbsent(nodeId + ":" + nonce, seconds) == null;
    } catch (Exception error) {
      return false;
    }
  }

  private boolean nodeMatchesPath(String path, String nodeId) {
    String prefix = "/api/v1/nodes/";
    if (!path.startsWith(prefix)) return true;
    int end = path.indexOf('/', prefix.length());
    String pathNodeId = end < 0 ? path.substring(prefix.length()) : path.substring(prefix.length(), end);
    return constantTimeEquals(pathNodeId, nodeId);
  }

  private String sha256Hex(byte[] body) throws Exception {
    return HexFormat.of().formatHex(MessageDigest.getInstance("SHA-256").digest(body));
  }

  private String sessionRole(HttpServletRequest request, String requiredRole) {
    var session = request.getSession(false);
    if (session != null) {
      Object role = session.getAttribute("authRole");
      Object username = session.getAttribute("username");
      if (username != null) {
        request.setAttribute("authUsername", username.toString());
      }
      if ("ADMIN".equals(role)) return "ADMIN";
      if ("OPERATOR".equals(role) && "OPERATOR".equals(requiredRole)) return "OPERATOR";
    }
    // 检查 Bearer Token（解决容器重启后内存 Session 丢失）
    String authHeader = request.getHeader("Authorization");
    if (authHeader != null && authHeader.startsWith("Bearer ") && tokenService != null) {
      String token = authHeader.substring(7).trim();
      TokenService.AuthUser user = tokenService.validateToken(token);
      if (user != null) {
        request.setAttribute("username", user.username());
        request.setAttribute("authUsername", user.username());
        if ("ADMIN".equals(user.role())) return "ADMIN";
        if ("OPERATOR".equals(user.role()) && "OPERATOR".equals(requiredRole)) return "OPERATOR";
      }
    }
    return null;
  }

  private boolean constantTimeEquals(String actual, String expected) {
    if (actual == null || expected == null) return false;
    return MessageDigest.isEqual(
        actual.getBytes(StandardCharsets.UTF_8),
        expected.getBytes(StandardCharsets.UTF_8));
  }

  private static final class CachedBodyRequest extends HttpServletRequestWrapper {
    private final byte[] body;

    private CachedBodyRequest(HttpServletRequest request, byte[] body) {
      super(request);
      this.body = body;
    }

    @Override
    public ServletInputStream getInputStream() {
      ByteArrayInputStream input = new ByteArrayInputStream(body);
      return new ServletInputStream() {
        @Override public boolean isFinished() { return input.available() == 0; }
        @Override public boolean isReady() { return true; }
        @Override public void setReadListener(ReadListener listener) {}
        @Override public int read() { return input.read(); }
      };
    }

    @Override
    public BufferedReader getReader() {
      return new BufferedReader(new InputStreamReader(getInputStream(), StandardCharsets.UTF_8));
    }
  }
}
