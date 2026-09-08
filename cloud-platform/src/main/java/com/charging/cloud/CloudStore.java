package com.charging.cloud;

import com.fasterxml.jackson.core.type.TypeReference;
import com.fasterxml.jackson.databind.ObjectMapper;
import com.fasterxml.jackson.databind.SerializationFeature;
import com.fasterxml.jackson.datatype.jsr310.JavaTimeModule;
import jakarta.annotation.PostConstruct;
import java.io.File;
import java.time.Instant;
import java.time.temporal.ChronoUnit;
import java.util.*;
import java.util.concurrent.ConcurrentHashMap;
import java.util.stream.Collectors;
import org.springframework.stereotype.Service;

@Service
public class CloudStore {
  private volatile PlatformConfiguration configuration = PlatformConfiguration.defaults();
  private final Map<UUID, ViolationEvent> events = new ConcurrentHashMap<>();
  private final Map<String, Instant> heartbeats = new ConcurrentHashMap<>();
  private final Map<String, WhitelistRecord> whitelist = new ConcurrentHashMap<>();
  private final ObjectMapper mapper = new ObjectMapper()
      .registerModule(new JavaTimeModule())
      .disable(SerializationFeature.WRITE_DATES_AS_TIMESTAMPS);

  private final File storageDir = new File("data/cloud_store");

  @PostConstruct
  public void init() {
    loadFromDisk();
  }

  public PlatformConfiguration configuration() {
    return configuration;
  }

  public RuleConfig rules() {
    return configuration.rules();
  }

  public synchronized PlatformConfiguration updateConfiguration(PlatformConfiguration incoming) {
    configuration = new PlatformConfiguration(
        configuration.version() + 1,
        Instant.now(),
        incoming.rules(),
        List.copyOf(incoming.bindings()));
    persistToDisk();
    return configuration;
  }

  public synchronized RuleConfig updateRules(RuleConfig rules) {
    updateConfiguration(new PlatformConfiguration(
        configuration.version(),
        Instant.now(),
        rules,
        configuration.bindings()));
    return configuration.rules();
  }

  public ViolationEvent add(ViolationEvent e) {
    events.put(e.eventId(), e);
    persistEvents();
    return e;
  }

  public List<ViolationEvent> events() {
    return events.values().stream()
        .sorted(Comparator.comparing(ViolationEvent::occurredAt).reversed())
        .toList();
  }

  public List<ViolationEvent> filterEvents(String spaceId, String plate, String violationType) {
    return events.values().stream()
        .filter(e -> spaceId == null || spaceId.isBlank() || spaceId.equalsIgnoreCase(e.spaceId()))
        .filter(e -> plate == null || plate.isBlank() || (e.plate() != null && e.plate().contains(plate)))
        .filter(e -> violationType == null || violationType.isBlank() || violationType.equalsIgnoreCase(e.violationType()))
        .sorted(Comparator.comparing(ViolationEvent::occurredAt).reversed())
        .toList();
  }

  public void heartbeat(String node) {
    heartbeats.put(node, Instant.now());
  }

  public Map<String, Instant> heartbeats() {
    return Map.copyOf(heartbeats);
  }

  // 白名单管理
  public List<WhitelistRecord> listWhitelist() {
    return whitelist.values().stream()
        .sorted(Comparator.comparing(WhitelistRecord::addedAt).reversed())
        .toList();
  }

  public WhitelistRecord saveWhitelist(WhitelistRecord record) {
    whitelist.put(record.plate(), record);
    persistWhitelist();
    return record;
  }

  public boolean removeWhitelist(String plate) {
    boolean removed = whitelist.remove(plate) != null;
    if (removed) {
      persistWhitelist();
    }
    return removed;
  }

  public boolean isWhitelisted(String plate) {
    if (plate == null || plate.isBlank()) return false;
    WhitelistRecord record = whitelist.get(plate);
    if (record == null || !record.enabled()) return false;
    return record.expiresAt() == null || record.expiresAt().isAfter(Instant.now());
  }

  // 大屏统计指标
  public DashboardStats dashboardStats() {
    long total = events.size();
    Instant oneDayAgo = Instant.now().minus(24, ChronoUnit.HOURS);
    long today = events.values().stream()
        .filter(e -> e.occurredAt() != null && e.occurredAt().isAfter(oneDayAgo))
        .count();

    Map<String, Long> byType = events.values().stream()
        .filter(e -> e.violationType() != null)
        .collect(Collectors.groupingBy(ViolationEvent::violationType, Collectors.counting()));

    Map<String, Long> bySpace = events.values().stream()
        .filter(e -> e.spaceId() != null)
        .collect(Collectors.groupingBy(ViolationEvent::spaceId, Collectors.counting()));

    Instant healthyThreshold = Instant.now().minus(90, ChronoUnit.SECONDS);
    int onlineCount = (int) heartbeats.values().stream()
        .filter(t -> t.isAfter(healthyThreshold))
        .count();

    return new DashboardStats(total, today, byType, bySpace, onlineCount, heartbeats.size());
  }

  // 本地轻量持久化
  private synchronized void persistEvents() {
    try {
      if (!storageDir.exists()) storageDir.mkdirs();
      mapper.writeValue(new File(storageDir, "events.json"), events.values());
    } catch (Exception ignored) {}
  }

  private synchronized void persistWhitelist() {
    try {
      if (!storageDir.exists()) storageDir.mkdirs();
      mapper.writeValue(new File(storageDir, "whitelist.json"), whitelist.values());
    } catch (Exception ignored) {}
  }

  private synchronized void persistToDisk() {
    try {
      if (!storageDir.exists()) storageDir.mkdirs();
      mapper.writeValue(new File(storageDir, "configuration.json"), configuration);
    } catch (Exception ignored) {}
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
      if (wlFile.exists()) {
        List<WhitelistRecord> list = mapper.readValue(wlFile, new TypeReference<List<WhitelistRecord>>() {});
        for (WhitelistRecord wl : list) {
          whitelist.put(wl.plate(), wl);
        }
      }
    } catch (Exception ignored) {}
  }
}

