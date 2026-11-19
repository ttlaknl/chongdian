package com.charging.cloud;

import static org.junit.jupiter.api.Assertions.*;
import java.time.Instant;
import java.util.List;
import java.util.UUID;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.condition.EnabledIfEnvironmentVariable;
import org.postgresql.ds.PGSimpleDataSource;
import org.springframework.beans.factory.support.StaticListableBeanFactory;
import org.springframework.beans.factory.ObjectProvider;
import org.springframework.jdbc.core.JdbcTemplate;
import org.springframework.jdbc.datasource.DataSourceTransactionManager;
import org.springframework.transaction.PlatformTransactionManager;
import org.testcontainers.containers.PostgreSQLContainer;

@EnabledIfEnvironmentVariable(named = "RUN_POSTGRES_TESTS", matches = "true")
class PostgresPersistenceTest {
  @Test void migratesAndRestoresConfigurationEventsWhitelistAndHeartbeat() {
    String externalUrl = System.getenv("TEST_POSTGRES_JDBC_URL");
    PostgreSQLContainer<?> postgres = null;
    try {
      PGSimpleDataSource dataSource = new PGSimpleDataSource();
      if (externalUrl == null || externalUrl.isBlank()) {
        postgres = new PostgreSQLContainer<>("postgres:16-alpine");
        postgres.start();
        dataSource.setURL(postgres.getJdbcUrl());
        dataSource.setUser(postgres.getUsername());
        dataSource.setPassword(postgres.getPassword());
      } else {
        dataSource.setURL(externalUrl);
        dataSource.setUser(System.getenv("TEST_POSTGRES_USER"));
        dataSource.setPassword(System.getenv("TEST_POSTGRES_PASSWORD"));
      }
      org.flywaydb.core.Flyway flyway = org.flywaydb.core.Flyway.configure().dataSource(dataSource)
          .locations("classpath:db/migration").load();
      flyway.migrate();
      flyway.migrate();
      JdbcTemplate jdbc = new JdbcTemplate(dataSource);
      PlatformTransactionManager transactionManager = new DataSourceTransactionManager(dataSource);

      CloudStore first = store(jdbc, transactionManager);
      first.init();
      Instant now = Instant.now();
      WhitelistRecord whitelist = new WhitelistRecord("粤A12345", "ANY", "test", now, null, true);
      PlatformConfiguration cfg = first.configuration();
      first.updateConfiguration(new PlatformConfiguration(cfg.version(), now, cfg.rules(), cfg.bindings(), List.of(whitelist)));
      UUID eventId = UUID.randomUUID();
      first.add(new ViolationEvent(eventId, "A-01:1", "A-01", "粤A12345", "FUEL", "FUEL_VEHICLE", now, null, EventStatus.ALARMING, null, now));
      first.heartbeat("edge-01");
      first.audit("ADMIN", "TEST", "test", "postgres persistence");
      first.saveCamera("default", new CameraConfig(
          "CAM-01", "A01", "default", "A-01", "edge-01", "RTSP",
          "rtsp://admin:pass@192.168.1.10/live", "", "1920x1080", "H.264",
          "REGISTERED", null, "[0.1,0.1,0.9,0.9]", true));
      first.updateSpaceStatuses("edge-01", List.of(new SpaceRuntimeStatus(
          "edge-01", "default", "A-01", "OCCUPIED", "IDLE", "粤A12345", "FUEL",
          true, eventId, now, false, "RTSP_VISION_AI", "", 95.0)));
      first.addRecognitionRecords("edge-01", List.of(new PlateRecognitionRecord(
          "rec-pg-1", "default", "A-01", "CAM-01", "粤A12345", "BLUE", "FUEL",
          95.0, "", "ENTER", "VIOLATION_ALARMING", false, now)));

      CloudStore restored = store(jdbc, transactionManager);
      restored.init();
      assertTrue(restored.isWhitelisted("粤a12345"));
      assertEquals(eventId, restored.events().getFirst().eventId());
      assertTrue(restored.heartbeats().containsKey("edge-01"));
      assertEquals(1, restored.getCamerasForSite("default").size());
      assertEquals("[0.1,0.1,0.9,0.9]", restored.getCamerasForSite("default").getFirst().getRoiJson());
      assertEquals(1, restored.spaceStatuses().size());
      assertEquals(1, restored.getRecognitionRecords("default", 10).size());
      assertEquals(1, jdbc.queryForObject("SELECT COUNT(*) FROM chg_operation_audit", Integer.class));
    } finally {
      if (postgres != null) postgres.stop();
    }
  }

  private CloudStore store(JdbcTemplate jdbc, PlatformTransactionManager transactionManager) {
    return new CloudStore("target/postgres-test", "postgres", provider(jdbc), provider(transactionManager));
  }

  private <T> ObjectProvider<T> provider(T instance) {
    StaticListableBeanFactory factory = new StaticListableBeanFactory();
    factory.addBean("value", instance);
    return factory.getBeanProvider((Class<T>) instance.getClass());
  }
}
