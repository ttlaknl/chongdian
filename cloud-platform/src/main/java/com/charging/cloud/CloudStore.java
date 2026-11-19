package com.charging.cloud;

import com.fasterxml.jackson.core.type.TypeReference;
import com.fasterxml.jackson.databind.DeserializationFeature;
import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.ObjectMapper;
import com.fasterxml.jackson.databind.SerializationFeature;
import com.fasterxml.jackson.datatype.jsr310.JavaTimeModule;
import jakarta.annotation.PostConstruct;
import java.io.File;
import java.io.IOException;
import java.net.InetSocketAddress;
import java.net.Socket;
import java.net.URLEncoder;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.StandardCopyOption;
import java.nio.file.StandardOpenOption;
import java.security.MessageDigest;
import java.sql.Timestamp;
import java.time.Instant;
import java.time.temporal.ChronoUnit;
import java.util.*;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.CopyOnWriteArrayList;
import java.util.regex.Matcher;
import java.util.regex.Pattern;
import java.util.stream.Collectors;
import javax.crypto.Mac;
import javax.crypto.spec.SecretKeySpec;
import org.springframework.beans.factory.ObjectProvider;
import org.springframework.beans.factory.annotation.Autowired;
import org.springframework.beans.factory.annotation.Value;
import org.springframework.jdbc.core.JdbcTemplate;
import org.springframework.stereotype.Service;
import org.springframework.transaction.PlatformTransactionManager;
import org.springframework.transaction.support.TransactionTemplate;
import org.springframework.web.servlet.mvc.method.annotation.SseEmitter;

@Service
public class CloudStore {
  private static final int MAX_RECOGNITION_FEED_MEMORY = 2000;

  private volatile PlatformConfiguration configuration = PlatformConfiguration.defaults();
  private final Map<UUID, ViolationEvent> events = new ConcurrentHashMap<>();
  private final Map<String, Instant> heartbeats = new ConcurrentHashMap<>();
  private final Map<String, SpaceRuntimeStatus> spaceStatuses = new ConcurrentHashMap<>();
  private final Map<String, ConfigurationApplyReport> configurationReports = new ConcurrentHashMap<>();
  private final List<OperationAuditRecord> audits = new CopyOnWriteArrayList<>();
  private final List<SseEmitter> spaceEmitters = new CopyOnWriteArrayList<>();
  private final List<SseEmitter> eventEmitters = new CopyOnWriteArrayList<>();
  private final java.util.concurrent.atomic.AtomicReference<UpgradeManifest> latestManifest =
      new java.util.concurrent.atomic.AtomicReference<>();
  private final Map<String, UpgradeNodeReport> nodeUpgradeReports = new ConcurrentHashMap<>();
  private final Map<String, UpgradeManifest> pendingNodeUpgrades = new ConcurrentHashMap<>();
  private final String upgradeSecret;
  private final long evidenceMaxBytes;

  // 多停车场支持
  private final Map<String, Site> sitesCache = new ConcurrentHashMap<>();
  private final Map<String, PlatformConfiguration> configurationBySite = new ConcurrentHashMap<>();
  private final Map<String, String> nodeToSite = new ConcurrentHashMap<>(); // nodeId -> siteId

  // 多终端管理与硬件遥测、摄像头配置与车牌识别流
  private final Map<String, EdgeNodeTelemetry> edgeNodesTelemetry = new ConcurrentHashMap<>();
  private final Map<String, List<CameraConfig>> camerasBySite = new ConcurrentHashMap<>();
  private final List<PlateRecognitionRecord> recognitionFeed = new CopyOnWriteArrayList<>();
  private final Map<String, ChargerProtocol> protocolsCache = new ConcurrentHashMap<>();

  private final ObjectMapper mapper = new ObjectMapper()
      .registerModule(new JavaTimeModule())
      .disable(SerializationFeature.WRITE_DATES_AS_TIMESTAMPS)
      .disable(DeserializationFeature.FAIL_ON_UNKNOWN_PROPERTIES);

  private final File storageDir;
  private final JdbcTemplate jdbc;
  private final boolean databaseMode;
  private final TransactionTemplate transaction;
  private final WebhookNotificationService webhookService;

  @Autowired
  public CloudStore(
      @Value("${app.storage-dir}") String storageDirectory,
      @Value("${app.storage-mode:file}") String storageMode,
      @Value("${app.upgrade.secret:}") String upgradeSecret,
      @Value("${app.evidence.max-bytes:10737418240}") long evidenceMaxBytes,
      ObjectProvider<JdbcTemplate> jdbcProvider,
      ObjectProvider<PlatformTransactionManager> transactionManagerProvider,
      ObjectProvider<WebhookNotificationService> webhookServiceProvider) {
    this.storageDir = new File(storageDirectory);
    this.upgradeSecret = upgradeSecret == null ? "" : upgradeSecret.trim();
    this.evidenceMaxBytes = evidenceMaxBytes > 0 ? evidenceMaxBytes : 10L * 1024 * 1024 * 1024;
    this.jdbc = jdbcProvider != null ? jdbcProvider.getIfAvailable() : null;
    this.databaseMode = "postgres".equalsIgnoreCase(storageMode);
    PlatformTransactionManager transactionManager =
        transactionManagerProvider != null ? transactionManagerProvider.getIfAvailable() : null;
    this.transaction = transactionManager == null ? null : new TransactionTemplate(transactionManager);
    this.webhookService = webhookServiceProvider != null ? webhookServiceProvider.getIfAvailable() : null;
    if (databaseMode && (jdbc == null || transaction == null)) {
      throw new IllegalStateException("postgres storage mode requires JDBC and a transaction manager");
    }
  }

  public CloudStore(
      String storageDirectory,
      String storageMode,
      ObjectProvider<JdbcTemplate> jdbcProvider,
      ObjectProvider<PlatformTransactionManager> transactionManagerProvider) {
    this(storageDirectory, storageMode, "", 10L * 1024 * 1024 * 1024,
        jdbcProvider, transactionManagerProvider, null);
  }

  public CloudStore(String storageDirectory) {
    this(storageDirectory, null);
  }

  public CloudStore(String storageDirectory, WebhookNotificationService webhookService) {
    this.storageDir = new File(storageDirectory);
    this.upgradeSecret = "";
    this.evidenceMaxBytes = 10L * 1024 * 1024 * 1024;
    this.jdbc = null;
    this.databaseMode = false;
    this.transaction = null;
    this.webhookService = webhookService;
  }

  @PostConstruct
  public void init() {
    if (databaseMode) {
      loadFromDatabase();
      if (jdbc.queryForObject(
          "SELECT COUNT(*) FROM chg_platform_configuration WHERE site_id IS NULL OR site_id='default'",
          Long.class) == 0) {
        persistConfiguration(configuration);
      }
    } else {
      loadFromDisk();
    }
    loadUpgradeManifest();
  }

  public PlatformConfiguration configuration() {
    return configuration.withCameras(getCamerasForSite("default"));
  }

  public RuleConfig rules() {
    return configuration.rules();
  }

  public synchronized PlatformConfiguration updateConfiguration(PlatformConfiguration incoming) {
    PlatformConfiguration next = new PlatformConfiguration(
        configuration.version() + 1,
        Instant.now(),
        incoming.rules(),
        List.copyOf(incoming.bindings()),
        List.copyOf(incoming.whitelist()),
        getCamerasForSite("default"));
    persistConfiguration(next);
    configuration = next;
    return configuration;
  }

  public synchronized RuleConfig updateRules(RuleConfig rules) {
    updateConfiguration(new PlatformConfiguration(
        configuration.version(),
        Instant.now(),
        rules,
        configuration.bindings(),
        configuration.whitelist(),
        configuration.cameras()));
    return configuration.rules();
  }

  public synchronized ViolationEvent add(ViolationEvent e) {
    ViolationEvent existing = events.values().stream()
        .filter(item -> e.sessionId() != null && !e.sessionId().isBlank()
            && e.siteId().equals(item.siteId()) && e.sessionId().equals(item.sessionId()))
        .findFirst().orElse(null);
    if (existing != null && existing.updatedAt() != null && e.updatedAt() != null
        && existing.updatedAt().isAfter(e.updatedAt())) {
      return existing;
    }
    UUID id = existing == null ? e.eventId() : existing.eventId();
    ViolationEvent stored = new ViolationEvent(
        id,
        e.sessionId() == null ? "" : e.sessionId(),
        e.siteId(),
        e.spaceId(),
        e.plate(),
        e.vehicleType(),
        e.violationType(),
        e.occurredAt() == null ? Instant.now() : e.occurredAt(),
        e.evidenceUrl(),
        e.status() == null ? EventStatus.ALARMING : e.status(),
        e.clearedAt(),
        e.updatedAt() == null ? Instant.now() : e.updatedAt(),
        e.acknowledgedBy() != null ? e.acknowledgedBy() : (existing != null ? existing.acknowledgedBy() : null),
        e.acknowledgedAt() != null ? e.acknowledgedAt() : (existing != null ? existing.acknowledgedAt() : null));
    ViolationEvent previous = events.put(id, stored);
    try {
      persistEvent(stored);
    } catch (RuntimeException error) {
      if (previous == null) events.remove(id); else events.put(id, previous);
      throw error;
    }
    broadcastEvent(stored);
    boolean enteredAlarm = stored.status() == EventStatus.ALARMING
        && (previous == null || previous.status() != EventStatus.ALARMING);
    if (webhookService != null && enteredAlarm) {
      webhookService.sendViolationNotification(stored);
    }
    return stored;
  }

  public List<ViolationEvent> events() {
    return events.values().stream()
        .sorted(Comparator.comparing(ViolationEvent::occurredAt).reversed())
        .toList();
  }

  public List<ViolationEvent> filterEvents(String spaceId, String plate, String violationType) {
    return filterEvents(null, spaceId, plate, violationType);
  }

  public List<ViolationEvent> filterEvents(
      String siteId, String spaceId, String plate, String violationType) {
    return events.values().stream()
        .filter(e -> siteId == null || siteId.isBlank() || siteId.equals(e.siteId()))
        .filter(e -> spaceId == null || spaceId.isBlank() || spaceId.equalsIgnoreCase(e.spaceId()))
        .filter(e -> plate == null || plate.isBlank() || (e.plate() != null && e.plate().contains(plate)))
        .filter(e -> violationType == null || violationType.isBlank() || violationType.equalsIgnoreCase(e.violationType()))
        .sorted(Comparator.comparing(ViolationEvent::occurredAt).reversed())
        .toList();
  }

  public synchronized void clearEvents(String siteId) {
    if (siteId != null && !siteId.isBlank() && !"all".equalsIgnoreCase(siteId)) {
      events.entrySet().removeIf(entry -> siteId.equalsIgnoreCase(entry.getValue().siteId()));
      if (databaseMode) {
        jdbc.update("DELETE FROM chg_violation_events WHERE site_id = ?", siteId);
      }
    } else {
      events.clear();
      if (databaseMode) {
        jdbc.update("DELETE FROM chg_violation_events");
      }
    }
    if (!databaseMode) {
      persistEvents();
    }
    broadcastEventsList();
  }

  public synchronized boolean deleteEvent(UUID eventId) {
    if (eventId == null) return false;
    ViolationEvent removed = events.remove(eventId);
    if (removed == null) return false;
    if (databaseMode) {
      jdbc.update("DELETE FROM chg_violation_events WHERE event_id = ?", eventId.toString());
    } else {
      persistEvents();
    }
    broadcastEventsList();
    return true;
  }

  public String exportEventsCsv(String spaceId, String plate, String violationType) {
    return exportEventsCsv(null, spaceId, plate, violationType);
  }

  public String exportEventsCsv(String siteId, String spaceId, String plate, String violationType) {
    List<ViolationEvent> list = filterEvents(siteId, spaceId, plate, violationType);
    StringBuilder sb = new StringBuilder();
    sb.append('\uFEFF');
    sb.append("事件ID,车位编号,车牌号码,车辆类型,违规类型,事件状态,发生时间,清除时间,确认人,证据图片链接\n");
    for (ViolationEvent ev : list) {
      sb.append(escapeCsv(ev.eventId() == null ? "" : ev.eventId().toString())).append(",");
      sb.append(escapeCsv(ev.spaceId())).append(",");
      sb.append(escapeCsv(ev.plate())).append(",");
      sb.append(escapeCsv(mapVehicleType(ev.vehicleType()))).append(",");
      sb.append(escapeCsv(mapViolationType(ev.violationType()))).append(",");
      sb.append(escapeCsv(ev.status() == null ? "ALARMING" : ev.status().name())).append(",");
      sb.append(escapeCsv(ev.occurredAt() == null ? "" : ev.occurredAt().toString())).append(",");
      sb.append(escapeCsv(ev.clearedAt() == null ? "" : ev.clearedAt().toString())).append(",");
      sb.append(escapeCsv(ev.acknowledgedBy())).append(",");
      sb.append(escapeCsv(ev.evidenceUrl())).append("\n");
    }
    return sb.toString();
  }

  private String mapVehicleType(String type) {
    if (type == null) return "未知";
    return switch (type) {
      case "FUEL" -> "燃油车";
      case "NEW_ENERGY" -> "新能源车";
      case "NON_MOTOR" -> "非机动车/两轮车";
      case "OBSTACLE" -> "杂物/桌椅障碍物";
      case "SPECIAL_VEHICLE" -> "特种车辆(军/警)";
      case "HEAVY_VEHICLE" -> "大客货车";
      default -> type;
    };
  }

  private String mapViolationType(String type) {
    if (type == null) return "未知违规";
    return switch (type) {
      case "FUEL_VEHICLE" -> "燃油车占用充电位";
      case "NON_MOTOR_VEHICLE" -> "两轮车/非机动车占用充电位";
      case "OBSTACLE_OCCUPATION" -> "杂物桌椅占用充电位";
      case "HEAVY_VEHICLE" -> "大客货车占用充电位";
      case "NOT_CHARGING_OVERTIME" -> "新能源车超时未充电";
      case "CHARGE_COMPLETED_OVERTIME" -> "充电完成超时滞留";
      case "CHARGER_STATUS_UNAVAILABLE" -> "充电桩离线滞留";
      case "CHARGER_FAULT" -> "充电桩故障占位";
      case "UNKNOWN_VEHICLE" -> "未识别车型占位";
      default -> type;
    };
  }

  private String escapeCsv(String val) {
    if (val == null) return "";
    if (val.contains(",") || val.contains("\"") || val.contains("\n")) {
      return "\"" + val.replace("\"", "\"\"") + "\"";
    }
    return val;
  }

  public void heartbeat(String node) {
    heartbeat(node, null, null);
  }

  public void heartbeat(String node, Map<String, Object> telemetry) {
    heartbeat(node, telemetry, null);
  }

  public void heartbeat(String node, Map<String, Object> telemetry, String remoteIp) {
    Instant now = Instant.now();
    double cpu = 0.0, mem = 0.0, temp = 0.0, disk = 0.0;
    long uptime = 0, frames = 0, violations = 0, alarms = 0;
    String version = "";
    String ipFromTelemetry = "";
    if (telemetry != null) {
      if (telemetry.get("cpuUsage") instanceof Number n) cpu = n.doubleValue();
      if (telemetry.get("memoryUsage") instanceof Number n) mem = n.doubleValue();
      if (telemetry.get("temperature") instanceof Number n) temp = n.doubleValue();
      if (telemetry.get("diskWatermark") instanceof Number n) disk = n.doubleValue();
      if (telemetry.get("uptimeSeconds") instanceof Number n) uptime = n.longValue();
      if (telemetry.get("framesProcessed") instanceof Number n) frames = n.longValue();
      if (telemetry.get("violationsDetected") instanceof Number n) violations = n.longValue();
      if (telemetry.get("alarmsTriggered") instanceof Number n) alarms = n.longValue();
      if (telemetry.get("version") instanceof String v) version = v;
      if (telemetry.get("ipAddress") instanceof String ip) ipFromTelemetry = ip;
    }

    String effectiveIp = !ipFromTelemetry.isBlank()
        ? ipFromTelemetry
        : (remoteIp != null && !remoteIp.isBlank() ? remoteIp : "");
    String siteId = siteIdForNode(node);

    if (databaseMode) {
      jdbc.update(
          "INSERT INTO chg_node_heartbeats(node_id,last_heartbeat,ip_address,status,site_id,cpu_usage,memory_usage,temperature,uptime_seconds,frames_processed,violations_detected,alarms_triggered,disk_watermark,version) " +
          "VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?) " +
          "ON CONFLICT(node_id) DO UPDATE SET last_heartbeat=EXCLUDED.last_heartbeat, ip_address=CASE WHEN EXCLUDED.ip_address <> '' THEN EXCLUDED.ip_address ELSE chg_node_heartbeats.ip_address END, status=EXCLUDED.status, site_id=EXCLUDED.site_id, cpu_usage=EXCLUDED.cpu_usage, memory_usage=EXCLUDED.memory_usage, temperature=EXCLUDED.temperature, uptime_seconds=EXCLUDED.uptime_seconds, frames_processed=EXCLUDED.frames_processed, violations_detected=EXCLUDED.violations_detected, alarms_triggered=EXCLUDED.alarms_triggered, disk_watermark=EXCLUDED.disk_watermark, version=EXCLUDED.version",
          node, Timestamp.from(now), effectiveIp, "ONLINE", siteId, cpu, mem, temp, uptime, frames, violations, alarms, disk, version);
    }
    heartbeats.put(node, now);

    boolean shouldPersistNodesDisk = false;
    EdgeNodeTelemetry item = edgeNodesTelemetry.get(node);
    if (item == null) {
      String ip = effectiveIp;
      if (ip.isBlank() && databaseMode && jdbc != null) {
        try {
          List<String> ips = jdbc.query(
              "SELECT ip_address FROM chg_node_site_mapping WHERE node_id = ?",
              (rs, rowNum) -> rs.getString("ip_address"), node);
          if (!ips.isEmpty() && ips.get(0) != null) ip = ips.get(0);
        } catch (Exception ignored) {}
      }
      item = new EdgeNodeTelemetry(
          node, siteId, node, ip,
          cpu, mem, temp, version, "ONLINE", now,
          Collections.emptyList()
      );
      edgeNodesTelemetry.put(node, item);
      nodeToSite.putIfAbsent(node, siteId);
      shouldPersistNodesDisk = true;
    } else if (!effectiveIp.isBlank()) {
      if (!effectiveIp.equals(item.getIpAddress())) {
        item.setIpAddress(effectiveIp);
        shouldPersistNodesDisk = true;
        if (databaseMode && jdbc != null) {
          try {
            jdbc.update("UPDATE chg_node_site_mapping SET ip_address = ? WHERE node_id = ?", effectiveIp, node);
          } catch (Exception ignored) {}
        }
      }
    }

    item.setLastHeartbeat(now);
    if (!"REBOOTING".equals(item.getStatus()) && !"UPGRADING".equals(item.getStatus())) {
      item.setStatus("ONLINE");
    } else if (item.getRebootRequestedAt() == null && "REBOOTING".equals(item.getStatus())) {
      item.setStatus("ONLINE");
    }

    if (telemetry != null) {
      item.setCpuUsage(cpu);
      item.setMemoryUsage(mem);
      item.setTemperature(temp);
      item.setDiskWatermark(disk);
      item.setUptimeSeconds(uptime);
      item.setFramesProcessed(frames);
      item.setViolationsDetected(violations);
      item.setAlarmsTriggered(alarms);
      if (!version.isBlank()) {
        item.setCurrentVersion(version);
      }

      UpgradeNodeReport upReport = nodeUpgradeReports.get(node);
      if (upReport != null && upReport.targetVersion() != null && !version.isBlank()) {
        long currentVerNum = 0;
        try {
          String num = version.replaceAll("[^0-9]", "");
          if (!num.isEmpty()) currentVerNum = Long.parseLong(num);
        } catch (Exception ignored) {}
        if (currentVerNum >= upReport.targetVersion() && !"COMMITTED".equals(upReport.state())) {
          UpgradeNodeReport committedReport = new UpgradeNodeReport(
              node, currentVerNum, upReport.targetVersion(), "COMMITTED",
              "工控机已成功运行在目标版本 " + version, null, now
          );
          saveNodeUpgradeReport(node, committedReport);
          item.setStatus("ONLINE");
        }
      }
    }

    if (!databaseMode && shouldPersistNodesDisk) {
      persistNodesToDisk();
    }
  }

