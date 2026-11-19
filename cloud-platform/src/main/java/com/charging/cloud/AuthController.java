package com.charging.cloud;

import jakarta.servlet.http.HttpServletRequest;
import jakarta.servlet.http.HttpSession;
import jakarta.validation.Valid;
import java.nio.charset.StandardCharsets;
import java.security.MessageDigest;
import java.time.Instant;
import java.util.Map;
import org.springframework.beans.factory.annotation.Autowired;
import org.springframework.beans.factory.annotation.Value;
import org.springframework.http.HttpStatus;
import org.springframework.http.ResponseEntity;
import org.springframework.web.bind.annotation.*;

@RestController
@RequestMapping("/api/v1/auth")
public class AuthController {
  private final boolean enabled;
  private final String adminUsername;
  private final String adminPassword;
  private final String operatorUsername;
  private final String operatorPassword;
  private final CloudStore store;
  private final TokenService tokenService;

  @Autowired
  public AuthController(
      @Value("${app.security.enabled:false}") boolean enabled,
      @Value("${app.security.admin-username:admin}") String adminUsername,
      @Value("${app.security.admin-password:}") String adminPassword,
      @Value("${app.security.operator-username:}") String operatorUsername,
      @Value("${app.security.operator-password:}") String operatorPassword,
      CloudStore store,
      TokenService tokenService) {
    this.enabled = enabled;
    this.adminUsername = adminUsername == null ? "admin" : adminUsername;
    this.adminPassword = adminPassword == null ? "" : adminPassword;
    this.operatorUsername = operatorUsername == null ? "" : operatorUsername;
    this.operatorPassword = operatorPassword == null ? "" : operatorPassword;
    this.store = store;
    this.tokenService = tokenService;
    if (enabled && (this.adminUsername.isBlank() || this.adminPassword.isBlank())) {
      throw new IllegalStateException("security is enabled but ADMIN_USERNAME or ADMIN_PASSWORD is empty");
    }
    if (enabled && (this.operatorUsername.isBlank() != this.operatorPassword.isBlank())) {
      throw new IllegalStateException("OPERATOR_USERNAME and OPERATOR_PASSWORD must be configured together");
    }
  }

  /** Convenience constructor for tests (security disabled) */
  public AuthController(boolean enabled, String adminUsername, String adminPassword, CloudStore store, TokenService tokenService) {
    this(enabled, adminUsername, adminPassword, "", "", store, tokenService);
  }

  @GetMapping("/session")
  public Map<String, Object> session(HttpServletRequest request) {
    HttpSession session = request.getSession(false);
    if (session != null && session.getAttribute("authRole") != null) {
      String role = (String) session.getAttribute("authRole");
      String username = (String) session.getAttribute("username");
      return Map.of(
          "authenticated", true,
          "role", role,
          "username", username != null ? username : role.toLowerCase(),
          "canConfigure", "ADMIN".equals(role),
          "canAcknowledge", "ADMIN".equals(role) || "OPERATOR".equals(role)
      );
    }
    // Session 不存在时，检查 Bearer Token（解决容器重启后内存 Session 丢失）
    String authHeader = request.getHeader("Authorization");
    if (authHeader != null && authHeader.startsWith("Bearer ") && tokenService != null) {
      String token = authHeader.substring(7).trim();
      TokenService.AuthUser user = tokenService.validateToken(token);
      if (user != null) {
        return Map.of(
            "authenticated", true,
            "role", user.role(),
            "username", user.username(),
            "canConfigure", "ADMIN".equals(user.role()),
            "canAcknowledge", "ADMIN".equals(user.role()) || "OPERATOR".equals(user.role())
        );
      }
    }
    return Map.of(
        "authenticated", false,
        "role", "GUEST",
        "username", "",
        "canConfigure", false,
        "canAcknowledge", false
    );
  }

  @PostMapping("/login")
  public ResponseEntity<Map<String, Object>> login(
      @Valid @RequestBody AdminLoginRequest login,
      HttpServletRequest request) {
    if (enabled) {
      boolean isAdmin = constantTimeEquals(login.username(), adminUsername)
          && constantTimeEquals(login.password(), adminPassword)
          && !adminPassword.isBlank();
      boolean isOperator = !operatorUsername.isBlank()
          && !operatorPassword.isBlank()
          && constantTimeEquals(login.username(), operatorUsername)
          && constantTimeEquals(login.password(), operatorPassword);
      if (!isAdmin && !isOperator) {
        return ResponseEntity.status(HttpStatus.UNAUTHORIZED)
            .body(Map.of("authenticated", false, "error", "用户名或密码错误"));
      }
      String role = isAdmin ? "ADMIN" : "OPERATOR";
      HttpSession session = request.getSession(false);
      if (session == null) {
        session = request.getSession(true);
      } else {
        request.changeSessionId();
      }
      session.setAttribute("authRole", role);
      session.setAttribute("username", login.username());
      session.setMaxInactiveInterval(60 * 60);
      store.audit(login.username(), "LOGIN", "session", role + " login via username/password");

      // 生成持久化 Bearer Token（有效时长 7 天），防止容器重启导致前端鉴权失效
      String token = tokenService != null ? tokenService.generateToken(login.username(), role, 7 * 24 * 3600) : "";
      return ResponseEntity.ok(Map.of(
          "authenticated", true,
          "role", role,
          "username", login.username(),
          "token", token,
          "canConfigure", isAdmin,
          "canAcknowledge", true
      ));
    }
    // Security disabled: reject login rather than auto-granting ADMIN.
    // To enable dev mode explicitly, set CLOUD_SECURITY_ENABLED=false and use
    // the dedicated dev profile; the default profile now requires credentials.
    return ResponseEntity.status(HttpStatus.SERVICE_UNAVAILABLE)
        .body(Map.of(
            "authenticated", false,
            "error", "安全认证已禁用，请联系管理员启用 CLOUD_SECURITY_ENABLED"
        ));
  }

  @PostMapping("/logout")
  public Map<String, Object> logout(HttpServletRequest request) {
    HttpSession session = request.getSession(false);
    if (session != null) {
      String username = (String) session.getAttribute("username");
      if (username != null) store.audit(username, "LOGOUT", "session", "user logout");
      session.invalidate();
    }
    return Map.of("authenticated", false);
  }

  /** 修改密码（仅 ADMIN 可调用，当前版本使用环境变量，此为预留扩展接口） */
  @PostMapping("/change-password")
  public ResponseEntity<Map<String, Object>> changePassword(
      @RequestBody Map<String, String> body,
      HttpServletRequest request) {
    HttpSession session = request.getSession(false);
    if (session == null || !"ADMIN".equals(session.getAttribute("authRole"))) {
      return ResponseEntity.status(HttpStatus.FORBIDDEN)
          .body(Map.of("error", "仅超级管理员可修改密码"));
    }
    // 当前版本：密码通过环境变量管理，此接口返回提示
    return ResponseEntity.ok(Map.of(
        "message", "请通过修改环境变量 ADMIN_PASSWORD 并重启服务来更改密码",
        "hint", "生产环境建议使用 Kubernetes Secret 或 Docker Swarm Secret 管理凭证"
    ));
  }

  private boolean constantTimeEquals(String actual, String expected) {
    if (actual == null || expected == null) return false;
    return MessageDigest.isEqual(
        actual.getBytes(StandardCharsets.UTF_8),
        expected.getBytes(StandardCharsets.UTF_8));
  }
}
