package com.charging.cloud;

import com.fasterxml.jackson.databind.ObjectMapper;
import com.sun.net.httpserver.HttpExchange;
import com.sun.net.httpserver.HttpHandler;
import com.sun.net.httpserver.HttpServer;
import org.junit.jupiter.api.AfterEach;
import org.junit.jupiter.api.BeforeEach;
import org.junit.jupiter.api.Test;

import java.io.IOException;
import java.io.InputStream;
import java.net.InetSocketAddress;
import java.nio.charset.StandardCharsets;
import java.time.Instant;
import java.util.UUID;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicReference;

import static org.junit.jupiter.api.Assertions.*;

class WebhookNotificationServiceTest {
  private HttpServer testServer;
  private int serverPort;
  private final ObjectMapper objectMapper = new ObjectMapper();

  @BeforeEach
  void setupServer() throws IOException {
    testServer = HttpServer.create(new InetSocketAddress("127.0.0.1", 0), 0);
    serverPort = testServer.getAddress().getPort();
    testServer.start();
  }

  @AfterEach
  void tearDownServer() {
    if (testServer != null) {
      testServer.stop(0);
    }
  }

  @Test
  void testHmacSha256Hex() {
    String secret = "test-secret-key-12345";
    String data = "timestamp:123456789\npayload:{\"test\":true}";
    String hash1 = WebhookNotificationService.computeHmacSha256Hex(secret, data);
    String hash2 = WebhookNotificationService.computeHmacSha256Hex(secret, data);

    assertNotNull(hash1);
    assertEquals(64, hash1.length(), "HMAC-SHA256 hex string should be 64 characters");
    assertEquals(hash1, hash2, "HMAC-SHA256 must be deterministic");
  }

  @Test
  void testSimulatedNotificationWhenSimulatedFlagEnabled() {
    WebhookNotificationService service = new WebhookNotificationService(
        "", "", "GENERIC", true, objectMapper
    );

    ViolationEvent event = new ViolationEvent(
        UUID.randomUUID(),
        "session-1",
        "A-01",
        "粤B12345",
        "FUEL",
        "FUEL_VEHICLE",
        Instant.now(),
        "https://oss.example.com/evidence/1.jpg",
        EventStatus.ALARMING,
        null,
        Instant.now()
    );

    WebhookNotificationService.NotificationRecord record = service.sendViolationNotification(event).join();

    assertNotNull(record);
    assertTrue(record.isSimulated(), "Explicit simulated flag must mark simulated mode");
    assertTrue(record.dataSource().contains("[MOCK_SIMULATED]"));
    assertTrue(record.success());
    assertEquals(1, record.attempts());
    assertEquals(1, service.getDeliveryLedger().size());
  }

  @Test
  void testBlankUrlWithoutSimulatedFlagDoesNotFakeSuccess() {
    WebhookNotificationService service = new WebhookNotificationService(
        "", "", "GENERIC", false, objectMapper
    );

    ViolationEvent event = new ViolationEvent(
        UUID.randomUUID(),
        "session-2",
        "A-01",
        "粤B12345",
        "FUEL",
        "FUEL_VEHICLE",
        Instant.now(),
        null,
        EventStatus.ALARMING,
        null,
        Instant.now()
    );

    WebhookNotificationService.NotificationRecord record = service.sendViolationNotification(event).join();

    assertNotNull(record);
    assertFalse(record.isSimulated());
    assertFalse(record.success());
    assertEquals(0, record.attempts());
    assertEquals("WEBHOOK_DISABLED", record.dataSource());
    assertEquals("WEBHOOK_URL_NOT_CONFIGURED", record.lastError());
  }