  public Map<String, Instant> heartbeats() {
    return Map.copyOf(heartbeats);
  }

  public synchronized boolean consumePendingReboot(String nodeId) {
    EdgeNodeTelemetry node = edgeNodesTelemetry.get(nodeId);
    if (node == null || node.getRebootRequestedAt() == null) {
      return false;
    }
    node.setRebootRequestedAt(null);
    if ("REBOOTING".equals(node.getStatus())) {
      node.setStatus("ONLINE");
    }
    if (databaseMode) {
      jdbc.update("UPDATE chg_node_site_mapping SET reboot_requested_at = NULL WHERE node_id = ?", nodeId);
    } else {
      persistNodesToDisk();
    }
    return true;
  }

  public synchronized void updateSpaceStatuses(String nodeId, List<SpaceRuntimeStatus> statuses) {
    String siteId = siteIdForNode(nodeId);
    Set<String> incomingKeys = statuses.stream()
        .map(status -> nodeId + ":" + status.spaceId())
        .collect(Collectors.toSet());
    spaceStatuses.keySet().removeIf(key -> key.startsWith(nodeId + ":") && !incomingKeys.contains(key));

    List<SpaceRuntimeStatus> storedList = new ArrayList<>(statuses.size());
    for (SpaceRuntimeStatus status : statuses) {
      SpaceRuntimeStatus scoped = status.withNodeAndSite(nodeId, siteId);
      if ((scoped.snapshotUrl() == null || scoped.snapshotUrl().isBlank())
          && getSpaceSnapshotFile(siteId, scoped.spaceId()) != null) {
        scoped = scoped.withSnapshotUrl(buildSnapshotUrl(siteId, scoped.spaceId()));
      }
      spaceStatuses.put(nodeId + ":" + scoped.spaceId(), scoped);
      storedList.add(scoped);
    }

    if (databaseMode) {
      transaction.executeWithoutResult(tx -> {
        jdbc.update("DELETE FROM chg_space_runtime_status WHERE node_id = ?", nodeId);
        for (SpaceRuntimeStatus st : storedList) {
          jdbc.update(
              "INSERT INTO chg_space_runtime_status(site_id,space_id,node_id,phase,charger_status,plate,vehicle_type,alarming,active_event_id,observed_at,is_simulated,stream_type,snapshot_url,confidence,updated_at) " +
              "VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?) " +
              "ON CONFLICT(site_id,space_id) DO UPDATE SET node_id=EXCLUDED.node_id,phase=EXCLUDED.phase,charger_status=EXCLUDED.charger_status,plate=EXCLUDED.plate,vehicle_type=EXCLUDED.vehicle_type,alarming=EXCLUDED.alarming,active_event_id=EXCLUDED.active_event_id,observed_at=EXCLUDED.observed_at,is_simulated=EXCLUDED.is_simulated,stream_type=EXCLUDED.stream_type,snapshot_url=EXCLUDED.snapshot_url,confidence=EXCLUDED.confidence,updated_at=EXCLUDED.updated_at",
              st.siteId(),
              st.spaceId(),
              st.nodeId(),
              st.phase() != null ? st.phase() : "EMPTY",
              st.chargerStatus() != null ? st.chargerStatus() : "UNKNOWN",
              st.plate(),
              st.vehicleType() != null ? st.vehicleType() : "UNKNOWN",
              st.alarming(),
              st.activeEventId() != null ? st.activeEventId().toString() : null,
              Timestamp.from(st.observedAt() != null ? st.observedAt() : Instant.now()),
              Boolean.TRUE.equals(st.isSimulated()),
              st.streamType() != null ? st.streamType() : "",
              st.snapshotUrl() != null ? st.snapshotUrl() : "",
              st.confidence() != null ? st.confidence() : 0.0,
              Timestamp.from(Instant.now()));
        }
      });
    } else {
      persistSpaceStatusesToDisk();
    }

    broadcastSpaces();
  }

  public List<SpaceRuntimeStatus> spaceStatuses() {
    return spaceStatuses.values().stream()
        .sorted(Comparator.comparing(SpaceRuntimeStatus::siteId)
            .thenComparing(SpaceRuntimeStatus::spaceId))
        .toList();
  }

  public SseEmitter subscribeSpaces() {
    SseEmitter emitter = new SseEmitter(180_000L);
    spaceEmitters.add(emitter);
    emitter.onCompletion(() -> spaceEmitters.remove(emitter));
    emitter.onTimeout(() -> spaceEmitters.remove(emitter));
    emitter.onError(e -> spaceEmitters.remove(emitter));
    try {
      emitter.send(SseEmitter.event().name("spaces").data(spaceStatuses()));
    } catch (Exception e) {
      spaceEmitters.remove(emitter);
    }
    return emitter;
  }

  public SseEmitter subscribeEvents() {
    SseEmitter emitter = new SseEmitter(180_000L);
    eventEmitters.add(emitter);
    emitter.onCompletion(() -> eventEmitters.remove(emitter));
    emitter.onTimeout(() -> eventEmitters.remove(emitter));
    emitter.onError(e -> eventEmitters.remove(emitter));
    try {
      emitter.send(SseEmitter.event().name("events").data(events()));
    } catch (Exception e) {
      eventEmitters.remove(emitter);
    }
    return emitter;
  }

  public void broadcastSpaces() {
    List<SpaceRuntimeStatus> list = spaceStatuses();
    for (SseEmitter emitter : spaceEmitters) {
      try {
        emitter.send(SseEmitter.event().name("spaces").data(list));
      } catch (Exception e) {
        spaceEmitters.remove(emitter);
      }
    }
  }

  public void broadcastEvent(ViolationEvent event) {
    for (SseEmitter emitter : eventEmitters) {
      try {
        emitter.send(SseEmitter.event().name("event").data(event));
      } catch (Exception e) {
        eventEmitters.remove(emitter);
      }
    }
  }

  public void broadcastEventsList() {
    List<ViolationEvent> list = events();
    for (SseEmitter emitter : eventEmitters) {
      try {
        emitter.send(SseEmitter.event().name("events").data(list));
      } catch (Exception e) {
        eventEmitters.remove(emitter);
      }
    }
  }

  public synchronized ViolationEvent acknowledgeEvent(UUID eventId) {
    return acknowledgeEvent(eventId, null);
  }

  public synchronized ViolationEvent acknowledgeEvent(UUID eventId, String actorUsername) {
    ViolationEvent previous = events.get(eventId);
    if (previous == null) return null;
    Instant now = Instant.now();
    ViolationEvent updated = new ViolationEvent(
        previous.eventId(), previous.sessionId(), previous.siteId(), previous.spaceId(),
        previous.plate(), previous.vehicleType(), previous.violationType(),
        previous.occurredAt(), previous.evidenceUrl(), EventStatus.ACKNOWLEDGED,
        previous.clearedAt(), now,
        actorUsername != null && !actorUsername.isBlank() ? actorUsername : previous.acknowledgedBy(),
        now);
    events.put(eventId, updated);
    try {
      persistEvent(updated);
    } catch (RuntimeException error) {
      events.put(eventId, previous);
      throw error;
    }
    broadcastEvent(updated);
    return updated;
  }

  public synchronized ConfigurationApplyReport saveConfigurationReport(
      String nodeId, ConfigurationApplyReport report) {
    ConfigurationApplyReport stored = report.withNodeId(nodeId);
    configurationReports.put(nodeId, stored);
    if (databaseMode) {
      jdbc.update(
          "INSERT INTO chg_configuration_reports(node_id,config_version,status,message,reported_at) VALUES(?,?,?,?,?) " +
          "ON CONFLICT(node_id) DO UPDATE SET config_version=EXCLUDED.config_version,status=EXCLUDED.status,message=EXCLUDED.message,reported_at=EXCLUDED.reported_at",
          stored.nodeId(), stored.version(), stored.status(),
          stored.message() != null ? stored.message() : "",
          Timestamp.from(stored.reportedAt()));
    } else {
      persistMapJson("config_reports.json", configurationReports);
    }
    return stored;
  }

  public Map<String, ConfigurationApplyReport> configurationReports() {
    return Map.copyOf(configurationReports);
  }

  public String calculateManifestSignature(long version, String sha256Hash, String packageUrl) {
    if (upgradeSecret == null || upgradeSecret.isBlank()) {
      return "";
    }
    try {
      String payload = version + ":" + sha256Hash + ":" + packageUrl;
      Mac mac = Mac.getInstance("HmacSHA256");
      mac.init(new SecretKeySpec(upgradeSecret.getBytes(StandardCharsets.UTF_8), "HmacSHA256"));
      return HexFormat.of().formatHex(mac.doFinal(payload.getBytes(StandardCharsets.UTF_8)));
    } catch (Exception e) {
      return "";
    }
  }

  public UpgradeManifest signManifest(UpgradeManifest manifest) {
    if (manifest == null) return null;
    String sig = manifest.signature();
    if (sig == null || sig.isBlank()) {
      sig = calculateManifestSignature(manifest.version(), manifest.sha256Hash(), manifest.packageUrl());
    }
    return new UpgradeManifest(
        manifest.version(),
        manifest.versionName(),
        manifest.packageUrl(),
        manifest.sha256Hash(),
        sig,
        manifest.minCompatibleVersion(),
        manifest.releaseNotes(),
        manifest.createdAt()
    );
  }

  public synchronized UpgradeManifest saveUpgradeManifest(UpgradeManifest manifest) {
    UpgradeManifest signed = signManifest(manifest);
    latestManifest.set(signed);
    if (databaseMode) {
      jdbc.update(
          "INSERT INTO chg_upgrade_manifests(version,version_name,package_url,sha256_hash,signature,min_compatible_version,release_notes,created_at) " +
          "VALUES(?,?,?,?,?,?,?,?) ON CONFLICT(version) DO UPDATE SET version_name=EXCLUDED.version_name,package_url=EXCLUDED.package_url,sha256_hash=EXCLUDED.sha256_hash,signature=EXCLUDED.signature,min_compatible_version=EXCLUDED.min_compatible_version,release_notes=EXCLUDED.release_notes,created_at=EXCLUDED.created_at",
          signed.version(),
          signed.versionName(),
          signed.packageUrl(),
          signed.sha256Hash(),
          signed.signature() != null ? signed.signature() : "",
          signed.minCompatibleVersion(),
          signed.releaseNotes() != null ? signed.releaseNotes() : "",
          Timestamp.from(signed.createdAt()));
    } else {
      persistObjectJson("upgrade_manifest.json", signed);
    }
    return signed;
  }

  public UpgradeManifest getUpgradeManifest() {
    return latestManifest.get();
  }

  public File getFirmwareDirectory() {
    File firmwareDir = new File(storageDir, "firmware");
    if (!firmwareDir.exists()) firmwareDir.mkdirs();
    return firmwareDir;
  }

  public synchronized Map<String, Object> saveFirmwareBinary(String rawFilename, byte[] bytes) {
    if (bytes == null || bytes.length == 0) {
      throw new IllegalArgumentException("firmware binary cannot be empty");
    }
    String safeName = sanitizeFilename(rawFilename, "chongdian-edge-firmware.bin");
    File target = new File(getFirmwareDirectory(), safeName);
    try {
      Path tmp = target.toPath().resolveSibling(safeName + ".tmp");
      Files.write(tmp, bytes);
      moveAtomically(tmp, target.toPath());
      byte[] digest = MessageDigest.getInstance("SHA-256").digest(bytes);
      String sha256 = HexFormat.of().formatHex(digest);
      Map<String, Object> result = new LinkedHashMap<>();
      result.put("filename", safeName);
      result.put("packageUrl", "/api/v1/firmware/" + safeName);
      result.put("sha256Hash", sha256);
      result.put("sizeBytes", bytes.length);
      result.put("signedWithSecret", upgradeSecret != null && !upgradeSecret.isBlank());
      return result;
    } catch (Exception e) {
      throw new IllegalStateException("save firmware binary failed: " + e.getMessage(), e);
    }
  }

  public synchronized UpgradeManifest pollPendingUpgrade(String nodeId) {
    UpgradeManifest manifest = pendingNodeUpgrades.remove(nodeId);
    if (manifest != null) {
      if (databaseMode) {
        jdbc.update("DELETE FROM chg_pending_node_upgrades WHERE node_id = ?", nodeId);
      } else {
        persistMapJson("pending_upgrades.json", pendingNodeUpgrades);
      }
    }
    return manifest;
  }

  public UpgradeManifest peekPendingUpgrade(String nodeId) {
    return pendingNodeUpgrades.get(nodeId);
  }

  private void loadUpgradeManifest() {
    if (databaseMode) {
      try {
        List<Map<String, Object>> rows = jdbc.queryForList(
            "SELECT * FROM chg_upgrade_manifests ORDER BY version DESC LIMIT 1");
        if (!rows.isEmpty()) {
          Map<String, Object> row = rows.getFirst();
          UpgradeManifest m = new UpgradeManifest(
              ((Number) row.get("version")).longValue(),
              (String) row.get("version_name"),
              (String) row.get("package_url"),
              (String) row.get("sha256_hash"),
              (String) row.get("signature"),
              ((Number) row.get("min_compatible_version")).longValue(),
              (String) row.get("release_notes"),
              toInstant(row.get("created_at"))
          );
          latestManifest.set(signManifest(m));
          return;
        }
      } catch (Exception ignored) {}
    } else {
      File manifestFile = new File(storageDir, "upgrade_manifest.json");
      if (manifestFile.exists()) {
        try {
          UpgradeManifest m = mapper.readValue(manifestFile, UpgradeManifest.class);
          latestManifest.set(signManifest(m));
        } catch (Exception ignored) {}
      }
    }
  }

  public Map<String, Object> batchUpgradeNodes(int version, String packageUrl, String sha256, String operator) {
    int count = 0;
    List<String> upgraded = new ArrayList<>();
    for (String nodeId : edgeNodesTelemetry.keySet()) {
      upgradeSingleNode(nodeId, version, packageUrl, sha256, operator);
      upgraded.add(nodeId);
      count++;
    }
    audit(operator != null ? operator : "ADMIN", "BATCH_UPGRADE", "all_nodes",
        "全网批量推送固件版本 v" + version + " (共 " + count + " 台工控机)");
    return Map.of("success", true, "count", count, "nodes", upgraded);
  }

  public synchronized UpgradeNodeReport saveNodeUpgradeReport(String nodeId, UpgradeNodeReport report) {
    nodeUpgradeReports.put(nodeId, report);
    if (databaseMode) {
      jdbc.update(
          "INSERT INTO chg_upgrade_node_reports(node_id,current_version,target_version,state,message,last_error,updated_at) " +
          "VALUES(?,?,?,?,?,?,?) ON CONFLICT(node_id) DO UPDATE SET current_version=EXCLUDED.current_version,target_version=EXCLUDED.target_version,state=EXCLUDED.state,message=EXCLUDED.message,last_error=EXCLUDED.last_error,updated_at=EXCLUDED.updated_at",
          nodeId,
          report.currentVersion(),
          report.targetVersion(),
          report.state(),
          report.message() != null ? report.message() : "",
          report.lastError(),
          Timestamp.from(report.updatedAt() != null ? report.updatedAt() : Instant.now()));
    } else {
      persistMapJson("upgrade_reports.json", nodeUpgradeReports);
    }
    return report;
  }

  public Map<String, UpgradeNodeReport> nodeUpgradeReports() {
    return Map.copyOf(nodeUpgradeReports);
  }

  // ====== 白名单管理（支持多停车场隔离） ======

  public List<WhitelistRecord> listWhitelist() {
    return listWhitelist("default");
  }

  public List<WhitelistRecord> listWhitelist(String siteId) {
    String effectiveSiteId = (siteId == null || siteId.isBlank()) ? "default" : siteId;
    return siteConfiguration(effectiveSiteId).whitelist().stream()
        .sorted(Comparator.comparing(WhitelistRecord::addedAt).reversed())
        .toList();
  }

