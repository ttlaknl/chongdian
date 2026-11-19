package com.charging.cloud;

import java.nio.charset.StandardCharsets;
import java.security.MessageDigest;
import java.security.SecureRandom;
import java.time.Instant;
import java.util.Base64;
import java.util.HexFormat;
import javax.crypto.Mac;
import javax.crypto.spec.SecretKeySpec;
import org.slf4j.Logger;
import org.slf4j.LoggerFactory;
import org.springframework.beans.factory.annotation.Value;
import org.springframework.stereotype.Service;

/**
 * 轻量级 HMAC 签名 Token 管理服务。
 * 解决 Spring Boot 容器重启或热部署导致内存 Session 丢失、用户前端产生 401 鉴权失效的问题。
 */
@Service
public class TokenService {
  private static final Logger log = LoggerFactory.getLogger(TokenService.class);
  private final byte[] secretKeyBytes;

  public TokenService(
      @Value("${app.security.token-secret:}") String tokenSecret,
      @Value("${app.security.edge-api-key:}") String edgeApiKey,
      @Value("${app.security.admin-password:}") String adminPassword) {
    String key = tokenSecret == null ? "" : tokenSecret.trim();
    String edge = edgeApiKey == null ? "" : edgeApiKey.trim();
    String admin = adminPassword == null ? "" : adminPassword.trim();

    if (key.isBlank()) {
      if (!edge.isBlank() || !admin.isBlank()) {
        key = edge + ":" + admin;
      } else {
        byte[] randomBytes = new byte[32];
        new SecureRandom().nextBytes(randomBytes);
        key = HexFormat.of().formatHex(randomBytes);
        log.warn("Token secret and security keys are unconfigured; using ephemeral per-process random token signing key.");
      }
    }
    this.secretKeyBytes = key.getBytes(StandardCharsets.UTF_8);
  }

  public record AuthUser(String username, String role) {}

  public String generateToken(String username, String role, long ttlSeconds) {
    long exp = Instant.now().getEpochSecond() + ttlSeconds;
    String payload = username + ":" + role + ":" + exp;
    String sig = hmacSha256(payload);
    String raw = payload + ":" + sig;
    return Base64.getUrlEncoder().withoutPadding().encodeToString(raw.getBytes(StandardCharsets.UTF_8));
  }

  public AuthUser validateToken(String token) {
    if (token == null || token.isBlank()) return null;
    try {
      String raw = new String(Base64.getUrlDecoder().decode(token), StandardCharsets.UTF_8);
      String[] parts = raw.split(":");
      if (parts.length != 4) return null;
      String username = parts[0];
      String role = parts[1];
      long exp = Long.parseLong(parts[2]);
      String sig = parts[3];
      if (Instant.now().getEpochSecond() > exp) return null;
      String expectedSig = hmacSha256(username + ":" + role + ":" + exp);
      if (!MessageDigest.isEqual(sig.getBytes(StandardCharsets.UTF_8), expectedSig.getBytes(StandardCharsets.UTF_8))) {
        return null;
      }
      return new AuthUser(username, role);
    } catch (Exception e) {
      return null;
    }
  }

  private String hmacSha256(String data) {
    try {
      Mac mac = Mac.getInstance("HmacSHA256");
      mac.init(new SecretKeySpec(secretKeyBytes, "HmacSHA256"));
      return HexFormat.of().formatHex(mac.doFinal(data.getBytes(StandardCharsets.UTF_8)));
    } catch (Exception e) {
      throw new RuntimeException("HMAC computation failed", e);
    }
  }
}