  @Test
  void testRealHttpWebhookDeliveryWithSignature() {
    AtomicReference<String> receivedSignature = new AtomicReference<>();
    AtomicReference<String> receivedIdempotency = new AtomicReference<>();
    AtomicReference<String> receivedBody = new AtomicReference<>();

    testServer.createContext("/webhook/test", new HttpHandler() {
      @Override
      public void handle(HttpExchange exchange) throws IOException {
        receivedSignature.set(exchange.getRequestHeaders().getFirst("X-Webhook-Signature"));
        receivedIdempotency.set(exchange.getRequestHeaders().getFirst("X-Idempotency-Key"));
        try (InputStream is = exchange.getRequestBody()) {
          receivedBody.set(new String(is.readAllBytes(), StandardCharsets.UTF_8));
        }
        byte[] response = "{\"ok\":true}".getBytes(StandardCharsets.UTF_8);
        exchange.getResponseHeaders().set("Content-Type", "application/json");
        exchange.sendResponseHeaders(200, response.length);
        exchange.getResponseBody().write(response);
        exchange.close();
      }
    });

    String targetUrl = "http://127.0.0.1:" + serverPort + "/webhook/test";
    String secret = "my-webhook-secret-999";

    WebhookNotificationService service = new WebhookNotificationService(
        targetUrl, secret, "GENERIC", false, objectMapper
    );

    UUID eventId = UUID.randomUUID();
    ViolationEvent event = new ViolationEvent(
        eventId,
        "session-test-real",
        "A-02",
        "粤B99999",
        "FUEL",
        "FUEL_VEHICLE",
        Instant.now(),
        "http://oss/real.jpg",
        EventStatus.ALARMING,
        null,
        Instant.now()
    );

    WebhookNotificationService.NotificationRecord record = service.sendViolationNotification(event).join();

    assertNotNull(record);
    assertFalse(record.isSimulated(), "Real HTTP endpoint should not be flagged simulated");
    assertTrue(record.dataSource().contains("[REAL_WEBHOOK]"));
    assertTrue(record.success());
    assertEquals(1, record.attempts());

    assertNotNull(receivedSignature.get(), "X-Webhook-Signature header must be present");
    assertEquals(64, receivedSignature.get().length());
    assertNotNull(receivedIdempotency.get(), "X-Idempotency-Key header must be present");
    assertTrue(receivedIdempotency.get().startsWith(eventId.toString()));
    assertNotNull(receivedBody.get());
    assertTrue(receivedBody.get().contains("A-02"));
    assertTrue(receivedBody.get().contains("粤B99999"));
  }

  @Test
  void testDingTalkAndFeishuFormatFormatting() {
    AtomicReference<String> dingtalkPayload = new AtomicReference<>();
    AtomicReference<String> feishuPayload = new AtomicReference<>();

    testServer.createContext("/webhook/dingtalk", exchange -> {
      try (InputStream is = exchange.getRequestBody()) {
        dingtalkPayload.set(new String(is.readAllBytes(), StandardCharsets.UTF_8));
      }
      exchange.sendResponseHeaders(200, 0);
      exchange.close();
    });

    testServer.createContext("/webhook/feishu", exchange -> {
      try (InputStream is = exchange.getRequestBody()) {
        feishuPayload.set(new String(is.readAllBytes(), StandardCharsets.UTF_8));
      }
      exchange.sendResponseHeaders(200, 0);
      exchange.close();
    });

    ViolationEvent event = new ViolationEvent(
        UUID.randomUUID(),
        "session-fmt",
        "B-03",
        "粤B66666",
        "FUEL",
        "FUEL_VEHICLE",
        Instant.now(),
        "http://oss/fmt.jpg",
        EventStatus.ALARMING,
        null,
        Instant.now()
    );

    // DingTalk
    WebhookNotificationService dingtalkService = new WebhookNotificationService(
        "http://127.0.0.1:" + serverPort + "/webhook/dingtalk", "", "DINGTALK", false, objectMapper
    );
    dingtalkService.sendViolationNotification(event).join();
    assertNotNull(dingtalkPayload.get());
    assertTrue(dingtalkPayload.get().contains("msgtype"));
    assertTrue(dingtalkPayload.get().contains("markdown"));
    assertTrue(dingtalkPayload.get().contains("B-03"));

    // Feishu
    WebhookNotificationService feishuService = new WebhookNotificationService(
        "http://127.0.0.1:" + serverPort + "/webhook/feishu", "", "FEISHU", false, objectMapper
    );
    feishuService.sendViolationNotification(event).join();
    assertNotNull(feishuPayload.get());
    assertTrue(feishuPayload.get().contains("msg_type"));
    assertTrue(feishuPayload.get().contains("interactive"));
    assertTrue(feishuPayload.get().contains("card"));
  }
}