  public synchronized WhitelistRecord saveWhitelist(WhitelistRecord record) {
    return saveWhitelist("default", record);
  }

  public synchronized WhitelistRecord saveWhitelist(String siteId, WhitelistRecord record) {
    String effectiveSiteId = (siteId == null || siteId.isBlank()) ? "default" : siteId;
    PlatformConfiguration current = siteConfiguration(effectiveSiteId);
    WhitelistRecord normalized = new WhitelistRecord(
        normalizePlate(record.plate()),
        record.vehicleType(),
        record.reason(),
        record.addedAt() == null ? Instant.now() : record.addedAt(),
        record.expiresAt(),
        record.enabled());
    Map<String, WhitelistRecord> entries = current.whitelist().stream()
        .collect(Collectors.toMap(item -> normalizePlate(item.plate()), item -> item, (left, right) -> right));
    entries.put(normalized.plate(), normalized);
    updateSiteConfiguration(effectiveSiteId, new PlatformConfiguration(
        current.version(), Instant.now(), current.rules(), current.bindings(),
        List.copyOf(entries.values()), current.cameras()));
    return normalized;
  }

  public synchronized boolean removeWhitelist(String plate) {
    return removeWhitelist("default", plate);
  }

  public synchronized boolean removeWhitelist(String siteId, String plate) {
    String effectiveSiteId = (siteId == null || siteId.isBlank()) ? "default" : siteId;
    PlatformConfiguration current = siteConfiguration(effectiveSiteId);
    String normalizedPlate = normalizePlate(plate);
    List<WhitelistRecord> entries = current.whitelist().stream()
        .filter(item -> !normalizePlate(item.plate()).equals(normalizedPlate))
        .toList();
    boolean removed = entries.size() != current.whitelist().size();
    if (removed) {
      updateSiteConfiguration(effectiveSiteId, new PlatformConfiguration(
          current.version(), Instant.now(), current.rules(), current.bindings(), entries, current.cameras()));
    }
    return removed;
  }

  public boolean isWhitelisted(String plate) {
    return isWhitelisted("default", plate);
  }

  public boolean isWhitelisted(String siteId, String plate) {
    if (plate == null || plate.isBlank()) return false;
    String effectiveSiteId = (siteId == null || siteId.isBlank()) ? "default" : siteId;
    PlatformConfiguration cfg = "default".equals(effectiveSiteId)
        ? configuration
        : configurationBySite.getOrDefault(effectiveSiteId, configuration);
    String normalizedPlate = normalizePlate(plate);
    WhitelistRecord record = cfg.whitelist().stream()
        .filter(item -> normalizePlate(item.plate()).equals(normalizedPlate))
        .findFirst()
        .orElse(null);
    if (record == null || !record.enabled()) return false;
    return record.expiresAt() == null || record.expiresAt().isAfter(Instant.now());
  }

  private String normalizePlate(String plate) {
    return plate == null ? "" : plate.trim().toUpperCase(Locale.ROOT);
  }

  // ====== 多停车场管理 ======

  public List<Site> listSites() {
    if (sitesCache.isEmpty()) {
      Site defaultSite = new Site("default", "默认停车场", "", true, Instant.now());
      sitesCache.put("default", defaultSite);
      if (databaseMode) {
        persistSite(defaultSite);
      }
    }
    return sitesCache.values().stream()
        .sorted(Comparator.comparing(Site::createdAt))
        .toList();
  }

  public List<Map<String, Object>> siteSummaries() {
    List<Site> sites = listSites();
    Instant threshold = Instant.now().minus(90, ChronoUnit.SECONDS);
    List<Map<String, Object>> result = new ArrayList<>(sites.size());
    for (Site site : sites) {
      String sid = site.siteId();
      Set<String> nodes = collectNodesForSite(sid);
      int onlineNodes = (int) nodes.stream()
          .filter(n -> heartbeats.containsKey(n) && heartbeats.get(n).isAfter(threshold))
          .count();
      int cameraCount = getCamerasForSite(sid).size();
      int spaceCount = getBindingsForSite(sid).size();
      Map<String, Object> row = new LinkedHashMap<>();
      row.put("siteId", sid);
      row.put("siteName", site.siteName());
      row.put("address", site.address());
      row.put("enabled", site.enabled());
      row.put("createdAt", site.createdAt());
      row.put("nodeCount", nodes.size());
      row.put("onlineNodeCount", onlineNodes);
      row.put("cameraCount", cameraCount);
      row.put("spaceCount", spaceCount);
      result.add(row);
    }
    return result;
  }

  public synchronized Site createSite(Site site) {
    if (sitesCache.containsKey(site.siteId())) {
      throw new IllegalArgumentException("site already exists: " + site.siteId());
    }
    PlatformConfiguration siteConfig = new PlatformConfiguration(
        0, Instant.now(), configuration.rules(), List.of(), List.of(), List.of());
    if (databaseMode) {
      transaction.executeWithoutResult(status -> {
        persistSite(site);
        persistSiteConfiguration(site.siteId(), siteConfig);
      });
    }
    sitesCache.put(site.siteId(), site);
    configurationBySite.put(site.siteId(), siteConfig);
    try {
      if (!databaseMode) persistMultiSiteState();
    } catch (RuntimeException error) {
      sitesCache.remove(site.siteId());
      configurationBySite.remove(site.siteId());
      throw error;
    }
    return site;
  }

  public synchronized Site updateSite(String siteId, Site incoming) {
    Site existing = sitesCache.get(siteId);
    if (existing == null) {
      throw new IllegalArgumentException("site not found: " + siteId);
    }
    Site merged = new Site(
        siteId,
        (incoming.siteName() != null && !incoming.siteName().isBlank()) ? incoming.siteName() : existing.siteName(),
        incoming.address() != null ? incoming.address() : existing.address(),
        incoming.enabled() != null ? incoming.enabled() : existing.enabled(),
        existing.createdAt()
    );
    if (databaseMode) {
      persistSite(merged);
    }
    sitesCache.put(siteId, merged);
    if (!databaseMode) {
      persistMultiSiteState();
    }
    return merged;
  }

  public synchronized void deleteSite(String siteId) {
    if ("default".equalsIgnoreCase(siteId)) {
      throw new IllegalArgumentException("Cannot delete default site");
    }
    if (!sitesCache.containsKey(siteId)) {
      return;
    }
    if (databaseMode) {
      transaction.executeWithoutResult(status -> {
        jdbc.update("DELETE FROM chg_whitelist WHERE site_id = ?", siteId);
        jdbc.update("DELETE FROM chg_space_runtime_status WHERE site_id = ?", siteId);
        jdbc.update("DELETE FROM chg_plate_recognition_records WHERE site_id = ?", siteId);
        jdbc.update("DELETE FROM chg_node_heartbeats WHERE site_id = ?", siteId);
        jdbc.update("DELETE FROM chg_cameras WHERE site_id = ?", siteId);
        jdbc.update("DELETE FROM chg_site_configuration WHERE site_id = ?", siteId);
        jdbc.update("DELETE FROM chg_node_site_mapping WHERE site_id = ?", siteId);
        jdbc.update("DELETE FROM chg_violation_events WHERE site_id = ?", siteId);
        jdbc.update("DELETE FROM chg_sites WHERE site_id = ?", siteId);
      });
    }
    sitesCache.remove(siteId);
    configurationBySite.remove(siteId);
    camerasBySite.remove(siteId);
    spaceStatuses.entrySet().removeIf(e -> siteId.equalsIgnoreCase(e.getValue().siteId()));
    recognitionFeed.removeIf(r -> siteId.equalsIgnoreCase(r.getSiteId()));
    Set<String> nodesToRemove = new HashSet<>();
    for (Map.Entry<String, String> entry : nodeToSite.entrySet()) {
      if (siteId.equals(entry.getValue())) {
        nodesToRemove.add(entry.getKey());
      }
    }
    for (String nId : nodesToRemove) {
      nodeToSite.remove(nId);
      edgeNodesTelemetry.remove(nId);
      heartbeats.remove(nId);
    }
    if (!databaseMode) {
      persistMultiSiteState();
      persistCamerasToDisk();
      persistNodesToDisk();
      persistSpaceStatusesToDisk();
    }
  }

  public Site getSite(String siteId) {
    return sitesCache.get(siteId);
  }

  /** 获取指定场地的配置 */
  public PlatformConfiguration siteConfiguration(String siteId) {
    if (siteId == null || siteId.isBlank() || "default".equals(siteId)) {
      return configuration.withCameras(getCamerasForSite("default"));
    }
    PlatformConfiguration siteCfg = configurationBySite.get(siteId);
    if (siteCfg == null) throw new NoSuchElementException("site configuration not found: " + siteId);
    return siteCfg.withCameras(getCamerasForSite(siteId));
  }

  /** 更新指定场地的配置 */
  public synchronized PlatformConfiguration updateSiteConfiguration(String siteId, PlatformConfiguration incoming) {
    if (siteId == null || siteId.isBlank() || "default".equals(siteId)) {
      return updateConfiguration(incoming);
    }
    PlatformConfiguration current = siteConfiguration(siteId);
    List<CameraConfig> cameras = getCamerasForSite(siteId);
    PlatformConfiguration next = new PlatformConfiguration(
        current.version() + 1, Instant.now(),
        incoming.rules(),
        List.copyOf(incoming.bindings()),
        List.copyOf(incoming.whitelist()),
        cameras);
    if (databaseMode) persistSiteConfiguration(siteId, next);
    PlatformConfiguration previous = configurationBySite.put(siteId, next);
    try {
      if (!databaseMode) persistMultiSiteState();
    } catch (RuntimeException error) {
      configurationBySite.put(siteId, previous);
      throw error;
    }
    return next;
  }

  /** 更新指定场地的规则 */
  public synchronized RuleConfig updateSiteRules(String siteId, RuleConfig rules) {
    if (siteId == null || siteId.isBlank() || "default".equals(siteId)) {
      return updateRules(rules);
    }
    PlatformConfiguration current = siteConfiguration(siteId);
    List<CameraConfig> cameras = getCamerasForSite(siteId);
    PlatformConfiguration next = new PlatformConfiguration(
        current.version() + 1, Instant.now(), rules,
        current.bindings(), current.whitelist(), cameras);
    if (databaseMode) persistSiteConfiguration(siteId, next);
    PlatformConfiguration previous = configurationBySite.put(siteId, next);
    try {
      if (!databaseMode) persistMultiSiteState();
    } catch (RuntimeException error) {
      configurationBySite.put(siteId, previous);
      throw error;
    }
    return rules;
  }

  private synchronized void bumpSiteConfigVersion(String siteId) {
    String effectiveSiteId = (siteId == null || siteId.isBlank()) ? "default" : siteId;
    PlatformConfiguration current = siteConfiguration(effectiveSiteId);
    updateSiteConfiguration(effectiveSiteId, current);
  }

  /** 将节点绑定到停车场 */
  public synchronized void bindNodeToSite(String nodeId, String siteId) {
    if (!sitesCache.containsKey(siteId)) {
      throw new NoSuchElementException("site not found: " + siteId);
    }
    if (databaseMode) {
      jdbc.update(
          "INSERT INTO chg_node_site_mapping(node_id,site_id,updated_at) VALUES(?,?,?) " +
          "ON CONFLICT(node_id) DO UPDATE SET site_id=EXCLUDED.site_id,updated_at=EXCLUDED.updated_at",
          nodeId, siteId, Timestamp.from(Instant.now()));
      jdbc.update("UPDATE chg_node_heartbeats SET site_id = ? WHERE node_id = ?", siteId, nodeId);
    }
    String previous = nodeToSite.put(nodeId, siteId);
    EdgeNodeTelemetry tele = edgeNodesTelemetry.get(nodeId);
    if (tele != null) {
      tele.setSiteId(siteId);
    }
    try {
      if (!databaseMode) {
        persistMultiSiteState();
        persistNodesToDisk();
      }
    } catch (RuntimeException error) {
      if (previous == null) nodeToSite.remove(nodeId); else nodeToSite.put(nodeId, previous);
      if (tele != null) tele.setSiteId(previous != null ? previous : "default");
      throw error;
    }
  }

  /** 通过节点 ID 获取对应场地的配置（附带归属该节点的真实摄像头配置） */
  public PlatformConfiguration configurationForNode(String nodeId) {
    String siteId = siteIdForNode(nodeId);
    PlatformConfiguration base = siteConfiguration(siteId);
    List<CameraConfig> allSiteCameras = getCamerasForSite(siteId);
    if (nodeId == null || nodeId.isBlank()) {
      return base;
    }

    Map<String, CameraConfig> cameraById = new HashMap<>();
    for (CameraConfig cam : allSiteCameras) {
      cameraById.put(cam.getCameraId(), cam);
    }

    List<CameraConfig> nodeCameras = allSiteCameras.stream()
        .filter(c -> Boolean.TRUE.equals(c.getEnabled()))
        .filter(c -> c.getNodeId() == null || c.getNodeId().isBlank() || nodeId.equals(c.getNodeId()))
        .toList();

    EdgeNodeTelemetry nodeMeta = edgeNodesTelemetry.get(nodeId);
    Set<String> assignedSpaces = (nodeMeta != null && nodeMeta.getAssignedSpaces() != null && !nodeMeta.getAssignedSpaces().isEmpty())
        ? new HashSet<>(nodeMeta.getAssignedSpaces())
        : null;

    List<DeviceBinding> nodeBindings = base.bindings().stream()
        .filter(b -> {
          if (assignedSpaces != null && !assignedSpaces.contains(b.spaceId())) {
            return false;
          }
          CameraConfig cam = cameraById.get(b.cameraId());
          if (cam != null && cam.getNodeId() != null && !cam.getNodeId().isBlank() && !nodeId.equals(cam.getNodeId())) {
            return false;
          }
          return true;
        })
        .toList();

    return new PlatformConfiguration(
        base.version(),
        base.updatedAt(),
        base.rules(),
        nodeBindings,
        base.whitelist(),
        nodeCameras
    );
  }

  public String siteIdForNode(String nodeId) {
    if (nodeId == null || nodeId.isBlank()) return "default";
    return nodeToSite.getOrDefault(nodeId, "default");
  }

  public Map<String, String> nodeToSiteMap() {
    return Map.copyOf(nodeToSite);
  }

  private Set<String> collectNodesForSite(String siteId) {
    Set<String> nodes = new HashSet<>();
    for (Map.Entry<String, String> e : nodeToSite.entrySet()) {
      if (siteId.equals(e.getValue())) nodes.add(e.getKey());
    }
    for (EdgeNodeTelemetry node : edgeNodesTelemetry.values()) {
      if (siteId.equals(node.getSiteId())) nodes.add(node.getNodeId());
    }
    if ("default".equals(siteId)) {
      for (String hbNode : heartbeats.keySet()) {
        if (!nodeToSite.containsKey(hbNode)) {
          nodes.add(hbNode);
        }
      }
    }
    return nodes;
  }

  // ====== 大屏统计指标 ======

  public DashboardStats dashboardStats() {
    return dashboardStats(null);
  }

  /** 按场地过滤统计（siteId 为 null 表示全局） */
  public DashboardStats dashboardStats(String siteId) {
    var filteredEvents = events.values().stream();
    if (siteId != null && !siteId.isBlank()) {
      filteredEvents = filteredEvents.filter(e -> siteId.equals(e.siteId()));
    }
    List<ViolationEvent> eventList = filteredEvents.toList();

    long total = eventList.size();
    Instant oneDayAgo = Instant.now().minus(24, ChronoUnit.HOURS);
    long today = eventList.stream()
        .filter(e -> e.occurredAt() != null && e.occurredAt().isAfter(oneDayAgo))
        .count();

    Map<String, Long> byType = eventList.stream()
        .filter(e -> e.violationType() != null)
        .collect(Collectors.groupingBy(ViolationEvent::violationType, Collectors.counting()));

    Map<String, Long> bySpace = eventList.stream()
        .filter(e -> e.spaceId() != null)
        .collect(Collectors.groupingBy(ViolationEvent::spaceId, Collectors.counting()));

    Instant healthyThreshold = Instant.now().minus(90, ChronoUnit.SECONDS);
    Set<String> nodes;
    if (siteId != null && !siteId.isBlank()) {
      nodes = collectNodesForSite(siteId);
    } else {
      nodes = new HashSet<>();
      nodes.addAll(heartbeats.keySet());
      nodes.addAll(nodeToSite.keySet());
      nodes.addAll(edgeNodesTelemetry.keySet());
    }
    int onlineCount = (int) nodes.stream()
        .filter(n -> heartbeats.containsKey(n) && heartbeats.get(n).isAfter(healthyThreshold))
        .count();
    int totalNodes = nodes.size();

    return new DashboardStats(total, today, byType, bySpace, onlineCount, totalNodes);
  }

  // 本地轻量持久化
  private synchronized void persistEvents() {
    if (databaseMode) return;
    try {
      if (!storageDir.exists()) storageDir.mkdirs();
      writeJsonAtomically("events.json", events.values());
    } catch (Exception error) { throw new IllegalStateException("persist events failed", error); }
  }

  private synchronized void persistToDisk() {
    if (databaseMode) return;
    try {
      if (!storageDir.exists()) storageDir.mkdirs();
      writeJsonAtomically("configuration.json", configuration);
    } catch (Exception error) { throw new IllegalStateException("persist configuration failed", error); }
  }

  private synchronized void persistMultiSiteState() {
    if (databaseMode) return;
    try {
      if (!storageDir.exists()) storageDir.mkdirs();
      writeJsonAtomically("sites.json", sitesCache);
      writeJsonAtomically("site-configurations.json", configurationBySite);
      writeJsonAtomically("node-sites.json", nodeToSite);
    } catch (Exception error) {
      throw new IllegalStateException("persist multi-site state failed", error);
    }
  }

  private synchronized void persistCamerasToDisk() {
    if (databaseMode) return;
    persistObjectJson("cameras.json", camerasBySite);
  }

