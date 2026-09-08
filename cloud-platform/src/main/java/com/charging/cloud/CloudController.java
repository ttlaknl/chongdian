package com.charging.cloud;

import jakarta.validation.Valid;
import java.time.Instant;
import java.util.*;
import org.springframework.http.HttpHeaders;
import org.springframework.http.HttpStatus;
import org.springframework.http.MediaType;
import org.springframework.http.ResponseEntity;
import org.springframework.web.bind.annotation.*;

@RestController
@RequestMapping("/api/v1")
public class CloudController {
  private final CloudStore store;

  public CloudController(CloudStore store) {
    this.store = store;
  }

  @GetMapping("/health")
  public Map<String, Object> health() {
    return Map.of("status", "UP", "service", "cloud-platform");
  }

  @GetMapping("/configuration")
  public PlatformConfiguration configuration() {
    return store.configuration();
  }

  @PutMapping("/configuration")
  public PlatformConfiguration updateConfiguration(@Valid @RequestBody PlatformConfiguration configuration) {
    return store.updateConfiguration(configuration);
  }

  @GetMapping("/rules")
  public RuleConfig rules() {
    return store.rules();
  }

  @PutMapping("/rules")
  public RuleConfig update(@Valid @RequestBody RuleConfig rules) {
    return store.updateRules(rules);
  }

  @PostMapping("/events")
  @ResponseStatus(HttpStatus.CREATED)
  public ViolationEvent event(@RequestBody ViolationEvent e) {
    return store.add(e.eventId() == null
        ? new ViolationEvent(UUID.randomUUID(), e.spaceId(), e.plate(), e.vehicleType(), e.violationType(),
            e.occurredAt() == null ? Instant.now() : e.occurredAt(), e.evidenceUrl())
        : e);
  }

  @GetMapping("/events")
  public List<ViolationEvent> events(
      @RequestParam(required = false) String spaceId,
      @RequestParam(required = false) String plate,
      @RequestParam(required = false) String violationType) {
    if (spaceId != null || plate != null || violationType != null) {
      return store.filterEvents(spaceId, plate, violationType);
    }
    return store.events();
  }

  @GetMapping(value = "/events/export", produces = "text/csv;charset=UTF-8")
  public ResponseEntity<String> exportEvents(
      @RequestParam(required = false) String spaceId,
      @RequestParam(required = false) String plate,
      @RequestParam(required = false) String violationType) {
    String csv = store.exportEventsCsv(spaceId, plate, violationType);
    return ResponseEntity.ok()
        .header(HttpHeaders.CONTENT_DISPOSITION, "attachment; filename=violation_ledger.csv")
        .contentType(MediaType.parseMediaType("text/csv;charset=UTF-8"))
        .body(csv);
  }

  @GetMapping("/dashboard/stats")
  public DashboardStats dashboardStats() {
    return store.dashboardStats();
  }

  @GetMapping("/whitelist")
  public List<WhitelistRecord> listWhitelist() {
    return store.listWhitelist();
  }

  @PostMapping("/whitelist")
  @ResponseStatus(HttpStatus.CREATED)
  public WhitelistRecord addWhitelist(@Valid @RequestBody WhitelistRecord record) {
    return store.saveWhitelist(record);
  }

  @DeleteMapping("/whitelist/{plate}")
  public Map<String, Object> removeWhitelist(@PathVariable String plate) {
    boolean removed = store.removeWhitelist(plate);
    return Map.of("plate", plate, "removed", removed);
  }

  @PostMapping("/nodes/{nodeId}/heartbeat")
  public Map<String, Object> heartbeat(@PathVariable String nodeId) {
    store.heartbeat(nodeId);
    return Map.of("accepted", true, "nodeId", nodeId, "serverTime", Instant.now());
  }

  @GetMapping("/nodes/heartbeats")
  public Map<String, Instant> heartbeats() {
    return store.heartbeats();
  }
}

