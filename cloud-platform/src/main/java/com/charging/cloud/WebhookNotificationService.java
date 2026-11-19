package com.charging.cloud;

import com.fasterxml.jackson.databind.ObjectMapper;
import java.net.URI;
import java.net.http.HttpClient;
import java.net.http.HttpRequest;
import java.net.http.HttpResponse;
import java.nio.charset.StandardCharsets;
import java.time.Duration;
import java.time.Instant;
import java.util.List;
import java.util.Map;
import java.util.UUID;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CopyOnWriteArrayList;
import javax.crypto.Mac;
import javax.crypto.spec.SecretKeySpec;
import org.slf4j.Logger;
import org.slf4j.LoggerFactory;
import org.springframework.beans.factory.ObjectProvider;
import org.springframework.beans.factory.annotation.Autowired;
import org.springframework.beans.factory.annotation.Value;
import org.springframework.jdbc.core.JdbcTemplate;
import org.springframework.stereotype.Service;

@Service
public class WebhookNotificationService {
  private static final Logger log = LoggerFactory.getLogger(WebhookNotificationService.class);

  public enum WebhookChannel { DINGTALK, FEISHU, GENERIC }

  public record NotificationRecord(
      UUID recordId,
      UUID eventId,
      WebhookChannel channel,
      String targetUrl,
      boolean success,
      int attempts,
      String lastError,
      Instant sentAt,
      boolean isSimulated,
      String dataSource
  ) {}

  private final String webhookUrl;
  private final String webhookSecret;
  private final WebhookChannel channel;
  private final boolean simulated;
  private final HttpClient httpClient;
  private final ObjectMapper objectMapper;
  private final JdbcTemplate jdbc;
  private final boolean databaseMode;
  private final List<NotificationRecord> deliveryLedger = new CopyOnWriteArrayList<>();

  @Autowired
  public WebhookNotificationService(
      @Value("${app.webhook.url:}") String webhookUrl,
      @Value("${app.webhook.secret:}") String webhookSecret,
      @Value("${app.webhook.channel:GENERIC}") String channelStr,
      @Value("${app.webhook.simulated:false}") boolean simulated,
      ObjectMapper objectMapper,
      ObjectProvider<JdbcTemplate> jdbcProvider,
      @Value("${app.storage-mode:file}") String storageMode) {
    this.webhookUrl = webhookUrl == null ? "" : webhookUrl.trim();
    this.webhookSecret = webhookSecret == null ? "" : webhookSecret.trim();
    WebhookChannel parsedChannel;
    try {
      parsedChannel = WebhookChannel.valueOf(channelStr.toUpperCase());
    } catch (Exception e) {
      parsedChannel = WebhookChannel.GENERIC;
    }
    this.channel = parsedChannel;
    this.simulated = simulated;
    this.httpClient = HttpClient.newBuilder()
        .connectTimeout(Duration.ofSeconds(5))
        .build();
    this.objectMapper = objectMapper;
    this.jdbc = jdbcProvider != null ? jdbcProvider.getIfAvailable() : null;
    this.databaseMode = "postgres".equalsIgnoreCase(storageMode) && this.jdbc != null;
  }

  public WebhookNotificationService(
      String webhookUrl,
      String webhookSecret,
      String channelStr,
      boolean simulated,
      ObjectMapper objectMapper) {
    this(webhookUrl, webhookSecret, channelStr, simulated, objectMapper, null, "file");
  }

  public static String computeHmacSha256Hex(String secret, String data) {
    try {
      Mac mac = Mac.getInstance("HmacSHA256");
      mac.init(new SecretKeySpec(secret.getBytes(StandardCharsets.UTF_8), "HmacSHA256"));
      byte[] hash = mac.doFinal(data.getBytes(StandardCharsets.UTF_8));
      StringBuilder hex = new StringBuilder(hash.length * 2);
      for (byte b : hash) {
        hex.append(String.format("%02x", b));
      }
      return hex.toString();
    } catch (Exception e) {
      throw new RuntimeException("HMAC-SHA256 failed", e);
    }
  }