  private synchronized void persistNodesToDisk() {
    if (databaseMode) return;
    persistObjectJson("nodes.json", edgeNodesTelemetry);
  }

  private synchronized void persistProtocolsToDisk() {
    if (databaseMode) return;
    persistObjectJson("protocols.json", protocolsCache);
  }

  private synchronized void persistSpaceStatusesToDisk() {
    if (databaseMode) return;
    persistObjectJson("space_statuses.json", spaceStatuses);
  }

  private synchronized void persistLprRecordsToDisk() {
    if (databaseMode) return;
    persistObjectJson("lpr_records.json", recognitionFeed);
  }

  private void persistMapJson(String fileName, Object map) {
    persistObjectJson(fileName, map);
  }

  private void persistObjectJson(String fileName, Object value) {
    if (databaseMode) return;
    try {
      if (!storageDir.exists()) storageDir.mkdirs();
      writeJsonAtomically(fileName, value);
    } catch (Exception error) {
      throw new IllegalStateException("persist " + fileName + " failed", error);
    }
  }

  private void writeJsonAtomically(String name, Object value) throws Exception {
    Path target = storageDir.toPath().resolve(name);
    Path temporary = storageDir.toPath().resolve(name + ".tmp");
    mapper.writeValue(temporary.toFile(), value);
    moveAtomically(temporary, target);
  }

  private void moveAtomically(Path temporary, Path target) throws IOException {
    try {
      Files.move(temporary, target, StandardCopyOption.REPLACE_EXISTING, StandardCopyOption.ATOMIC_MOVE);
    } catch (java.nio.file.AtomicMoveNotSupportedException error) {
      Files.move(temporary, target, StandardCopyOption.REPLACE_EXISTING);
    }
  }

  private void persistConfiguration(PlatformConfiguration next) {
    if (!databaseMode) {
      PlatformConfiguration previous = configuration;
      configuration = next;
      try {
        persistToDisk();
      } finally {
        configuration = previous;
      }
      return;
    }
    try {
      String rulesJson = mapper.writeValueAsString(next.rules());
      String bindingsJson = mapper.writeValueAsString(next.bindings());
      String whitelistJson = mapper.writeValueAsString(next.whitelist());
      transaction.executeWithoutResult(status -> {
        jdbc.update(
            "INSERT INTO chg_platform_configuration(config_version,updated_at,rules_json,bindings_json,whitelist_json,site_id) VALUES(?,?,?,?,?,?) " +
            "ON CONFLICT(config_version) DO UPDATE SET updated_at=EXCLUDED.updated_at,rules_json=EXCLUDED.rules_json,bindings_json=EXCLUDED.bindings_json,whitelist_json=EXCLUDED.whitelist_json",
            next.version(), Timestamp.from(next.updatedAt()), rulesJson, bindingsJson, whitelistJson, "default");
        syncWhitelistTable("default", next.whitelist());
      });
    } catch (Exception error) {
      throw new IllegalStateException("persist postgres configuration failed", error);
    }
  }

  private void persistSite(Site site) {
    if (!databaseMode) return;
    try {
      jdbc.update(
          "INSERT INTO chg_sites(site_id,site_name,address,enabled,created_at) VALUES(?,?,?,?,?) " +
          "ON CONFLICT(site_id) DO UPDATE SET site_name=EXCLUDED.site_name,address=EXCLUDED.address,enabled=EXCLUDED.enabled",
          site.siteId(), site.siteName(), site.address(), site.enabled(),
          Timestamp.from(site.createdAt()));
    } catch (Exception error) {
      throw new IllegalStateException("persist site failed", error);
    }
  }

  private void persistSiteConfiguration(String siteId, PlatformConfiguration next) {
    if (!databaseMode) return;
    try {
      String rulesJson = mapper.writeValueAsString(next.rules());
      String bindingsJson = mapper.writeValueAsString(next.bindings());
      String whitelistJson = mapper.writeValueAsString(next.whitelist());
      transaction.executeWithoutResult(status -> {
        jdbc.update(
            "INSERT INTO chg_site_configuration(site_id,config_version,updated_at,rules_json,bindings_json,whitelist_json) VALUES(?,?,?,?,?,?) " +
            "ON CONFLICT(site_id,config_version) DO UPDATE SET updated_at=EXCLUDED.updated_at,rules_json=EXCLUDED.rules_json,bindings_json=EXCLUDED.bindings_json,whitelist_json=EXCLUDED.whitelist_json",
            siteId, next.version(), Timestamp.from(next.updatedAt()), rulesJson, bindingsJson, whitelistJson);
        syncWhitelistTable(siteId, next.whitelist());
      });
    } catch (Exception error) {
      throw new IllegalStateException("persist site configuration failed for " + siteId, error);
    }
  }

  private void syncWhitelistTable(String siteId, List<WhitelistRecord> whitelist) {
    jdbc.update("DELETE FROM chg_whitelist WHERE site_id = ?", siteId);
    for (WhitelistRecord entry : whitelist) {
      jdbc.update(
          "INSERT INTO chg_whitelist(site_id,plate,vehicle_type,reason,added_at,expires_at,enabled) VALUES(?,?,?,?,?,?,?)",
          siteId,
          normalizePlate(entry.plate()),
          entry.vehicleType(),
          entry.reason(),
          Timestamp.from(entry.addedAt()),
          entry.expiresAt() == null ? null : Timestamp.from(entry.expiresAt()),
          entry.enabled());
    }
  }

  private void persistEvent(ViolationEvent event) {
    if (!databaseMode) {
      persistEvents();
      return;
    }
    jdbc.update(
        "INSERT INTO chg_violation_events(event_id,session_id,site_id,space_id,plate,vehicle_type,violation_type,occurred_at,evidence_url,event_status,cleared_at,updated_at,acknowledged_by,acknowledged_at) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?) " +
        "ON CONFLICT(event_id) DO UPDATE SET session_id=EXCLUDED.session_id,site_id=EXCLUDED.site_id,space_id=EXCLUDED.space_id,plate=EXCLUDED.plate,vehicle_type=EXCLUDED.vehicle_type,violation_type=EXCLUDED.violation_type,evidence_url=EXCLUDED.evidence_url,event_status=EXCLUDED.event_status,cleared_at=EXCLUDED.cleared_at,updated_at=EXCLUDED.updated_at,acknowledged_by=COALESCE(EXCLUDED.acknowledged_by,chg_violation_events.acknowledged_by),acknowledged_at=COALESCE(EXCLUDED.acknowledged_at,chg_violation_events.acknowledged_at)",
        event.eventId().toString(),
        event.sessionId(),
        event.siteId(),
        event.spaceId(),
        event.plate(),
        event.vehicleType(),
        event.violationType(),
        Timestamp.from(event.occurredAt()),
        event.evidenceUrl(),
        event.status().name(),
        event.clearedAt() == null ? null : Timestamp.from(event.clearedAt()),
        Timestamp.from(event.updatedAt()),
        event.acknowledgedBy(),
        event.acknowledgedAt() == null ? null : Timestamp.from(event.acknowledgedAt()));
  }

  public void audit(String actor, String action, String resource, String detail) {
    String safeDetail = detail == null ? "" : detail;
    OperationAuditRecord record = new OperationAuditRecord(actor, action, resource, safeDetail, Instant.now());
    if (databaseMode) {
      jdbc.update(
          "INSERT INTO chg_operation_audit(actor,action,resource,detail,occurred_at) VALUES(?,?,?,?,?)",
          actor, action, resource, safeDetail, Timestamp.from(record.occurredAt()));
    } else {
      try {
        if (!storageDir.exists()) storageDir.mkdirs();
        File file = new File(storageDir, "audit.jsonl");
        String line = mapper.writeValueAsString(record) + System.lineSeparator();
        Files.writeString(file.toPath(), line, StandardOpenOption.CREATE, StandardOpenOption.APPEND);
      } catch (Exception error) {
        throw new IllegalStateException("persist audit failed", error);
      }
    }
    audits.add(record);
  }

  public List<OperationAuditRecord> auditRecords() {
    return audits.stream()
        .sorted(Comparator.comparing(OperationAuditRecord::occurredAt).reversed())
        .toList();
  }

  private void loadFromDatabase() {
    try {
      for (Map<String, Object> row : jdbc.queryForList(
          "SELECT site_id,site_name,address,enabled,created_at FROM chg_sites")) {
        Site site = new Site((String) row.get("site_id"), (String) row.get("site_name"),
            (String) row.get("address"), (Boolean) row.get("enabled"), toInstant(row.get("created_at")));
        sitesCache.put(site.siteId(), site);
      }
      if (!sitesCache.containsKey("default")) {
        Site defaultSite = new Site("default", "默认停车场", "", true, Instant.now());
        sitesCache.put("default", defaultSite);
        persistSite(defaultSite);
      }

      // 从 chg_whitelist 加载按场地分组的白名单，确保单表记录权威可读
      Map<String, List<WhitelistRecord>> whitelistBySite = new HashMap<>();
      try {
        for (Map<String, Object> row : jdbc.queryForList("SELECT * FROM chg_whitelist")) {
          String wSite = row.get("site_id") != null ? (String) row.get("site_id") : "default";
          WhitelistRecord rec = new WhitelistRecord(
              (String) row.get("plate"),
              (String) row.get("vehicle_type"),
              (String) row.get("reason"),
              toInstant(row.get("added_at")),
              row.get("expires_at") == null ? null : toInstant(row.get("expires_at")),
              row.get("enabled") == null || (Boolean) row.get("enabled"));
          whitelistBySite.computeIfAbsent(wSite, k -> new ArrayList<>()).add(rec);
        }
      } catch (Exception ignored) {}

      List<Map<String, Object>> configs = jdbc.queryForList(
          "SELECT config_version,updated_at,rules_json,bindings_json,whitelist_json FROM chg_platform_configuration WHERE site_id IS NULL OR site_id='default' ORDER BY config_version DESC LIMIT 1");
      if (!configs.isEmpty()) {
        Map<String, Object> row = configs.getFirst();
        List<WhitelistRecord> jsonWl = mapper.readValue(
            (String) row.get("whitelist_json"), new TypeReference<List<WhitelistRecord>>() {});
        List<WhitelistRecord> dbWl = whitelistBySite.getOrDefault("default", Collections.emptyList());
        List<WhitelistRecord> effectiveWl = !dbWl.isEmpty() ? dbWl : jsonWl;
        configuration = new PlatformConfiguration(
            ((Number) row.get("config_version")).longValue(),
            toInstant(row.get("updated_at")),
            mapper.readValue((String) row.get("rules_json"), RuleConfig.class),
            mapper.readValue((String) row.get("bindings_json"), new TypeReference<List<DeviceBinding>>() {}),
            effectiveWl,
            List.of());
      }

      for (ViolationEvent event : jdbc.query(
          "SELECT event_id,session_id,site_id,space_id,plate,vehicle_type,violation_type,occurred_at,evidence_url,event_status,cleared_at,updated_at,acknowledged_by,acknowledged_at FROM chg_violation_events",
          (rs, rowNum) -> new ViolationEvent(
              UUID.fromString(rs.getString("event_id")),
              rs.getString("session_id"),
              rs.getString("site_id"),
              rs.getString("space_id"),
              rs.getString("plate"),
              rs.getString("vehicle_type"),
              rs.getString("violation_type"),
              rs.getTimestamp("occurred_at").toInstant(),
              rs.getString("evidence_url"),
              EventStatus.valueOf(rs.getString("event_status")),
              rs.getTimestamp("cleared_at") == null ? null : rs.getTimestamp("cleared_at").toInstant(),
              rs.getTimestamp("updated_at") == null ? rs.getTimestamp("occurred_at").toInstant() : rs.getTimestamp("updated_at").toInstant(),
              rs.getString("acknowledged_by"),
              rs.getTimestamp("acknowledged_at") == null ? null : rs.getTimestamp("acknowledged_at").toInstant()))) {
        events.put(event.eventId(), event);
      }

      for (Map<String, Object> row : jdbc.queryForList("SELECT node_id,last_heartbeat FROM chg_node_heartbeats")) {
        heartbeats.put((String) row.get("node_id"), toInstant(row.get("last_heartbeat")));
      }

      for (Map<String, Object> row : jdbc.queryForList(
          "SELECT actor,action,resource,detail,occurred_at FROM chg_operation_audit ORDER BY occurred_at")) {
        audits.add(new OperationAuditRecord(
            (String) row.get("actor"),
            (String) row.get("action"),
            (String) row.get("resource"),
            (String) row.get("detail"),
            toInstant(row.get("occurred_at"))));
      }

      for (Map<String, Object> row : jdbc.queryForList(
          "SELECT m.node_id, m.site_id, m.node_name, m.auth_key, m.ip_address AS m_ip, m.assigned_spaces_json, m.reboot_requested_at, " +
          "h.last_heartbeat, h.ip_address AS h_ip, h.status, h.cpu_usage, h.memory_usage, h.temperature, " +
          "h.uptime_seconds, h.frames_processed, h.violations_detected, h.alarms_triggered, h.disk_watermark, h.version " +
          "FROM chg_node_site_mapping m LEFT JOIN chg_node_heartbeats h ON m.node_id = h.node_id")) {
        EdgeNodeTelemetry tele = mapDbNodeRow(row, false);
        edgeNodesTelemetry.put(tele.getNodeId(), tele);
        nodeToSite.put(tele.getNodeId(), tele.getSiteId());
      }

      // 同时恢复仅发送过心跳、尚未在 mapping 表注册的边缘节点
      for (Map<String, Object> row : jdbc.queryForList(
          "SELECT h.node_id, COALESCE(NULLIF(h.site_id,''), 'default') AS site_id, h.ip_address AS h_ip, " +
          "h.last_heartbeat, h.status, h.cpu_usage, h.memory_usage, h.temperature, " +
          "h.uptime_seconds, h.frames_processed, h.violations_detected, h.alarms_triggered, h.disk_watermark, h.version " +
          "FROM chg_node_heartbeats h WHERE h.node_id NOT IN (SELECT node_id FROM chg_node_site_mapping)")) {
        EdgeNodeTelemetry tele = mapDbNodeRow(row, true);
        edgeNodesTelemetry.putIfAbsent(tele.getNodeId(), tele);
        nodeToSite.putIfAbsent(tele.getNodeId(), tele.getSiteId());
      }

      for (Map<String, Object> row : jdbc.queryForList("SELECT * FROM chg_cameras")) {
        String siteId = (String) row.get("site_id");
        CameraConfig c = new CameraConfig(
            (String) row.get("camera_id"),
            (String) row.get("camera_name"),
            siteId,
            (String) row.get("space_id"),
            (String) row.get("node_id"),
            (String) row.get("protocol"),
            (String) row.get("stream_url"),
            (String) row.get("sip_device_id"),
            (String) row.get("resolution"),
            (String) row.get("codec"),
            (String) row.get("status"),
            toInstantNullable(row.get("last_frame_at")),
            (String) row.get("roi_json"),
            row.get("enabled") == null ? Boolean.TRUE : (Boolean) row.get("enabled")
        );
        camerasBySite.computeIfAbsent(siteId, k -> new ArrayList<>()).add(c);
      }

      for (Map<String, Object> row : jdbc.queryForList(
          "SELECT DISTINCT ON (site_id) site_id,config_version,updated_at,rules_json,bindings_json,whitelist_json " +
              "FROM chg_site_configuration ORDER BY site_id,config_version DESC")) {
        String sId = (String) row.get("site_id");
        List<WhitelistRecord> jsonWl = mapper.readValue(
            (String) row.get("whitelist_json"), new TypeReference<List<WhitelistRecord>>() {});
        List<WhitelistRecord> dbWl = whitelistBySite.getOrDefault(sId, Collections.emptyList());
        List<WhitelistRecord> effectiveWl = !dbWl.isEmpty() ? dbWl : jsonWl;
        configurationBySite.put(sId, new PlatformConfiguration(
            ((Number) row.get("config_version")).longValue(),
            toInstant(row.get("updated_at")),
            mapper.readValue((String) row.get("rules_json"), RuleConfig.class),
            mapper.readValue((String) row.get("bindings_json"), new TypeReference<List<DeviceBinding>>() {}),
            effectiveWl,
            List.of()));
      }

      for (Map<String, Object> row : jdbc.queryForList(
          "SELECT * FROM chg_charger_protocols ORDER BY is_builtin DESC, protocol_id ASC")) {
        ChargerProtocol p = new ChargerProtocol(
            (String) row.get("protocol_id"),
            (String) row.get("brand_name"),
            (String) row.get("protocol_category"),
            (String) row.get("description"),
            row.get("default_port") == null ? 80 : ((Number) row.get("default_port")).intValue(),
            (String) row.get("connection_config_schema"),
            (String) row.get("status_mapping"),
            (String) row.get("data_parser_rules"),
            row.get("is_builtin") != null && (Boolean) row.get("is_builtin"),
            row.get("enabled") == null || (Boolean) row.get("enabled"),
            toInstant(row.get("created_at")),
            toInstant(row.get("updated_at"))
        );
        protocolsCache.put(p.protocolId(), p);
      }

      for (Map<String, Object> row : jdbc.queryForList("SELECT * FROM chg_space_runtime_status")) {
        String activeIdStr = (String) row.get("active_event_id");
        UUID activeId = (activeIdStr != null && !activeIdStr.isBlank()) ? UUID.fromString(activeIdStr) : null;
        SpaceRuntimeStatus st = new SpaceRuntimeStatus(
            (String) row.get("node_id"),
            (String) row.get("site_id"),
            (String) row.get("space_id"),
            (String) row.get("phase"),
            (String) row.get("charger_status"),
            (String) row.get("plate"),
            (String) row.get("vehicle_type"),
            row.get("alarming") != null && (Boolean) row.get("alarming"),
            activeId,
            toInstant(row.get("observed_at")),
            row.get("is_simulated") != null && (Boolean) row.get("is_simulated"),
            (String) row.get("stream_type"),
            (String) row.get("snapshot_url"),
            row.get("confidence") != null ? ((Number) row.get("confidence")).doubleValue() : 0.0
        );
        spaceStatuses.put(st.nodeId() + ":" + st.spaceId(), st);
      }

      for (Map<String, Object> row : jdbc.queryForList(
          "SELECT * FROM chg_plate_recognition_records ORDER BY recognized_at DESC LIMIT ?",
          MAX_RECOGNITION_FEED_MEMORY)) {
        PlateRecognitionRecord rec = new PlateRecognitionRecord(
            (String) row.get("record_id"),
            (String) row.get("site_id"),
            (String) row.get("space_id"),
            (String) row.get("camera_id"),
            (String) row.get("plate"),
            (String) row.get("plate_color"),
            (String) row.get("vehicle_type"),
            row.get("confidence") != null ? ((Number) row.get("confidence")).doubleValue() : 0.0,
            (String) row.get("snapshot_url"),
            (String) row.get("direction"),
            (String) row.get("judgment_status"),
            row.get("is_simulated") != null && (Boolean) row.get("is_simulated"),
            toInstant(row.get("recognized_at"))
        );
        recognitionFeed.add(rec);
      }

      for (Map<String, Object> row : jdbc.queryForList("SELECT * FROM chg_configuration_reports")) {
        ConfigurationApplyReport rep = new ConfigurationApplyReport(
            (String) row.get("node_id"),
            ((Number) row.get("config_version")).longValue(),
            (String) row.get("status"),
            (String) row.get("message"),
            toInstant(row.get("reported_at"))
        );
        configurationReports.put(rep.nodeId(), rep);
      }

      for (Map<String, Object> row : jdbc.queryForList("SELECT * FROM chg_upgrade_node_reports")) {
        UpgradeNodeReport rep = new UpgradeNodeReport(
            (String) row.get("node_id"),
            ((Number) row.get("current_version")).longValue(),
            row.get("target_version") != null ? ((Number) row.get("target_version")).longValue() : null,
            (String) row.get("state"),
            (String) row.get("message"),
            (String) row.get("last_error"),
            toInstant(row.get("updated_at"))
        );
        nodeUpgradeReports.put(rep.nodeId(), rep);
      }

      for (Map<String, Object> row : jdbc.queryForList("SELECT * FROM chg_pending_node_upgrades")) {
        String nId = (String) row.get("node_id");
        UpgradeManifest m = mapper.readValue((String) row.get("manifest_json"), UpgradeManifest.class);
        pendingNodeUpgrades.put(nId, m);
      }
    } catch (Exception error) {
      throw new IllegalStateException("load postgres data failed", error);
    }
  }

  private EdgeNodeTelemetry mapDbNodeRow(Map<String, Object> row, boolean fromHeartbeatOnly) {
    String nodeId = (String) row.get("node_id");
    String siteId = row.get("site_id") != null ? (String) row.get("site_id") : "default";
    String nodeName = fromHeartbeatOnly ? null : (String) row.get("node_name");
    String authKey = fromHeartbeatOnly ? null : (String) row.get("auth_key");
    String mIp = fromHeartbeatOnly ? null : (String) row.get("m_ip");
    String hIp = (String) row.get("h_ip");
    String ipAddress = (mIp != null && !mIp.isBlank()) ? mIp : (hIp != null ? hIp : "");

    List<String> assigned = new ArrayList<>();
    if (!fromHeartbeatOnly && row.get("assigned_spaces_json") instanceof String json && !json.isBlank()) {
      try {
        assigned = mapper.readValue(json, new TypeReference<List<String>>() {});
      } catch (Exception ignored) {}
    }

    EdgeNodeTelemetry tele = new EdgeNodeTelemetry();
    tele.setNodeId(nodeId);
    tele.setSiteId(siteId);
    tele.setNodeName(nodeName != null && !nodeName.isBlank() ? nodeName : nodeId);
    tele.setAuthKey(authKey != null ? authKey : "");
    tele.setIpAddress(ipAddress);
    tele.setAssignedSpaces(assigned);
    if (!fromHeartbeatOnly && row.get("reboot_requested_at") != null) {
      tele.setRebootRequestedAt(toInstant(row.get("reboot_requested_at")));
    }
    tele.setLastHeartbeat(toInstantNullable(row.get("last_heartbeat")));
    tele.setStatus(row.get("status") != null ? (String) row.get("status") : "OFFLINE");
    tele.setCpuUsage(row.get("cpu_usage") != null ? ((Number) row.get("cpu_usage")).doubleValue() : 0.0);
    tele.setMemoryUsage(row.get("memory_usage") != null ? ((Number) row.get("memory_usage")).doubleValue() : 0.0);
    tele.setTemperature(row.get("temperature") != null ? ((Number) row.get("temperature")).doubleValue() : 0.0);
    tele.setUptimeSeconds(row.get("uptime_seconds") != null ? ((Number) row.get("uptime_seconds")).longValue() : 0);
    tele.setFramesProcessed(row.get("frames_processed") != null ? ((Number) row.get("frames_processed")).longValue() : 0);
    tele.setViolationsDetected(row.get("violations_detected") != null ? ((Number) row.get("violations_detected")).longValue() : 0);
    tele.setAlarmsTriggered(row.get("alarms_triggered") != null ? ((Number) row.get("alarms_triggered")).longValue() : 0);
    tele.setDiskWatermark(row.get("disk_watermark") != null ? ((Number) row.get("disk_watermark")).doubleValue() : 0.0);
    tele.setCurrentVersion(row.get("version") != null ? (String) row.get("version") : "");
    return tele;
  }

  private Instant toInstant(Object value) {
    Instant inst = toInstantNullable(value);
    return inst != null ? inst : Instant.now();
  }

  private Instant toInstantNullable(Object value) {
    if (value == null) return null;
    if (value instanceof Instant inst) return inst;
    if (value instanceof Timestamp timestamp) return timestamp.toInstant();
    if (value instanceof java.time.OffsetDateTime dateTime) return dateTime.toInstant();
    if (value instanceof java.time.LocalDateTime ldt) return ldt.atZone(java.time.ZoneId.systemDefault()).toInstant();
    if (value instanceof java.util.Date d) return d.toInstant();
    return null;
  }

  private void loadFromDisk() {
    try {
      File cfgFile = new File(storageDir, "configuration.json");
      if (cfgFile.exists()) {
        configuration = mapper.readValue(cfgFile, PlatformConfiguration.class);
      }
      File evFile = new File(storageDir, "events.json");
      if (evFile.exists()) {
        List<ViolationEvent> list = mapper.readValue(evFile, new TypeReference<List<ViolationEvent>>() {});
        for (ViolationEvent ev : list) {
          events.put(ev.eventId(), ev);
        }
      }
      File wlFile = new File(storageDir, "whitelist.json");
      if (configuration.whitelist().isEmpty() && wlFile.exists()) {
        List<WhitelistRecord> list = mapper.readValue(wlFile, new TypeReference<List<WhitelistRecord>>() {});
        configuration = new PlatformConfiguration(
            configuration.version(), configuration.updatedAt(), configuration.rules(),
            configuration.bindings(), list, configuration.cameras());
        persistConfiguration(configuration);
      }
      File auditFile = new File(storageDir, "audit.jsonl");
      if (auditFile.exists()) {
        for (String line : Files.readAllLines(auditFile.toPath())) {
          if (!line.isBlank()) audits.add(mapper.readValue(line, OperationAuditRecord.class));
        }
      }
      File sitesFile = new File(storageDir, "sites.json");
      if (sitesFile.exists()) {
        sitesCache.putAll(mapper.readValue(sitesFile, new TypeReference<Map<String, Site>>() {}));
      }
      File siteConfigurationsFile = new File(storageDir, "site-configurations.json");
      if (siteConfigurationsFile.exists()) {
        configurationBySite.putAll(mapper.readValue(siteConfigurationsFile,
            new TypeReference<Map<String, PlatformConfiguration>>() {}));
      }
      File nodeSitesFile = new File(storageDir, "node-sites.json");
      if (nodeSitesFile.exists()) {
        nodeToSite.putAll(mapper.readValue(nodeSitesFile, new TypeReference<Map<String, String>>() {}));
      }
      File camerasFile = new File(storageDir, "cameras.json");
      if (camerasFile.exists()) {
        camerasBySite.putAll(mapper.readValue(camerasFile,
            new TypeReference<Map<String, List<CameraConfig>>>() {}));
      }
      File nodesFile = new File(storageDir, "nodes.json");
      if (nodesFile.exists()) {
        edgeNodesTelemetry.putAll(mapper.readValue(nodesFile,
            new TypeReference<Map<String, EdgeNodeTelemetry>>() {}));
        for (EdgeNodeTelemetry node : edgeNodesTelemetry.values()) {
          if (node.getLastHeartbeat() != null) {
            heartbeats.putIfAbsent(node.getNodeId(), node.getLastHeartbeat());
          }
          if (node.getSiteId() != null && !node.getSiteId().isBlank()) {
            nodeToSite.putIfAbsent(node.getNodeId(), node.getSiteId());
          }
        }
      }
      File protocolsFile = new File(storageDir, "protocols.json");
      if (protocolsFile.exists()) {
        protocolsCache.putAll(mapper.readValue(protocolsFile,
            new TypeReference<Map<String, ChargerProtocol>>() {}));
      }
      File statusesFile = new File(storageDir, "space_statuses.json");
      if (statusesFile.exists()) {
        spaceStatuses.putAll(mapper.readValue(statusesFile,
            new TypeReference<Map<String, SpaceRuntimeStatus>>() {}));
      }
      File lprFile = new File(storageDir, "lpr_records.json");
      if (lprFile.exists()) {
        List<PlateRecognitionRecord> recs = mapper.readValue(lprFile,
            new TypeReference<List<PlateRecognitionRecord>>() {});
        recognitionFeed.addAll(recs);
      }
      File cfgRepFile = new File(storageDir, "config_reports.json");
      if (cfgRepFile.exists()) {
        configurationReports.putAll(mapper.readValue(cfgRepFile,
            new TypeReference<Map<String, ConfigurationApplyReport>>() {}));
      }
      File upRepFile = new File(storageDir, "upgrade_reports.json");
      if (upRepFile.exists()) {
        nodeUpgradeReports.putAll(mapper.readValue(upRepFile,
            new TypeReference<Map<String, UpgradeNodeReport>>() {}));
      }
      File pendingUpFile = new File(storageDir, "pending_upgrades.json");
      if (pendingUpFile.exists()) {
        pendingNodeUpgrades.putAll(mapper.readValue(pendingUpFile,
            new TypeReference<Map<String, UpgradeManifest>>() {}));
      }
      if (!sitesCache.containsKey("default")) {
        sitesCache.put("default", new Site("default", "默认停车场", "", true, Instant.now()));
      }
    } catch (Exception error) { throw new IllegalStateException("load file storage failed", error); }
  }

  // ====== 多边缘终端管理与硬件遥测 ======

  public List<EdgeNodeTelemetry> getNodesForSite(String siteId) {
    String effectiveSiteId = (siteId == null || siteId.isBlank()) ? "default" : siteId;
    List<EdgeNodeTelemetry> result = new ArrayList<>();

    for (EdgeNodeTelemetry node : edgeNodesTelemetry.values()) {
      if (effectiveSiteId.equals(node.getSiteId())) {
        Instant hb = heartbeats.get(node.getNodeId());
        if (hb != null) {
          node.setLastHeartbeat(hb);
          boolean online = ChronoUnit.SECONDS.between(hb, Instant.now()) < 45;
          if (!"REBOOTING".equals(node.getStatus()) && !"UPGRADING".equals(node.getStatus())) {
            node.setStatus(online ? "ONLINE" : "OFFLINE");
          }
        }
        result.add(node);
      }
    }
    return result;
  }

  public boolean rebootNode(String nodeId, String operator) {
    Instant now = Instant.now();
    EdgeNodeTelemetry node = edgeNodesTelemetry.get(nodeId);
    if (node != null) {
      node.setStatus("REBOOTING");
      node.setRebootRequestedAt(now);
    }
    if (databaseMode) {
      jdbc.update("UPDATE chg_node_site_mapping SET reboot_requested_at = ? WHERE node_id = ?",
          Timestamp.from(now), nodeId);
    } else {
      persistNodesToDisk();
    }
    audit(operator != null ? operator : "ADMIN", "NODE_REBOOT", "node:" + nodeId, "下发远程硬件重启指令");
    return true;
  }

  public boolean upgradeSingleNode(String nodeId, int version, String packageUrl, String sha256, String operator) {
    EdgeNodeTelemetry node = edgeNodesTelemetry.get(nodeId);
    if (node != null) {
      node.setStatus("UPGRADING");
    }
    long curVer = 1L;
    if (node != null && node.getCurrentVersion() != null) {
      try {
        String num = node.getCurrentVersion().replaceAll("[^0-9]", "");
        if (!num.isEmpty()) curVer = Long.parseLong(num);
      } catch (Exception ignored) {}
    }
    UpgradeNodeReport report = new UpgradeNodeReport(
        nodeId, curVer, (long) version, "UPGRADING", "平台已下发定向升级指令，等待工控机同步拉取固件",
        null, Instant.now()
    );
    saveNodeUpgradeReport(nodeId, report);

    UpgradeManifest latest = latestManifest.get();
    String effectivePackageUrl = (packageUrl != null && !packageUrl.isBlank())
        ? packageUrl
        : (latest != null ? latest.packageUrl() : "");
    String effectiveSha256 = (sha256 != null && !sha256.isBlank())
        ? sha256
        : (latest != null ? latest.sha256Hash() : "");

    String sig = calculateManifestSignature(version, effectiveSha256, effectivePackageUrl);
    UpgradeManifest targetedManifest = new UpgradeManifest(
        (long) version,
        "v" + version + ".0.0-targeted",
        effectivePackageUrl,
        effectiveSha256,
        sig,
        1L,
        "平台管理员 (" + (operator != null ? operator : "ADMIN") + ") 定向推送版本",
        Instant.now()
    );
    pendingNodeUpgrades.put(nodeId, targetedManifest);
    if (databaseMode) {
      try {
        String json = mapper.writeValueAsString(targetedManifest);
        jdbc.update(
            "INSERT INTO chg_pending_node_upgrades(node_id,manifest_json,created_at) VALUES(?,?,?) " +
            "ON CONFLICT(node_id) DO UPDATE SET manifest_json=EXCLUDED.manifest_json,created_at=EXCLUDED.created_at",
            nodeId, json, Timestamp.from(Instant.now()));
      } catch (Exception e) {
        throw new IllegalStateException("persist pending node upgrade failed", e);
      }
    } else {
      persistMapJson("pending_upgrades.json", pendingNodeUpgrades);
    }

    audit(operator != null ? operator : "ADMIN", "NODE_UPGRADE", "node:" + nodeId,
        "单台定向推送固件版本 v" + version + " (URL: " + effectivePackageUrl + ")");
    return true;
  }

  public synchronized EdgeNodeTelemetry addOrUpdateNode(String siteId, EdgeNodeTelemetry node) {
    String effectiveSiteId = (siteId == null || siteId.isBlank()) ? "default" : siteId;
    node.setSiteId(effectiveSiteId);
    if (node.getNodeName() == null || node.getNodeName().isBlank()) {
      node.setNodeName(node.getNodeId());
    }
    if (node.getAuthKey() == null || node.getAuthKey().isBlank()) {
      node.setAuthKey("edge-key-" + UUID.randomUUID().toString().replace("-", "").substring(0, 12));
    }
    if (node.getStatus() == null || node.getStatus().isBlank()) {
      node.setStatus(heartbeats.containsKey(node.getNodeId()) ? "ONLINE" : "OFFLINE");
    }
    if (node.getAssignedSpaces() == null) {
      node.setAssignedSpaces(new ArrayList<>());
    }
    edgeNodesTelemetry.put(node.getNodeId(), node);
    nodeToSite.put(node.getNodeId(), effectiveSiteId);
    if (databaseMode) {
      try {
        String spacesJson = mapper.writeValueAsString(node.getAssignedSpaces());
        jdbc.update(
            "INSERT INTO chg_node_site_mapping(node_id, site_id, node_name, auth_key, ip_address, assigned_spaces_json, updated_at) " +
            "VALUES(?,?,?,?,?,?,?) ON CONFLICT(node_id) DO UPDATE SET site_id=EXCLUDED.site_id, node_name=EXCLUDED.node_name, auth_key=EXCLUDED.auth_key, ip_address=EXCLUDED.ip_address, assigned_spaces_json=EXCLUDED.assigned_spaces_json, updated_at=EXCLUDED.updated_at",
            node.getNodeId(), effectiveSiteId, node.getNodeName(), node.getAuthKey(),
            node.getIpAddress() != null ? node.getIpAddress() : "", spacesJson, Timestamp.from(Instant.now()));
      } catch (Exception e) {
        throw new IllegalStateException("persist edge node failed", e);
      }
    } else {
      persistMultiSiteState();
      persistNodesToDisk();
    }
    return node;
  }

  public synchronized boolean deleteNode(String siteId, String nodeId) {
    edgeNodesTelemetry.remove(nodeId);
    nodeToSite.remove(nodeId);
    heartbeats.remove(nodeId);
    configurationReports.remove(nodeId);
    nodeUpgradeReports.remove(nodeId);
    pendingNodeUpgrades.remove(nodeId);
    if (databaseMode) {
      jdbc.update("DELETE FROM chg_node_heartbeats WHERE node_id = ?", nodeId);
      jdbc.update("DELETE FROM chg_node_site_mapping WHERE node_id = ?", nodeId);
      jdbc.update("DELETE FROM chg_configuration_reports WHERE node_id = ?", nodeId);
      jdbc.update("DELETE FROM chg_upgrade_node_reports WHERE node_id = ?", nodeId);
      jdbc.update("DELETE FROM chg_pending_node_upgrades WHERE node_id = ?", nodeId);
      jdbc.update("UPDATE chg_cameras SET node_id = NULL WHERE node_id = ?", nodeId);
    } else {
      persistMultiSiteState();
      persistNodesToDisk();
      persistCamerasToDisk();
    }
    return true;
  }

  // ====== 摄像头接入管理 ======