  public CompletableFuture<NotificationRecord> sendViolationNotification(ViolationEvent event) {
    return CompletableFuture.supplyAsync(() -> {
      UUID recordId = UUID.randomUUID();
      Instant now = Instant.now();
      String idempotencyKey = event.eventId() + ":" + now.toEpochMilli();

      if (simulated) {
        log.info("[MOCK_SIMULATED] 模拟触发 Webhook 违规告警通知: 事件ID={}, 车位={}, 渠道={}",
            event.eventId(), event.spaceId(), channel);
        NotificationRecord record = new NotificationRecord(
            recordId,
            event.eventId(),
            channel,
            webhookUrl.isBlank() ? "mock://local-webhook" : webhookUrl,
            true,
            1,
            null,
            now,
            true,
            "[MOCK_SIMULATED] 虚拟仿真Webhook中心"
        );
        recordDelivery(record);
        return record;
      }

      if (webhookUrl.isBlank()) {
        log.debug("Webhook URL not configured, skipping notification for event {}", event.eventId());
        NotificationRecord record = new NotificationRecord(
            recordId,
            event.eventId(),
            channel,
            "",
            false,
            0,
            "WEBHOOK_URL_NOT_CONFIGURED",
            now,
            false,
            "WEBHOOK_DISABLED"
        );
        recordDelivery(record);
        return record;
      }

      String payload = buildPayload(event);
      long timestamp = now.toEpochMilli();
      String signature = webhookSecret.isBlank() ? "" : computeHmacSha256Hex(webhookSecret, timestamp + "\n" + payload);

      int attempts = 0;
      String lastError = null;
      boolean success = false;
      int[] backoffMillis = {1000, 2000, 4000};

      for (int i = 0; i < 3; i++) {
        attempts++;
        try {
          HttpRequest.Builder reqBuilder = HttpRequest.newBuilder()
              .uri(URI.create(webhookUrl))
              .timeout(Duration.ofSeconds(5))
              .header("Content-Type", "application/json; charset=utf-8")
              .header("X-Idempotency-Key", idempotencyKey)
              .header("X-Webhook-Timestamp", String.valueOf(timestamp))
              .POST(HttpRequest.BodyPublishers.ofString(payload, StandardCharsets.UTF_8));

          if (!signature.isBlank()) {
            reqBuilder.header("X-Webhook-Signature", signature);
          }

          HttpResponse<String> resp = httpClient.send(reqBuilder.build(), HttpResponse.BodyHandlers.ofString());
          if (resp.statusCode() >= 200 && resp.statusCode() < 300) {
            success = true;
            break;
          } else {
            lastError = "HTTP " + resp.statusCode() + ": " + resp.body();
          }
        } catch (Exception e) {
          lastError = e.getMessage();
        }

        if (i < 2) {
          try {
            Thread.sleep(backoffMillis[i]);
          } catch (InterruptedException ignored) {
            Thread.currentThread().interrupt();
            break;
          }
        }
      }

      NotificationRecord record = new NotificationRecord(
          recordId,
          event.eventId(),
          channel,
          webhookUrl,
          success,
          attempts,
          lastError,
          now,
          false,
          "[REAL_WEBHOOK] 生产公网Webhook"
      );
      recordDelivery(record);
      return record;
    });
  }

  private void recordDelivery(NotificationRecord record) {
    deliveryLedger.add(record);
    if (databaseMode) {
      try {
        jdbc.update(
            "INSERT INTO chg_webhook_deliveries(record_id,event_id,channel,target_url,success,attempts,last_error,sent_at,is_simulated,data_source) " +
            "VALUES(?,?,?,?,?,?,?,?,?,?) ON CONFLICT(record_id) DO NOTHING",
            record.recordId().toString(),
            record.eventId() != null ? record.eventId().toString() : "",
            record.channel().name(),
            record.targetUrl() != null ? record.targetUrl() : "",
            record.success(),
            record.attempts(),
            record.lastError(),
            java.sql.Timestamp.from(record.sentAt()),
            record.isSimulated(),
            record.dataSource() != null ? record.dataSource() : ""
        );
      } catch (Exception e) {
        log.warn("Persist webhook delivery failed: {}", e.getMessage());
      }
    }
  }

  private String buildPayload(ViolationEvent event) {
    try {
      return switch (channel) {
        case DINGTALK -> objectMapper.writeValueAsString(Map.of(
            "msgtype", "markdown",
            "markdown", Map.of(
                "title", "🚨 充电车位违规告警",
                "text", String.format(
                    "### 🚨 充电车位违规占位告警\n- **车位编号**: %s\n- **车牌号码**: %s\n- **违规类型**: %s\n- **发生时间**: %s\n\n> [查看详情与证据](%s)",
                    event.spaceId(),
                    event.plate() == null ? "未识别" : event.plate(),
                    event.violationType(),
                    event.occurredAt(),
                    event.evidenceUrl() == null ? "#" : event.evidenceUrl()
                )
            )
        ));
        case FEISHU -> objectMapper.writeValueAsString(Map.of(
            "msg_type", "interactive",
            "card", Map.of(
                "header", Map.of("title", Map.of("tag", "plain_text", "content", "🚨 充电车位违规告警")),
                "elements", List.of(Map.of(
                    "tag", "div",
                    "text", Map.of("tag", "lark_md", "content", String.format(
                        "**车位**: %s\n**车牌**: %s\n**违规类型**: %s\n**时间**: %s",
                        event.spaceId(), event.plate() == null ? "未知" : event.plate(), event.violationType(), event.occurredAt()
                    ))
                ))
            )
        ));
        case GENERIC -> objectMapper.writeValueAsString(Map.of(
            "eventId", event.eventId().toString(),
            "spaceId", event.spaceId(),
            "plate", event.plate() == null ? "" : event.plate(),
            "vehicleType", event.vehicleType(),
            "violationType", event.violationType(),
            "status", event.status().name(),
            "occurredAt", event.occurredAt().toString(),
            "evidenceUrl", event.evidenceUrl() == null ? "" : event.evidenceUrl()
        ));
      };
    } catch (Exception e) {
      return "{}";
    }
  }

  public List<NotificationRecord> getDeliveryLedger() {
    return List.copyOf(deliveryLedger);
  }
}