  public List<CameraConfig> getCamerasForSite(String siteId) {
    String effectiveSiteId = (siteId == null || siteId.isBlank()) ? "default" : siteId;
    if (databaseMode) {
      List<CameraConfig> dbList = jdbc.query(
          "SELECT * FROM chg_cameras WHERE site_id = ? ORDER BY camera_id",
          (rs, rowNum) -> new CameraConfig(
              rs.getString("camera_id"),
              rs.getString("camera_name"),
              rs.getString("site_id"),
              rs.getString("space_id"),
              rs.getString("node_id"),
              rs.getString("protocol"),
              rs.getString("stream_url"),
              rs.getString("sip_device_id"),
              rs.getString("resolution"),
              rs.getString("codec"),
              rs.getString("status"),
              rs.getTimestamp("last_frame_at") == null ? null : rs.getTimestamp("last_frame_at").toInstant(),
              rs.getString("roi_json"),
              rs.getObject("enabled") == null ? Boolean.TRUE : rs.getBoolean("enabled")
          ),
          effectiveSiteId);
      Instant now = Instant.now();
      for (CameraConfig cam : dbList) {
        if (cam.getLastFrameAt() != null && java.time.temporal.ChronoUnit.SECONDS.between(cam.getLastFrameAt(), now) > 45) {
          if ("ONLINE".equals(cam.getStatus())) {
            cam.setStatus("OFFLINE");
          }
        }
      }
      camerasBySite.put(effectiveSiteId, new ArrayList<>(dbList));
      return dbList;
    }
    List<CameraConfig> memList = camerasBySite.computeIfAbsent(effectiveSiteId, k -> new ArrayList<>());
    Instant now = Instant.now();
    for (CameraConfig cam : memList) {
      if (cam.getLastFrameAt() != null && java.time.temporal.ChronoUnit.SECONDS.between(cam.getLastFrameAt(), now) > 45) {
        if ("ONLINE".equals(cam.getStatus())) {
          cam.setStatus("OFFLINE");
        }
      }
    }
    return List.copyOf(memList);
  }

  public synchronized CameraConfig saveCamera(String siteId, CameraConfig camera) {
    if (camera == null) {
      throw new IllegalArgumentException("camera cannot be null");
    }
    if (camera.getCameraId() == null || camera.getCameraId().isBlank()) {
      throw new IllegalArgumentException("cameraId cannot be empty");
    }
    String effectiveSiteId = (siteId == null || siteId.isBlank()) ? "default" : siteId;
    if (!sitesCache.containsKey(effectiveSiteId) && !"default".equals(effectiveSiteId)) {
      throw new NoSuchElementException("site not found: " + effectiveSiteId);
    }

    if (camera.getCameraName() == null || camera.getCameraName().isBlank()) {
      camera.setCameraName(camera.getCameraId());
    }
    camera.setSiteId(effectiveSiteId);
    if (camera.getStatus() == null || camera.getStatus().isBlank()) {
      camera.setStatus("REGISTERED");
    }
    if (camera.getProtocol() == null || camera.getProtocol().isBlank()) {
      camera.setProtocol("RTSP");
    }
    if (camera.getResolution() == null || camera.getResolution().isBlank()) {
      camera.setResolution("1920x1080");
    }
    if (camera.getCodec() == null || camera.getCodec().isBlank()) {
      camera.setCodec("H.264");
    }
    if (camera.getStreamUrl() == null) camera.setStreamUrl("");
    if (camera.getSipDeviceId() == null) camera.setSipDeviceId("");
    if (camera.getSpaceId() == null) camera.setSpaceId("");
    if (camera.getNodeId() == null) camera.setNodeId("");
    if (camera.getRoiJson() == null) camera.setRoiJson("");
    if (camera.getEnabled() == null) camera.setEnabled(Boolean.TRUE);

    List<CameraConfig> list = camerasBySite.computeIfAbsent(effectiveSiteId, k -> new ArrayList<>());
    list.removeIf(c -> c.getCameraId().equalsIgnoreCase(camera.getCameraId()));
    list.add(camera);
    if (databaseMode) {
      jdbc.update(
          "INSERT INTO chg_cameras(camera_id, site_id, camera_name, node_id, space_id, protocol, stream_url, sip_device_id, resolution, codec, status, last_frame_at, roi_json, enabled) " +
          "VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?) " +
          "ON CONFLICT(camera_id, site_id) DO UPDATE SET camera_name=EXCLUDED.camera_name, node_id=EXCLUDED.node_id, space_id=EXCLUDED.space_id, protocol=EXCLUDED.protocol, stream_url=EXCLUDED.stream_url, sip_device_id=EXCLUDED.sip_device_id, resolution=EXCLUDED.resolution, codec=EXCLUDED.codec, status=EXCLUDED.status, last_frame_at=EXCLUDED.last_frame_at, roi_json=EXCLUDED.roi_json, enabled=EXCLUDED.enabled",
          camera.getCameraId(), effectiveSiteId, camera.getCameraName(), camera.getNodeId(), camera.getSpaceId(),
          camera.getProtocol(), camera.getStreamUrl(), camera.getSipDeviceId(), camera.getResolution(),
          camera.getCodec(), camera.getStatus(),
          camera.getLastFrameAt() != null ? Timestamp.from(camera.getLastFrameAt()) : null,
          camera.getRoiJson(), camera.getEnabled());
    } else {
      persistCamerasToDisk();
    }

    // 自动双向同步车位绑定：若该摄像头指定了车位，更新该车位绑定的 cameraId
    if (camera.getSpaceId() != null && !camera.getSpaceId().isBlank()) {
      List<DeviceBinding> bindings = getBindingsForSite(effectiveSiteId);
      boolean bindingChanged = false;
      List<DeviceBinding> updatedBindings = new ArrayList<>();
      for (DeviceBinding b : bindings) {
        if (b.spaceId().equalsIgnoreCase(camera.getSpaceId())) {
          if (!b.cameraId().equalsIgnoreCase(camera.getCameraId())) {
            updatedBindings.add(new DeviceBinding(
                b.siteId(), b.spaceId(), camera.getCameraId(), b.chargerId(),
                b.relayChannel(), b.speakerChannel(), b.enabled(),
                b.chargerBrand(), b.protocolId(), b.connectorId(), b.chargerParams()
            ));
            bindingChanged = true;
          } else {
            updatedBindings.add(b);
          }
        } else {
          updatedBindings.add(b);
        }
      }
      if (bindingChanged) {
        saveBindingsForSite(effectiveSiteId, updatedBindings);
      }
    }

    bumpSiteConfigVersion(effectiveSiteId);
    return camera;
  }

  public synchronized boolean deleteCamera(String siteId, String cameraId) {
    String effectiveSiteId = (siteId == null || siteId.isBlank()) ? "default" : siteId;
    List<CameraConfig> list = camerasBySite.get(effectiveSiteId);
    boolean removed = list != null && list.removeIf(c -> c.getCameraId().equalsIgnoreCase(cameraId));
    if (databaseMode) {
      int rows = jdbc.update("DELETE FROM chg_cameras WHERE camera_id = ? AND site_id = ?", cameraId, effectiveSiteId);
      removed = removed || rows > 0;
    } else if (removed) {
      persistCamerasToDisk();
    }
    if (removed) {
      bumpSiteConfigVersion(effectiveSiteId);
    }
    return removed;
  }

  public Map<String, Object> testCamera(String siteId, String cameraId) {
    String effectiveSiteId = (siteId == null || siteId.isBlank()) ? "default" : siteId;
    List<CameraConfig> list = getCamerasForSite(effectiveSiteId);
    CameraConfig camera = list.stream()
        .filter(c -> c.getCameraId().equalsIgnoreCase(cameraId))
        .findFirst()
        .orElse(null);
    if (camera == null) {
      return Map.of("success", false, "connected", false, "error", "找不到摄像头: " + cameraId);
    }

    String protocol = camera.getProtocol() != null ? camera.getProtocol().toUpperCase() : "RTSP";
    if ("GB28181".equals(protocol)) {
      return probeGb28181Camera(effectiveSiteId, camera);
    } else {
      return probeRtspCamera(effectiveSiteId, camera);
    }
  }

  private Map<String, Object> probeRtspCamera(String siteId, CameraConfig camera) {
    String url = camera.getStreamUrl();
    if (url == null || url.isBlank()) {
      updateCameraStatus(siteId, camera.getCameraId(), "OFFLINE", null, null);
      return Map.of(
          "success", false,
          "connected", false,
          "cameraId", camera.getCameraId(),
          "protocol", "RTSP",
          "targetHost", "--",
          "status", "OFFLINE",
          "error", "摄像头未配置有效的 RTSP 拉流地址 (URL 为空)"
      );
    }

    String raw = url.trim();
    if (raw.toLowerCase().startsWith("rtsp://")) raw = raw.substring(7);
    else if (raw.toLowerCase().startsWith("rtsps://")) raw = raw.substring(8);

    String username = "";
    String password = "";
    int atIdx = raw.indexOf('@');
    if (atIdx >= 0) {
      String userPass = raw.substring(0, atIdx);
      raw = raw.substring(atIdx + 1);
      int colonIdx = userPass.indexOf(':');
      if (colonIdx >= 0) {
        username = userPass.substring(0, colonIdx);
        password = userPass.substring(colonIdx + 1);
      } else {
        username = userPass;
      }
    }

    String hostPort = raw;
    String path = "/";
    int slashIdx = raw.indexOf('/');
    if (slashIdx >= 0) {
      hostPort = raw.substring(0, slashIdx);
      path = raw.substring(slashIdx);
    }

    String host = hostPort;
    int port = 554;
    int colonIdx = hostPort.indexOf(':');
    if (colonIdx >= 0) {
      host = hostPort.substring(0, colonIdx);
      try {
        port = Integer.parseInt(hostPort.substring(colonIdx + 1));
      } catch (NumberFormatException ignored) {}
    }

    long start = System.currentTimeMillis();
    try (Socket socket = new Socket()) {
      socket.connect(new InetSocketAddress(host, port), 2500);
      socket.setSoTimeout(3000);

      java.io.OutputStream out = socket.getOutputStream();
      java.io.BufferedReader reader = new java.io.BufferedReader(
          new java.io.InputStreamReader(socket.getInputStream(), StandardCharsets.ISO_8859_1));

      String uri = "rtsp://" + host + ":" + port + (path.startsWith("/") ? path : "/" + path);
      String req1 = "DESCRIBE " + uri + " RTSP/1.0\r\n"
          + "CSeq: 1\r\n"
          + "User-Agent: ChongDian-Prober/2.0\r\n"
          + "Accept: application/sdp\r\n\r\n";
      out.write(req1.getBytes(StandardCharsets.UTF_8));
      out.flush();

      String statusLine1 = reader.readLine();
      if (statusLine1 == null) {
        String advice = checkAndAdviseAlternativeRtspPort(host, port);
        updateCameraStatus(siteId, camera.getCameraId(), "OFFLINE", null, null);
        return Map.of(
            "success", false,
            "connected", false,
            "cameraId", camera.getCameraId(),
            "protocol", "RTSP",
            "targetHost", host + ":" + port,
            "status", "OFFLINE",
            "error", "TCP连接已建立，但目标端口未响应RTSP协议。" + advice
        );
      }

      Map<String, String> headers1 = new HashMap<>();
      String line;
      while ((line = reader.readLine()) != null && !line.isBlank()) {
        int c = line.indexOf(':');
        if (c > 0) headers1.put(line.substring(0, c).trim().toLowerCase(), line.substring(c + 1).trim());
      }

      String sdpContent = "";
      String finalStatus = statusLine1;

      if (statusLine1.contains("401") && !username.isBlank()) {
        String authHeader = headers1.get("www-authenticate");
        if (authHeader != null && authHeader.toLowerCase().contains("digest")) {
          String realm = extractQuotedValue(authHeader, "realm");
          String nonce = extractQuotedValue(authHeader, "nonce");
          if (realm != null && nonce != null) {
            String ha1 = md5(username + ":" + realm + ":" + password);
            String ha2 = md5("DESCRIBE:" + uri);
            String responseHash = md5(ha1 + ":" + nonce + ":" + ha2);
            String digestAuth = "Digest username=\"" + username + "\", realm=\"" + realm
                + "\", nonce=\"" + nonce + "\", uri=\"" + uri + "\", response=\"" + responseHash + "\"";

            String req2 = "DESCRIBE " + uri + " RTSP/1.0\r\n"
                + "CSeq: 2\r\n"
                + "User-Agent: ChongDian-Prober/2.0\r\n"
                + "Accept: application/sdp\r\n"
                + "Authorization: " + digestAuth + "\r\n\r\n";
            out.write(req2.getBytes(StandardCharsets.UTF_8));
            out.flush();

            String statusLine2 = reader.readLine();
            if (statusLine2 != null) {
              finalStatus = statusLine2;
              Map<String, String> headers2 = new HashMap<>();
              while ((line = reader.readLine()) != null && !line.isBlank()) {
                int c = line.indexOf(':');
                if (c > 0) headers2.put(line.substring(0, c).trim().toLowerCase(), line.substring(c + 1).trim());
              }
              if (headers2.containsKey("content-length")) {
                int len = Integer.parseInt(headers2.get("content-length"));
                char[] buf = new char[len];
                int readLen = reader.read(buf, 0, len);
                if (readLen > 0) sdpContent = new String(buf, 0, readLen);
              }
            }
          }
        }
      } else if (statusLine1.contains("200") && headers1.containsKey("content-length")) {
        int len = Integer.parseInt(headers1.get("content-length"));
        char[] buf = new char[len];
        int readLen = reader.read(buf, 0, len);
        if (readLen > 0) sdpContent = new String(buf, 0, readLen);
      }

      long totalLatency = System.currentTimeMillis() - start;

      if (finalStatus.contains("200")) {
        String detectedCodec = "H.264";
        String detectedFps = "--";
        if (sdpContent.toLowerCase().contains("h265") || sdpContent.toLowerCase().contains("hevc")) {
          detectedCodec = "H.265";
        } else if (sdpContent.toLowerCase().contains("h264")) {
          detectedCodec = "H.264";
        }
        Matcher mFps = Pattern.compile("framerate:([0-9.]+)").matcher(sdpContent);
        if (mFps.find()) {
          detectedFps = mFps.group(1);
          if (detectedFps.endsWith(".000000")) detectedFps = detectedFps.substring(0, detectedFps.indexOf('.'));
        }

        updateCameraStatus(siteId, camera.getCameraId(), "ONLINE", detectedCodec, null);

        Map<String, Object> res = new LinkedHashMap<>();
        res.put("success", true);
        res.put("connected", true);
        res.put("cameraId", camera.getCameraId());
        res.put("protocol", "RTSP");
        res.put("targetHost", host + ":" + port);
        res.put("latencyMs", totalLatency);
        res.put("status", "ONLINE");
        res.put("rtspStatus", finalStatus.trim());
        res.put("fps", detectedFps);
        res.put("resolution", camera.getResolution() != null ? camera.getResolution() : "1920x1080");
        res.put("codec", detectedCodec);
        res.put("authResult", username.isBlank() ? "免密直连" : "密码鉴权成功");
        res.put("message", "RTSP协议握手与流描述获取成功，网络延时 " + totalLatency + "ms，视频编码: " + detectedCodec
            + (detectedFps.equals("--") ? "" : " (" + detectedFps + " FPS)"));
        res.put("testedAt", Instant.now());
        return res;
      } else if (finalStatus.contains("401")) {
        updateCameraStatus(siteId, camera.getCameraId(), "OFFLINE", null, null);
        return Map.of(
            "success", false,
            "connected", true,
            "cameraId", camera.getCameraId(),
            "protocol", "RTSP",
            "targetHost", host + ":" + port,
            "latencyMs", totalLatency,
            "status", "OFFLINE",
            "rtspStatus", finalStatus.trim(),
            "error", "RTSP网络已通但鉴权失败 (401 Unauthorized): 请检查填写的摄像头账号或密码是否正确"
        );
      } else {
        updateCameraStatus(siteId, camera.getCameraId(), "OFFLINE", null, null);
        return Map.of(
            "success", false,
            "connected", true,
            "cameraId", camera.getCameraId(),
            "protocol", "RTSP",
            "targetHost", host + ":" + port,
            "latencyMs", totalLatency,
            "status", "OFFLINE",
            "rtspStatus", finalStatus.trim(),
            "error", "RTSP服务端返回非200状态码: " + finalStatus.trim()
        );
      }
    } catch (java.net.SocketTimeoutException e) {
      String advice = checkAndAdviseAlternativeRtspPort(host, port);
      updateCameraStatus(siteId, camera.getCameraId(), "OFFLINE", null, null);
      return Map.of(
          "success", false,
          "connected", false,
          "cameraId", camera.getCameraId(),
          "protocol", "RTSP",
          "targetHost", host + ":" + port,
          "status", "OFFLINE",
          "error", "连接超时(>2500ms): 目标 " + host + ":" + port + " 无响应。" + advice
      );
    } catch (java.net.ConnectException e) {
      String advice = checkAndAdviseAlternativeRtspPort(host, port);
      updateCameraStatus(siteId, camera.getCameraId(), "OFFLINE", null, null);
      return Map.of(
          "success", false,
          "connected", false,
          "cameraId", camera.getCameraId(),
          "protocol", "RTSP",
          "targetHost", host + ":" + port,
          "status", "OFFLINE",
          "error", "连接被拒绝 (Connection Refused): 目标 " + host + ":" + port + " 端口未开放。" + advice
      );
    } catch (Exception e) {
      updateCameraStatus(siteId, camera.getCameraId(), "OFFLINE", null, null);
      return Map.of(
          "success", false,
          "connected", false,
          "cameraId", camera.getCameraId(),
          "protocol", "RTSP",
          "targetHost", host + ":" + port,
          "status", "OFFLINE",
          "error", "拉流探测失败: " + (e.getMessage() != null ? e.getMessage() : e.getClass().getSimpleName())
      );
    }
  }

  private String checkAndAdviseAlternativeRtspPort(String host, int currentPort) {
    if (currentPort == 37777) {
      try (Socket s = new Socket()) {
        s.connect(new InetSocketAddress(host, 554), 1000);
        return "\n\n【排查诊断提示】37777 为大华私有SDK端口，系统检测到 554 标准 RTSP 端口已开放，请改用 554 端口！";
      } catch (Exception ignored) {}
    } else if (currentPort == 8000) {
      try (Socket s = new Socket()) {
        s.connect(new InetSocketAddress(host, 554), 1000);
        return "\n\n【排查诊断提示】8000 为海康私有SDK端口，系统检测到 554 标准 RTSP 端口已开放，请改用 554 端口！";
      } catch (Exception ignored) {}
    }
    return "\n\n请检查摄像头电源供电、网线连接及局域网路由设置。";
  }

  private Map<String, Object> probeGb28181Camera(String siteId, CameraConfig camera) {
    String sipId = camera.getSipDeviceId();
    if (sipId == null || sipId.isBlank()) {
      updateCameraStatus(siteId, camera.getCameraId(), "OFFLINE", null, null);
      return Map.of(
          "success", false, "connected", false,
          "cameraId", camera.getCameraId(),
          "protocol", "GB28181",
          "status", "OFFLINE",
          "error", "未配置GB28181国标20位设备编码"
      );
    }
    if (!sipId.matches("^\\d{20}$")) {
      updateCameraStatus(siteId, camera.getCameraId(), "OFFLINE", null, null);
      return Map.of(
          "success", false, "connected", false,
          "cameraId", camera.getCameraId(),
          "protocol", "GB28181",
          "status", "OFFLINE",
          "error", "GB28181国标编码格式非法(必须为20位纯数字)"
      );
    }
    boolean active = camera.getLastFrameAt() != null
        && ChronoUnit.SECONDS.between(camera.getLastFrameAt(), Instant.now()) <= 60;
    String status = active ? "ONLINE" : "OFFLINE";
    updateCameraStatus(siteId, camera.getCameraId(), status, null, null);
    return Map.of(
        "success", active,
        "connected", active,
        "cameraId", camera.getCameraId(),
        "protocol", "GB28181",
        "sipDeviceId", sipId,
        "status", status,
        "error", active ? "" : "GB28181国标设备离线: 最近 60 秒内尚未收到来自边缘 SIP 网关的视频帧心跳"
    );
  }

  public synchronized void updateCameraStatus(
      String siteId, String cameraId, String status, String codec, String resolution) {
    String effectiveSiteId = (siteId == null || siteId.isBlank()) ? "default" : siteId;
    Instant now = Instant.now();
    List<CameraConfig> list = camerasBySite.get(effectiveSiteId);
    if (list != null) {
      for (CameraConfig c : list) {
        if (c.getCameraId().equalsIgnoreCase(cameraId)) {
          c.setStatus(status);
          if (codec != null && !codec.isBlank()) c.setCodec(codec);
          if (resolution != null && !resolution.isBlank()) c.setResolution(resolution);
          if ("ONLINE".equalsIgnoreCase(status)) {
            c.setLastFrameAt(now);
          }
          break;
        }
      }
    }
    if (databaseMode) {
      if ("ONLINE".equalsIgnoreCase(status)) {
        jdbc.update(
            "UPDATE chg_cameras SET status = ?, codec = COALESCE(NULLIF(?, ''), codec), resolution = COALESCE(NULLIF(?, ''), resolution), last_frame_at = ? WHERE camera_id = ? AND site_id = ?",
            status, codec != null ? codec : "", resolution != null ? resolution : "",
            Timestamp.from(now), cameraId, effectiveSiteId);
      } else {
        jdbc.update(
            "UPDATE chg_cameras SET status = ?, codec = COALESCE(NULLIF(?, ''), codec), resolution = COALESCE(NULLIF(?, ''), resolution) WHERE camera_id = ? AND site_id = ?",
            status, codec != null ? codec : "", resolution != null ? resolution : "",
            cameraId, effectiveSiteId);
      }
    } else {
      persistCamerasToDisk();
    }
  }

  private String md5(String input) {
    try {
      MessageDigest md = MessageDigest.getInstance("MD5");
      byte[] digest = md.digest(input.getBytes(StandardCharsets.UTF_8));
      return HexFormat.of().formatHex(digest);
    } catch (Exception e) {
      throw new RuntimeException(e);
    }
  }

  private String extractQuotedValue(String header, String key) {
    Matcher m = Pattern.compile(key + "=\"([^\"]+)\"").matcher(header);
    return m.find() ? m.group(1) : null;
  }

  // ====== 车位与声光联动设备绑定管理 ======

  public List<DeviceBinding> getBindingsForSite(String siteId) {
    String effectiveSiteId = (siteId == null || siteId.isBlank()) ? "default" : siteId;
    PlatformConfiguration config = siteConfiguration(effectiveSiteId);
    return config != null && config.bindings() != null ? config.bindings() : Collections.emptyList();
  }

  public synchronized List<DeviceBinding> saveBindingsForSite(String siteId, List<DeviceBinding> bindings) {
    String effectiveSiteId = (siteId == null || siteId.isBlank()) ? "default" : siteId;
    PlatformConfiguration current = siteConfiguration(effectiveSiteId);
    PlatformConfiguration next = new PlatformConfiguration(
        current.version() + 1,
        Instant.now(),
        current.rules(),
        bindings,
        current.whitelist(),
        getCamerasForSite(effectiveSiteId)
    );
    updateSiteConfiguration(effectiveSiteId, next);
    return next.bindings();
  }

  public synchronized DeviceBinding saveSingleBinding(String siteId, DeviceBinding binding) {
    String effectiveSiteId = (siteId == null || siteId.isBlank()) ? "default" : siteId;
    List<DeviceBinding> currentBindings = new ArrayList<>(getBindingsForSite(effectiveSiteId));
    currentBindings.removeIf(b -> b.spaceId().equalsIgnoreCase(binding.spaceId()));
    currentBindings.add(binding);
    saveBindingsForSite(effectiveSiteId, currentBindings);

    // 自动双向同步关联摄像头：将该摄像头的 spaceId 同步设为该车位
    if (binding.cameraId() != null && !binding.cameraId().isBlank()) {
      List<CameraConfig> cams = camerasBySite.get(effectiveSiteId);
      if (cams != null) {
        for (CameraConfig cam : cams) {
          if (binding.cameraId().equalsIgnoreCase(cam.getCameraId())) {
            if (!binding.spaceId().equalsIgnoreCase(cam.getSpaceId())) {
              cam.setSpaceId(binding.spaceId());
              if (databaseMode) {
                jdbc.update("UPDATE chg_cameras SET space_id = ? WHERE camera_id = ? AND site_id = ?",
                    binding.spaceId(), cam.getCameraId(), effectiveSiteId);
              }
            }
          }
        }
      }
    }
    return binding;
  }

  public synchronized boolean deleteBinding(String siteId, String spaceId) {
    String effectiveSiteId = (siteId == null || siteId.isBlank()) ? "default" : siteId;
    List<DeviceBinding> currentBindings = new ArrayList<>(getBindingsForSite(effectiveSiteId));
    boolean removed = currentBindings.removeIf(b -> b.spaceId().equalsIgnoreCase(spaceId));
    if (removed) {
      saveBindingsForSite(effectiveSiteId, currentBindings);
      // 解绑时，将原绑定的摄像头 spaceId 同步置空
      List<CameraConfig> cams = camerasBySite.get(effectiveSiteId);
      if (cams != null) {
        for (CameraConfig cam : cams) {
          if (spaceId.equalsIgnoreCase(cam.getSpaceId())) {
            cam.setSpaceId("");
            if (databaseMode) {
              jdbc.update("UPDATE chg_cameras SET space_id = '' WHERE camera_id = ? AND site_id = ?",
                  cam.getCameraId(), effectiveSiteId);
            }
          }
        }
      }
    }
    return removed;
  }

  // ====== AI 智能视觉与车牌感知流水、抓拍与取证文件存储 ======

  public synchronized void addRecognitionRecords(String nodeId, List<PlateRecognitionRecord> records) {
    if (records == null || records.isEmpty()) return;
    String siteId = siteIdForNode(nodeId);
    for (PlateRecognitionRecord rec : records) {
      if (rec.getRecordId() == null || rec.getRecordId().isBlank()) {
        rec.setRecordId(UUID.randomUUID().toString());
      }
      rec.setSiteId(siteId);
      if (rec.getRecognizedAt() == null) {
        rec.setRecognizedAt(Instant.now());
      }
      if ((rec.getSnapshotUrl() == null || rec.getSnapshotUrl().isBlank())
          && rec.getSpaceId() != null
          && getSpaceSnapshotFile(siteId, rec.getSpaceId()) != null) {
        rec.setSnapshotUrl(buildSnapshotUrl(siteId, rec.getSpaceId()));
      }
      if (rec.getPlateColor() == null || rec.getPlateColor().isBlank()) {
        rec.setPlateColor("NEW_ENERGY".equalsIgnoreCase(rec.getVehicleType()) ? "GREEN"
            : ("FUEL".equalsIgnoreCase(rec.getVehicleType()) ? "BLUE" : "UNKNOWN"));
      }
      if (rec.getDirection() == null || rec.getDirection().isBlank()) {
        rec.setDirection("STATIONARY");
      }
      if (rec.getJudgmentStatus() == null || rec.getJudgmentStatus().isBlank()) {
        rec.setJudgmentStatus("NORMAL");
      }
      if (rec.getCameraId() == null) rec.setCameraId("");
      if (rec.getVehicleType() == null || rec.getVehicleType().isBlank()) rec.setVehicleType("UNKNOWN");

      recognitionFeed.removeIf(existing -> rec.getRecordId().equals(existing.getRecordId()));
      recognitionFeed.add(0, rec);
      if (databaseMode) {
        jdbc.update(
            "INSERT INTO chg_plate_recognition_records(record_id,site_id,space_id,camera_id,plate,plate_color,vehicle_type,confidence,snapshot_url,direction,judgment_status,is_simulated,recognized_at) " +
            "VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?) ON CONFLICT(record_id) DO NOTHING",
            rec.getRecordId(), rec.getSiteId(), rec.getSpaceId(), rec.getCameraId(),
            rec.getPlate(), rec.getPlateColor(), rec.getVehicleType(), rec.getConfidence(),
            rec.getSnapshotUrl() != null ? rec.getSnapshotUrl() : "",
            rec.getDirection(), rec.getJudgmentStatus(), rec.isSimulated(),
            Timestamp.from(rec.getRecognizedAt()));
      }
    }
    while (recognitionFeed.size() > MAX_RECOGNITION_FEED_MEMORY) {
      recognitionFeed.remove(recognitionFeed.size() - 1);
    }
    if (!databaseMode) {
      persistLprRecordsToDisk();
    }
  }

  public List<PlateRecognitionRecord> getRecognitionRecords(String siteId, int limit) {
    String effectiveSiteId = (siteId == null || siteId.isBlank()) ? "default" : siteId;
    int effLimit = limit > 0 ? limit : 50;
    return recognitionFeed.stream()
        .filter(r -> r.getSiteId() != null && r.getSiteId().equalsIgnoreCase(effectiveSiteId))
        .sorted(Comparator.comparing(
            (PlateRecognitionRecord r) -> r.getRecognizedAt() != null ? r.getRecognizedAt() : Instant.EPOCH
        ).reversed())
        .limit(effLimit)
        .toList();
  }

  public synchronized String saveSpaceSnapshot(String siteId, String spaceId, byte[] jpegBytes) {
    if (jpegBytes == null || jpegBytes.length < 4 || (jpegBytes[0] & 0xFF) != 0xFF || (jpegBytes[1] & 0xFF) != 0xD8) {
      throw new IllegalArgumentException("snapshot payload must be a valid JPEG");
    }
    String safeSite = sanitizePathSegment(siteId, "default");
    String safeSpace = sanitizePathSegment(spaceId, "space");
    File dir = new File(new File(storageDir, "snapshots"), safeSite);
    if (!dir.exists()) dir.mkdirs();
    Path target = dir.toPath().resolve(safeSpace + ".jpg");
    Path tmp = dir.toPath().resolve(safeSpace + ".jpg.tmp");
    try {
      Files.write(tmp, jpegBytes);
      moveAtomically(tmp, target);
    } catch (IOException e) {
      throw new IllegalStateException("save snapshot failed: " + e.getMessage(), e);
    }

    // 同步刷新关联摄像头的在线状态与最后帧时间
    List<CameraConfig> cams = camerasBySite.get(safeSite);
    if (cams != null) {
      for (CameraConfig cam : cams) {
        if (spaceId.equalsIgnoreCase(cam.getSpaceId())) {
          updateCameraStatus(safeSite, cam.getCameraId(), "ONLINE", null, null);
        }
      }
    }
    return buildSnapshotUrl(safeSite, safeSpace);
  }

  public File getSpaceSnapshotFile(String siteId, String spaceId) {
    String safeSite = sanitizePathSegment(siteId, "default");
    String safeSpace = sanitizePathSegment(spaceId, "");
    if (safeSpace.isBlank()) return null;
    File file = new File(new File(new File(storageDir, "snapshots"), safeSite), safeSpace + ".jpg");
    return (file.exists() && file.isFile()) ? file : null;
  }

  public synchronized String saveEvidenceImage(String rawFilename, byte[] imageBytes) {
    if (imageBytes == null || imageBytes.length == 0) {
      throw new IllegalArgumentException("evidence image cannot be empty");
    }
    String safeName = sanitizeFilename(rawFilename, UUID.randomUUID() + ".jpg");
    File evidenceDir = new File(storageDir, "evidences");
    if (!evidenceDir.exists()) evidenceDir.mkdirs();
    Path target = evidenceDir.toPath().resolve(safeName);
    Path tmp = evidenceDir.toPath().resolve(safeName + ".tmp");
    try {
      Files.write(tmp, imageBytes);
      moveAtomically(tmp, target);
      cleanupEvidenceQuota(evidenceDir);
    } catch (IOException e) {
      throw new IllegalStateException("save evidence image failed: " + e.getMessage(), e);
    }
    return "/api/v1/evidences/" + safeName;
  }

  public File getEvidenceFile(String filename) {
    if (filename == null || filename.isBlank() || filename.contains("..") || filename.contains("/") || filename.contains("\\")) {
      return null;
    }
    File file = new File(new File(storageDir, "evidences"), filename);
    return (file.exists() && file.isFile()) ? file : null;
  }

  private void cleanupEvidenceQuota(File evidenceDir) {
    File[] files = evidenceDir.listFiles(File::isFile);
    if (files == null || files.length == 0) return;
    long total = 0;
    for (File f : files) total += f.length();
    if (total <= evidenceMaxBytes) return;
    Arrays.sort(files, Comparator.comparingLong(File::lastModified));
    for (File f : files) {
      if (total <= evidenceMaxBytes) break;
      long len = f.length();
      if (f.delete()) {
        total -= len;
      }
    }
  }

  private String buildSnapshotUrl(String siteId, String spaceId) {
    return "/api/v1/sites/" + URLEncoder.encode(siteId, StandardCharsets.UTF_8)
        + "/spaces/" + URLEncoder.encode(spaceId, StandardCharsets.UTF_8) + "/snapshot";
  }

  private String sanitizePathSegment(String raw, String fallback) {
    if (raw == null || raw.isBlank()) return fallback;
    String cleaned = raw.trim().replaceAll("[^a-zA-Z0-9._-]", "_");
    if (cleaned.isBlank() || cleaned.contains("..")) return fallback;
    return cleaned;
  }

  private String sanitizeFilename(String raw, String fallback) {
    if (raw == null || raw.isBlank()) return fallback;
    String name = Path.of(raw).getFileName().toString();
    String cleaned = name.replaceAll("[^a-zA-Z0-9._-]", "_");
    if (cleaned.isBlank() || cleaned.contains("..")) return fallback;
    return cleaned;
  }

  // ====== 充电桩接口协议管理与车位品牌联动 ======

  public void initBuiltinProtocols() {
    if (!protocolsCache.isEmpty()) return;
    protocolsCache.put("proto-teld", new ChargerProtocol(
        "proto-teld", "特来电 (TELD)", "HTTP_API",
        "特来电开放云平台 OpenAPI 协议，支持多桩状态实时推送与高频主动轮询", 443,
        "{\"operatorId\":\"\",\"appId\":\"\",\"appSecret\":\"\",\"stationId\":\"\",\"pollIntervalSeconds\":5}",
        "{\"1\":\"IDLE\",\"2\":\"PREPARING\",\"3\":\"CHARGING\",\"4\":\"COMPLETED\",\"255\":\"FAULT\"}",
        "{\"statusPath\":\"$.data.connectorStatus\",\"socPath\":\"$.data.soc\",\"powerPath\":\"$.data.powerKw\"}",
        true, true
    ));
    protocolsCache.put("proto-starcharge", new ChargerProtocol(
        "proto-starcharge", "星星充电 (StarCharge)", "HTTP_API",
        "万帮星星充电开放互联规约，支持标准 REST 签名校验与 MQTT 实时工况流", 443,
        "{\"operatorId\":\"\",\"operatorSecret\":\"\",\"stubId\":\"\",\"connectorId\":\"1\",\"pollIntervalSeconds\":5}",
        "{\"0\":\"OFFLINE\",\"1\":\"IDLE\",\"2\":\"CHARGING\",\"3\":\"COMPLETED\",\"4\":\"FAULT\"}",
        "{\"statusPath\":\"$.connectorStatusInfo.status\",\"socPath\":\"$.connectorStatusInfo.soc\",\"powerPath\":\"$.connectorStatusInfo.power\"}",
        true, true
    ));
    protocolsCache.put("proto-stategrid", new ChargerProtocol(
        "proto-stategrid", "国家电网 (StateGrid)", "HTTP_API",
        "符合中电联 GB/T 32694 电动汽车充换电服务信息交换规范及车联网互联协议", 8080,
        "{\"operatorId\":\"\",\"operatorSecret\":\"\",\"equipmentId\":\"\",\"connectorId\":\"1\"}",
        "{\"1\":\"IDLE\",\"2\":\"PREPARING\",\"3\":\"CHARGING\",\"4\":\"COMPLETED\",\"5\":\"FAULT\",\"255\":\"OFFLINE\"}",
        "{\"statusPath\":\"$.connectorStatusInfo.status\",\"socPath\":\"$.connectorStatusInfo.soc\",\"powerPath\":\"$.connectorStatusInfo.power\"}",
        true, true
    ));
    protocolsCache.put("proto-ykc", new ChargerProtocol(
        "proto-ykc", "云快充 (YKC 1.5/1.6)", "TCP_GATEWAY",
        "南京云快充通用物联协议，支持主流第三方运营商聚合平台与网关接入", 9000,
        "{\"gatewayIp\":\"\",\"gatewayPort\":9000,\"pileCode\":\"\",\"gunNo\":1}",
        "{\"0\":\"IDLE\",\"1\":\"CHARGING\",\"2\":\"COMPLETED\",\"3\":\"FAULT\",\"4\":\"PREPARING\"}",
        "{\"statusPath\":\"$.data.gunStatus\",\"socPath\":\"$.data.soc\",\"powerPath\":\"$.data.realPower\"}",
        true, true
    ));
    protocolsCache.put("proto-modbus-tcp", new ChargerProtocol(
        "proto-modbus-tcp", "通用 Modbus-TCP 电桩", "MODBUS_TCP",
        "支持标准工业以太网 Modbus-TCP 直连各类直流充电机、交流充电桩控制器与智能电表", 502,
        "{\"host\":\"\",\"port\":502,\"slaveId\":1,\"statusRegister\":40001,\"socRegister\":40002,\"powerRegister\":40003}",
        "{\"0\":\"IDLE\",\"1\":\"PREPARING\",\"2\":\"CHARGING\",\"3\":\"COMPLETED\",\"4\":\"FAULT\"}",
        "{\"statusRegister\":40001,\"socRegister\":40002,\"powerRegister\":40003}",
        true, true
    ));
    protocolsCache.put("proto-ocpp-16j", new ChargerProtocol(
        "proto-ocpp-16j", "国际标准 OCPP 1.6-J", "OCPP_JSON",
        "Open Charge Point Protocol 1.6-J 国际开放充电协议，支持 WebSocket JSON 格式", 9220,
        "{\"centralSystemUrl\":\"\",\"chargePointId\":\"\",\"connectorId\":1}",
        "{\"Available\":\"IDLE\",\"Preparing\":\"PREPARING\",\"Charging\":\"CHARGING\",\"Finishing\":\"COMPLETED\",\"SuspendedEVSE\":\"COMPLETED\",\"Faulted\":\"FAULT\",\"Unavailable\":\"OFFLINE\"}",
        "{\"statusPath\":\"$.status\",\"socPath\":\"$.soc\",\"powerPath\":\"$.meterValue.power\"}",
        true, true
    ));
  }

  public List<ChargerProtocol> listProtocols() {
    if (protocolsCache.isEmpty()) {
      initBuiltinProtocols();
    }
    return protocolsCache.values().stream()
        .sorted(Comparator.comparing(ChargerProtocol::isBuiltin).reversed()
            .thenComparing(ChargerProtocol::brandName))
        .toList();
  }

  public ChargerProtocol getProtocol(String protocolId) {
    if (protocolsCache.isEmpty()) {
      initBuiltinProtocols();
    }
    return protocolsCache.get(protocolId);
  }

  public synchronized ChargerProtocol saveProtocol(ChargerProtocol protocol) {
    if (protocol.protocolId() == null || protocol.protocolId().isBlank()) {
      throw new IllegalArgumentException("protocolId cannot be empty");
    }
    if (protocolsCache.isEmpty() && !databaseMode) {
      initBuiltinProtocols();
    }
    ChargerProtocol existing = protocolsCache.get(protocol.protocolId());
    boolean isBuiltin = existing != null ? existing.isBuiltin() : Boolean.TRUE.equals(protocol.isBuiltin());
    ChargerProtocol saved = new ChargerProtocol(
        protocol.protocolId(),
        protocol.brandName(),
        protocol.protocolCategory(),
        protocol.description(),
        protocol.defaultPort(),
        protocol.connectionConfigSchema(),
        protocol.statusMapping(),
        protocol.dataParserRules(),
        isBuiltin,
        protocol.enabled(),
        existing != null ? existing.createdAt() : Instant.now(),
        Instant.now()
    );
    protocolsCache.put(saved.protocolId(), saved);
    if (databaseMode) {
      jdbc.update(
          "INSERT INTO chg_charger_protocols(protocol_id, brand_name, protocol_category, description, default_port, connection_config_schema, status_mapping, data_parser_rules, is_builtin, enabled, created_at, updated_at) " +
          "VALUES(?,?,?,?,?,?,?,?,?,?,?,?) ON CONFLICT(protocol_id) DO UPDATE SET brand_name=EXCLUDED.brand_name, protocol_category=EXCLUDED.protocol_category, description=EXCLUDED.description, default_port=EXCLUDED.default_port, connection_config_schema=EXCLUDED.connection_config_schema, status_mapping=EXCLUDED.status_mapping, data_parser_rules=EXCLUDED.data_parser_rules, enabled=EXCLUDED.enabled, updated_at=EXCLUDED.updated_at",
          saved.protocolId(), saved.brandName(), saved.protocolCategory(), saved.description(), saved.defaultPort(),
          saved.connectionConfigSchema(), saved.statusMapping(), saved.dataParserRules(), saved.isBuiltin(), saved.enabled(),
          Timestamp.from(saved.createdAt()), Timestamp.from(saved.updatedAt()));
    } else {
      persistProtocolsToDisk();
    }
    return saved;
  }

  public synchronized boolean deleteProtocol(String protocolId) {
    ChargerProtocol p = protocolsCache.get(protocolId);
    if (p == null) return false;
    if (Boolean.TRUE.equals(p.isBuiltin())) {
      throw new IllegalStateException("系统内置品牌协议不可删除: " + protocolId);
    }
    protocolsCache.remove(protocolId);
    if (databaseMode) {
      jdbc.update("DELETE FROM chg_charger_protocols WHERE protocol_id = ?", protocolId);
    } else {
      persistProtocolsToDisk();
    }
    return true;
  }

  public Map<String, Object> testChargerProtocol(String protocolId, Map<String, Object> sampleData) {
    ChargerProtocol p = getProtocol(protocolId);
    if (p == null) {
      return Map.of("success", false, "error", "未找到协议: " + protocolId);
    }
    String rawCode = sampleData != null && sampleData.containsKey("rawCode") ? String.valueOf(sampleData.get("rawCode")) : "";
    String mappedStatus = "UNKNOWN";
    try {
      if (p.statusMapping() != null && !p.statusMapping().isBlank() && !rawCode.isBlank()) {
        Map<String, String> mapping = mapper.readValue(p.statusMapping(), new TypeReference<Map<String, String>>() {});
        mappedStatus = mapping.getOrDefault(rawCode, mapping.getOrDefault(rawCode.toUpperCase(), "UNKNOWN"));
      }
    } catch (Exception ignored) {}
    Map<String, Object> result = new LinkedHashMap<>();
    result.put("success", !"UNKNOWN".equals(mappedStatus));
    result.put("protocolId", protocolId);
    result.put("brandName", p.brandName());
    result.put("protocolCategory", p.protocolCategory());
    result.put("inputRawCode", rawCode);
    result.put("mappedChargerStatus", mappedStatus);
    result.put("testedAt", Instant.now());
    return result;
  }

  public List<Map<String, Object>> getChargerStatusesForSite(String siteId) {
    String effectiveSiteId = (siteId == null || siteId.isBlank()) ? "default" : siteId;
    PlatformConfiguration config = siteConfiguration(effectiveSiteId);
    List<DeviceBinding> bindings = config != null ? config.bindings() : Collections.emptyList();
    List<Map<String, Object>> statuses = new ArrayList<>();
    for (DeviceBinding b : bindings) {
      Map<String, Object> item = new LinkedHashMap<>();
      item.put("spaceId", b.spaceId());
      item.put("chargerId", b.chargerId());
      item.put("chargerBrand", b.chargerBrand() != null ? b.chargerBrand() : "");
      item.put("protocolId", b.protocolId());
      item.put("connectorId", b.connectorId());
      SpaceRuntimeStatus st = null;
      for (SpaceRuntimeStatus s : spaceStatuses.values()) {
        if (effectiveSiteId.equalsIgnoreCase(s.siteId()) && b.spaceId().equalsIgnoreCase(s.spaceId())) {
          st = s;
          break;
        }
      }
      String chargerStatus = (st != null && st.chargerStatus() != null) ? st.chargerStatus() : "UNKNOWN";
      item.put("status", chargerStatus);
      item.put("soc", null);
      item.put("powerKw", null);
      item.put("voltage", null);
      item.put("current", null);
      item.put("gunConnected", "CHARGING".equalsIgnoreCase(chargerStatus) || "PREPARING".equalsIgnoreCase(chargerStatus));
      item.put("lastHeartbeat", st != null ? st.observedAt() : null);
      statuses.add(item);
    }
    return statuses;
  }

  public Map<String, Object> testSpaceCharger(String siteId, String spaceId) {
    String effectiveSiteId = (siteId == null || siteId.isBlank()) ? "default" : siteId;
    PlatformConfiguration config = siteConfiguration(effectiveSiteId);
    List<DeviceBinding> bindings = config != null ? config.bindings() : Collections.emptyList();
    DeviceBinding binding = bindings.stream().filter(b -> b.spaceId().equalsIgnoreCase(spaceId)).findFirst().orElse(null);
    if (binding == null) {
      return Map.of("success", false, "connected", false, "error", "车位未配置充电桩绑定: " + spaceId);
    }
    ChargerProtocol proto = getProtocol(binding.protocolId());
    String brand = proto != null ? proto.brandName() : (binding.chargerBrand() != null ? binding.chargerBrand() : "");
    String category = proto != null ? proto.protocolCategory() : "UNKNOWN";

    Map<String, Object> result = new LinkedHashMap<>();
    result.put("spaceId", spaceId);
    result.put("chargerId", binding.chargerId());
    result.put("chargerBrand", brand);
    result.put("protocolId", binding.protocolId());
    result.put("connectorId", binding.connectorId());
    result.put("protocolCategory", category);

    boolean connected = false;
    long latencyMs = -1;
    String message;

    String paramsStr = binding.chargerParams();
    String host = null;
    int port = -1;
    if (paramsStr != null && !paramsStr.isBlank()) {
      try {
        JsonNode node = mapper.readTree(paramsStr);
        if (node.has("host")) host = node.get("host").asText();
        else if (node.has("gatewayIp")) host = node.get("gatewayIp").asText();

        if (node.has("port")) port = node.get("port").asInt();
        else if (node.has("gatewayPort")) port = node.get("gatewayPort").asInt();
      } catch (Exception ignored) {}
    }

    SpaceRuntimeStatus runtime = spaceStatuses.values().stream()
        .filter(s -> effectiveSiteId.equalsIgnoreCase(s.siteId()) && spaceId.equalsIgnoreCase(s.spaceId()))
        .findFirst()
        .orElse(null);
    String observedChargerStatus = (runtime != null && runtime.chargerStatus() != null)
        ? runtime.chargerStatus()
        : "UNKNOWN";

    if ("HTTP_API".equalsIgnoreCase(category) || "OCPP_JSON".equalsIgnoreCase(category)) {
      message = "该协议 (" + category + ") 由边缘侧/运营商接口适配器主动同步，云端不直连现场内网端口；当前边缘上报状态: " + observedChargerStatus;
    } else if (host != null && !host.isBlank() && port > 0) {
      long start = System.currentTimeMillis();
      try (Socket socket = new Socket()) {
        socket.connect(new InetSocketAddress(host, port), 1200);
        connected = true;
        latencyMs = System.currentTimeMillis() - start;
        message = "已成功建立至充电桩网关 " + host + ":" + port + " 的 TCP 连接 (" + latencyMs + "ms)，当前业务状态: " + observedChargerStatus;
      } catch (Exception e) {
        connected = false;
        message = "探测连接失败：无法连通 " + host + ":" + port + " (" + e.getMessage() + ")";
      }
    } else {
      connected = false;
      message = "未配置该车位充电桩的通讯网关 IP (host/gatewayIp) 与端口";
    }

    result.put("success", connected);
    result.put("connected", connected);
    result.put("chargerStatus", observedChargerStatus);
    result.put("latencyMs", latencyMs >= 0 ? latencyMs : null);
    result.put("message", message);
    result.put("testedAt", Instant.now());
    return result;
  }

  public List<Map<String, Object>> getSpaceVisionMatrix(String siteId) {
    String effectiveSiteId = (siteId == null || siteId.isBlank()) ? "default" : siteId;
    PlatformConfiguration config = siteConfiguration(effectiveSiteId);
    List<DeviceBinding> bindings = config != null ? config.bindings() : Collections.emptyList();
    if (bindings.isEmpty()) {
      return Collections.emptyList();
    }

    Map<String, CameraConfig> cameraById = new HashMap<>();
    for (CameraConfig cam : getCamerasForSite(effectiveSiteId)) {
      cameraById.put(cam.getCameraId(), cam);
    }

    Map<String, SpaceRuntimeStatus> siteSpaces = new LinkedHashMap<>();
    for (SpaceRuntimeStatus st : spaceStatuses.values()) {
      if (effectiveSiteId.equalsIgnoreCase(st.siteId())) {
        siteSpaces.put(st.spaceId(), st);
      }
    }

    List<Map<String, Object>> matrix = new ArrayList<>();
    for (DeviceBinding binding : bindings) {
      String spaceId = binding.spaceId();
      String camId = binding.cameraId();
      String chargerId = binding.chargerId();
      String chargerBrand = binding.chargerBrand() != null ? binding.chargerBrand() : "";
      CameraConfig cam = cameraById.get(camId);

      SpaceRuntimeStatus st = siteSpaces.get(spaceId);
      boolean isOccupied = st != null
          && !"EMPTY".equalsIgnoreCase(st.phase())
          && !"Free".equalsIgnoreCase(st.phase());

      String vehicleType = isOccupied && st.vehicleType() != null && !st.vehicleType().isBlank()
          && !"UNKNOWN".equalsIgnoreCase(st.vehicleType())
          ? st.vehicleType()
          : (isOccupied ? "UNKNOWN" : "NONE");
      String plate = isOccupied ? (st.plate() != null && !st.plate().isBlank() ? st.plate() : null) : null;
      String plateColor = "NEW_ENERGY".equalsIgnoreCase(vehicleType) ? "GREEN"
          : (isOccupied && "FUEL".equalsIgnoreCase(vehicleType) ? "BLUE"
          : (isOccupied && "SPECIAL_VEHICLE".equalsIgnoreCase(vehicleType) ? "WHITE"
          : (isOccupied && "HEAVY_VEHICLE".equalsIgnoreCase(vehicleType) ? "YELLOW"
          : (isOccupied && "NON_MOTOR".equalsIgnoreCase(vehicleType) ? "TWO_WHEELER"
          : (isOccupied && "OBSTACLE".equalsIgnoreCase(vehicleType) ? "OBSTACLE"
          : (isOccupied ? "UNKNOWN" : "EMPTY"))))));

      String judgmentStatus;
      if (!isOccupied) {
        judgmentStatus = "IDLE";
      } else if (st.alarming() || st.activeEventId() != null) {
        judgmentStatus = "VIOLATION_ALARMING";
      } else if ("NEW_ENERGY".equalsIgnoreCase(vehicleType)) {
        judgmentStatus = "CHARGING".equalsIgnoreCase(st.chargerStatus()) ? "NORMAL_CHARGING" : "GRACE_PERIOD";
      } else if ("SPECIAL_VEHICLE".equalsIgnoreCase(vehicleType)) {
        judgmentStatus = "SPECIAL_ALLOWED";
      } else if (plate != null && isWhitelisted(effectiveSiteId, plate)) {
        judgmentStatus = "WHITELIST_ALLOWED";
      } else if ("FUEL".equalsIgnoreCase(vehicleType) || "NON_MOTOR".equalsIgnoreCase(vehicleType) || "OBSTACLE".equalsIgnoreCase(vehicleType) || "HEAVY_VEHICLE".equalsIgnoreCase(vehicleType)) {
        judgmentStatus = "VIOLATION_ALARMING";
      } else {
        judgmentStatus = "GRACE_PERIOD";
      }

      String chargerStatus = st != null && st.chargerStatus() != null ? st.chargerStatus() : "UNKNOWN";
      String snapshotUrl = getSpaceSnapshotFile(effectiveSiteId, spaceId) != null
          ? buildSnapshotUrl(effectiveSiteId, spaceId)
          : null;

      Map<String, Object> cell = new LinkedHashMap<>();
      cell.put("spaceId", spaceId);
      cell.put("cameraId", camId);
      cell.put("cameraName", cam != null ? cam.getCameraName() : camId);
      cell.put("cameraStatus", cam != null ? cam.getStatus() : "UNREGISTERED");
      cell.put("protocol", cam != null ? cam.getProtocol() : "RTSP");
      cell.put("streamUrl", cam != null ? CameraConfig.maskStreamUrlCredentials(cam.getStreamUrl()) : "");
      cell.put("roiJson", cam != null ? cam.getRoiJson() : "");
      cell.put("chargerId", chargerId);
      cell.put("chargerBrand", chargerBrand);
      cell.put("occupied", isOccupied);
      cell.put("plate", plate);
      cell.put("plateColor", plateColor);
      cell.put("vehicleType", vehicleType);
      cell.put("confidence", isOccupied && st.confidence() != null ? st.confidence() : 0.0);
      cell.put("judgmentStatus", judgmentStatus);
      cell.put("chargerStatus", chargerStatus);
      cell.put("snapshotUrl", snapshotUrl);
      cell.put("isSimulated", st != null && Boolean.TRUE.equals(st.isSimulated()));
      cell.put("recognizedAt", st != null && st.observedAt() != null ? st.observedAt().toString() : null);
      matrix.add(cell);
    }
    return matrix;
  }
}
