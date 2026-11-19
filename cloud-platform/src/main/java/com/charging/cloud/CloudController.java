package com.charging.cloud;

import jakarta.servlet.http.HttpServletRequest;
import jakarta.validation.Valid;
import java.io.File;
import java.time.Instant;
import java.util.*;
import org.springframework.core.io.Resource;
import org.springframework.core.io.UrlResource;
import org.springframework.http.CacheControl;
import org.springframework.http.HttpHeaders;
import org.springframework.http.HttpStatus;
import org.springframework.http.MediaType;
import org.springframework.http.ResponseEntity;
import org.springframework.web.bind.annotation.*;
import org.springframework.web.multipart.MultipartFile;
import org.springframework.web.servlet.mvc.method.annotation.SseEmitter;

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

  // ====== 停车场 (Sites) 接口 ======

  @GetMapping("/sites")
  public List<Site> listSites() {
    return store.listSites();
  }

  @GetMapping("/sites/summary")
  public List<Map<String, Object>> siteSummaries() {
    return store.siteSummaries();
  }

  @GetMapping("/sites/{siteId}")
  public ResponseEntity<Site> getSite(@PathVariable String siteId) {
    Site site = store.getSite(siteId);
    return site != null ? ResponseEntity.ok(site) : ResponseEntity.notFound().build();
  }

  @PostMapping("/sites")
  @ResponseStatus(HttpStatus.CREATED)
  public Site createSite(@Valid @RequestBody Site site, HttpServletRequest request) {
    Site created = store.createSite(site);
    store.audit(actor(request), "SITE_CREATE", "site:" + created.siteId(), created.siteName());
    return created;
  }

  @PutMapping("/sites/{siteId}")
  public Site updateSite(
      @PathVariable String siteId,
      @RequestBody Site site,
      HttpServletRequest request) {
    Site updated = store.updateSite(siteId, site);
    store.audit(actor(request), "SITE_UPDATE", "site:" + siteId, updated.siteName());
    return updated;
  }

  @DeleteMapping("/sites/{siteId}")
  @ResponseStatus(HttpStatus.NO_CONTENT)
  public void deleteSite(@PathVariable String siteId, HttpServletRequest request) {
    store.deleteSite(siteId);
    store.audit(actor(request), "SITE_DELETE", "site:" + siteId, "deleted");
  }

  @GetMapping("/sites/{siteId}/configuration")
  public PlatformConfiguration siteConfiguration(
      @PathVariable String siteId,
      HttpServletRequest request) {
    PlatformConfiguration cfg = store.siteConfiguration(siteId);
    Object role = request.getAttribute("authRole");
    if (role == null && !isDevelopmentUnsecured(request)) {
      return cfg.sanitizedForPublic();
    }
    return cfg;
  }

  @PutMapping("/sites/{siteId}/configuration")
  public PlatformConfiguration updateSiteConfiguration(
      @PathVariable String siteId,
      @Valid @RequestBody PlatformConfiguration configuration,
      HttpServletRequest request) {
    PlatformConfiguration updated = store.updateSiteConfiguration(siteId, configuration);
    store.audit(actor(request), "SITE_CONFIGURATION_UPDATE", "site:" + siteId, "version=" + updated.version());
    return updated;
  }

  @GetMapping({"/sites/{siteId}/rules", "/sites/{siteId}/configuration/rules"})
  public RuleConfig siteRules(@PathVariable String siteId) {
    return store.siteConfiguration(siteId).rules();
  }

  @PutMapping({"/sites/{siteId}/rules", "/sites/{siteId}/configuration/rules"})
  public RuleConfig updateSiteRules(
      @PathVariable String siteId,
      @Valid @RequestBody RuleConfig rules,
      HttpServletRequest request) {
    RuleConfig updated = store.updateSiteRules(siteId, rules);
    store.audit(actor(request), "SITE_RULE_UPDATE", "site:" + siteId, "rules updated");
    return updated;
  }

  @PostMapping("/sites/{siteId}/nodes/{nodeId}")
  public Map<String, Object> bindNodeToSite(
      @PathVariable String siteId,
      @PathVariable String nodeId,
      HttpServletRequest request) {
    store.bindNodeToSite(nodeId, siteId);
    store.audit(actor(request), "SITE_NODE_BIND", "node:" + nodeId, "bound to site:" + siteId);
    return Map.of("accepted", true, "siteId", siteId, "nodeId", nodeId);
  }

  @GetMapping("/configuration")
  public PlatformConfiguration configuration(HttpServletRequest request) {
    String nodeId = request.getHeader("X-Node-Id");
    if (nodeId != null && !nodeId.isBlank()) {
      return store.configurationForNode(nodeId);
    }
    return store.configuration();
  }

  @PutMapping("/configuration")
  public PlatformConfiguration updateConfiguration(
      @Valid @RequestBody PlatformConfiguration configuration,
      HttpServletRequest request) {
    PlatformConfiguration updated = store.updateConfiguration(configuration);
    store.audit(actor(request), "CONFIGURATION_UPDATE", "configuration", "version=" + updated.version());
    return updated;
  }

  @GetMapping({"/rules", "/configuration/rules"})
  public RuleConfig rules() {
    return store.rules();
  }

  @PutMapping({"/rules", "/configuration/rules"})
  public RuleConfig update(@Valid @RequestBody RuleConfig rules, HttpServletRequest request) {
    RuleConfig updated = store.updateRules(rules);
    store.audit(actor(request), "RULE_UPDATE", "rules", "configuration version=" + store.configuration().version());
    return updated;
  }

  @PostMapping("/events")
  @ResponseStatus(HttpStatus.CREATED)
  public ViolationEvent event(@Valid @RequestBody ViolationEvent e, HttpServletRequest request) {
    String nodeHeader = request.getHeader("X-Node-Id");
    String siteId = (nodeHeader != null && !nodeHeader.isBlank())
        ? store.siteIdForNode(nodeHeader)
        : (e.siteId() != null && !e.siteId().isBlank() ? e.siteId() : "default");
    ViolationEvent scoped = e.withSiteId(siteId);
    ViolationEvent stored = store.add(scoped.eventId() == null
        ? new ViolationEvent(UUID.randomUUID(), scoped.sessionId(), scoped.siteId(), scoped.spaceId(),
            scoped.plate(), scoped.vehicleType(), scoped.violationType(), scoped.occurredAt(),
            scoped.evidenceUrl(), scoped.status(), scoped.clearedAt(), scoped.updatedAt(),
            scoped.acknowledgedBy(), scoped.acknowledgedAt())
        : scoped);
    store.audit(actor(request), "EVENT_UPSERT", "event:" + stored.eventId(), stored.status().name());
    return stored;
  }

  @GetMapping("/events")
  public List<ViolationEvent> events(
      @RequestParam(required = false) String spaceId,
      @RequestParam(required = false) String plate,
      @RequestParam(required = false) String violationType,
      @RequestParam(required = false) String siteId) {
    return store.filterEvents(siteId, spaceId, plate, violationType);
  }

  @PostMapping("/events/{eventId}/acknowledge")
  public ResponseEntity<ViolationEvent> acknowledgeEvent(
      @PathVariable UUID eventId,
      HttpServletRequest request) {
    ViolationEvent event = store.acknowledgeEvent(eventId, actor(request));
    if (event != null) {
      store.audit(actor(request), "EVENT_ACKNOWLEDGE", "event:" + eventId, "ACKNOWLEDGED");
    }
    return event == null ? ResponseEntity.notFound().build() : ResponseEntity.ok(event);
  }

  @DeleteMapping("/events")
  public ResponseEntity<Map<String, Object>> clearEvents(
      @RequestParam(required = false) String siteId,
      HttpServletRequest request) {
    store.clearEvents(siteId);
    store.audit(actor(request), "CLEAR_EVENTS", siteId == null || siteId.isBlank() ? "all" : siteId, "清空违规流水记录");
    return ResponseEntity.ok(Map.of("success", true, "message", "已成功清空违规流水记录"));
  }

  @DeleteMapping("/events/{eventId}")
  public ResponseEntity<Map<String, Object>> deleteEvent(
      @PathVariable UUID eventId,
      HttpServletRequest request) {
    boolean removed = store.deleteEvent(eventId);
    if (removed) {
      store.audit(actor(request), "DELETE_EVENT", "event:" + eventId, "删除单条违规事件");
      return ResponseEntity.ok(Map.of("success", true));
    }
    return ResponseEntity.notFound().build();
  }

  @GetMapping(value = "/events/export", produces = "text/csv;charset=UTF-8")
  public ResponseEntity<String> exportEvents(
      @RequestParam(required = false) String spaceId,
      @RequestParam(required = false) String plate,
      @RequestParam(required = false) String violationType,
      @RequestParam(required = false) String siteId) {
    String csv = store.exportEventsCsv(siteId, spaceId, plate, violationType);
    return ResponseEntity.ok()
        .header(HttpHeaders.CONTENT_DISPOSITION, "attachment; filename=violation_ledger.csv")
        .contentType(MediaType.parseMediaType("text/csv;charset=UTF-8"))
        .body(csv);
  }

  @GetMapping("/dashboard/stats")
  public DashboardStats dashboardStats(@RequestParam(required = false) String siteId) {
    return store.dashboardStats(siteId);
  }

  // ====== 白名单接口（支持全局与多场地隔离） ======

  @GetMapping("/whitelist")
  public List<WhitelistRecord> listWhitelist() {
    return store.listWhitelist();
  }

  @PostMapping("/whitelist")
  @ResponseStatus(HttpStatus.CREATED)
  public WhitelistRecord addWhitelist(
      @Valid @RequestBody WhitelistRecord record,
      HttpServletRequest request) {
    WhitelistRecord stored = store.saveWhitelist(record);
    store.audit(actor(request), "WHITELIST_UPSERT", "plate:" + stored.plate(), stored.reason());
    return stored;
  }

  @PutMapping("/whitelist/{plate}")
  public WhitelistRecord updateWhitelist(
      @PathVariable String plate,
      @Valid @RequestBody WhitelistRecord record,
      HttpServletRequest request) {
    WhitelistRecord target = new WhitelistRecord(
        plate,
        record.vehicleType(),
        record.reason(),
        record.addedAt() == null ? Instant.now() : record.addedAt(),
        record.expiresAt(),
        record.enabled()
    );
    WhitelistRecord stored = store.saveWhitelist(target);
    store.audit(actor(request), "WHITELIST_UPDATE", "plate:" + stored.plate(), stored.reason());
    return stored;
  }

  @DeleteMapping("/whitelist/{plate}")
  public Map<String, Object> removeWhitelist(@PathVariable String plate, HttpServletRequest request) {
    boolean removed = store.removeWhitelist(plate);
    if (removed) store.audit(actor(request), "WHITELIST_DELETE", "plate:" + plate, "removed");
    return Map.of("plate", plate, "removed", removed);
  }

  @GetMapping("/sites/{siteId}/whitelist")
  public List<WhitelistRecord> listSiteWhitelist(@PathVariable String siteId) {
    return store.listWhitelist(siteId);
  }

  @PostMapping("/sites/{siteId}/whitelist")
  @ResponseStatus(HttpStatus.CREATED)
  public WhitelistRecord addSiteWhitelist(
      @PathVariable String siteId,
      @Valid @RequestBody WhitelistRecord record,
      HttpServletRequest request) {
    WhitelistRecord stored = store.saveWhitelist(siteId, record);
    store.audit(actor(request), "WHITELIST_UPSERT", "site:" + siteId + "/plate:" + stored.plate(), stored.reason());
    return stored;
  }

  @PutMapping("/sites/{siteId}/whitelist/{plate}")
  public WhitelistRecord updateSiteWhitelist(
      @PathVariable String siteId,
      @PathVariable String plate,
      @Valid @RequestBody WhitelistRecord record,
      HttpServletRequest request) {
    WhitelistRecord target = new WhitelistRecord(
        plate,
        record.vehicleType(),
        record.reason(),
        record.addedAt() == null ? Instant.now() : record.addedAt(),
        record.expiresAt(),
        record.enabled()
    );
    WhitelistRecord stored = store.saveWhitelist(siteId, target);
    store.audit(actor(request), "WHITELIST_UPDATE", "site:" + siteId + "/plate:" + stored.plate(), stored.reason());
    return stored;
  }

  @DeleteMapping("/sites/{siteId}/whitelist/{plate}")
  public Map<String, Object> removeSiteWhitelist(
      @PathVariable String siteId,
      @PathVariable String plate,
      HttpServletRequest request) {
    boolean removed = store.removeWhitelist(siteId, plate);
    if (removed) store.audit(actor(request), "WHITELIST_DELETE", "site:" + siteId + "/plate:" + plate, "removed");
    return Map.of("siteId", siteId, "plate", plate, "removed", removed);
  }

  // ====== 边缘心跳、遥测与 OTA 固件分发 ======

  @PostMapping("/nodes/{nodeId}/heartbeat")
  public Map<String, Object> heartbeat(
      @PathVariable String nodeId,
      @RequestBody(required = false) Map<String, Object> telemetry,
      HttpServletRequest request) {
    store.heartbeat(nodeId, telemetry, request.getRemoteAddr());
    Map<String, Object> resp = new HashMap<>();
    resp.put("accepted", true);
    resp.put("nodeId", nodeId);
    resp.put("serverTime", Instant.now());
    UpgradeManifest pending = store.pollPendingUpgrade(nodeId);
    if (pending != null) {
      resp.put("pendingUpgrade", pending);
    }
    if (store.consumePendingReboot(nodeId)) {
      resp.put("pendingReboot", true);
    }
    return resp;
  }

  @GetMapping("/nodes/heartbeats")
  public Map<String, Instant> heartbeats() {
    return store.heartbeats();
  }

  @PostMapping(value = "/firmware/upload", consumes = MediaType.MULTIPART_FORM_DATA_VALUE)
  public Map<String, Object> uploadFirmwareMultipart(
      @RequestParam("file") MultipartFile file,
      HttpServletRequest request) throws Exception {
    Map<String, Object> result = store.saveFirmwareBinary(file.getOriginalFilename(), file.getBytes());
    store.audit(actor(request), "FIRMWARE_UPLOAD", "firmware:" + result.get("filename"),
        "sha256=" + result.get("sha256Hash"));
    return result;
  }

  @PostMapping(value = "/firmware/upload", consumes = MediaType.APPLICATION_OCTET_STREAM_VALUE)
  public Map<String, Object> uploadFirmwareOctet(
      @RequestParam String filename,
      @RequestBody byte[] bytes,
      HttpServletRequest request) {
    Map<String, Object> result = store.saveFirmwareBinary(filename, bytes);
    store.audit(actor(request), "FIRMWARE_UPLOAD", "firmware:" + result.get("filename"),
        "sha256=" + result.get("sha256Hash"));
    return result;
  }

  @GetMapping("/firmware/{filename:.+}")
  public ResponseEntity<Resource> downloadFirmware(@PathVariable String filename) {
    try {
      if (filename.contains("..") || filename.contains("/") || filename.contains("\\")) {
        return ResponseEntity.status(HttpStatus.FORBIDDEN).build();
      }
      File firmwareFile = new File(store.getFirmwareDirectory(), filename);
      if (!firmwareFile.exists() || !firmwareFile.isFile()) {
        return ResponseEntity.notFound().build();
      }
      Resource resource = new UrlResource(firmwareFile.toURI());
      return ResponseEntity.ok()
          .contentType(MediaType.APPLICATION_OCTET_STREAM)
          .header(HttpHeaders.CONTENT_DISPOSITION, "attachment; filename=\"" + filename + "\"")
          .body(resource);
    } catch (Exception e) {
      return ResponseEntity.status(HttpStatus.INTERNAL_SERVER_ERROR).build();
    }
  }

  // ====== 实时车位状态、视频抓拍与取证图片 ======

  @PostMapping("/nodes/{nodeId}/spaces/status")
  public Map<String, Object> updateSpaceStatuses(
      @PathVariable String nodeId,
      @RequestBody List<SpaceRuntimeStatus> statuses) {
    store.updateSpaceStatuses(nodeId, statuses);
    return Map.of("accepted", true, "count", statuses.size(), "serverTime", Instant.now());
  }

  @GetMapping("/spaces/status")
  public List<SpaceRuntimeStatus> spaceStatuses() {
    return store.spaceStatuses();
  }

  @PostMapping(value = "/nodes/{nodeId}/spaces/{spaceId}/snapshot", consumes = {"image/jpeg", MediaType.APPLICATION_OCTET_STREAM_VALUE})
  public Map<String, Object> uploadSpaceSnapshot(
      @PathVariable String nodeId,
      @PathVariable String spaceId,
      @RequestBody byte[] jpegBytes) {
    String siteId = store.siteIdForNode(nodeId);
    String url = store.saveSpaceSnapshot(siteId, spaceId, jpegBytes);
    return Map.of("accepted", true, "siteId", siteId, "spaceId", spaceId, "snapshotUrl", url);
  }

  @GetMapping("/sites/{siteId}/spaces/{spaceId}/snapshot")
  public ResponseEntity<Resource> getSpaceSnapshot(
      @PathVariable String siteId,
      @PathVariable String spaceId) {
    try {
      File file = store.getSpaceSnapshotFile(siteId, spaceId);
      if (file == null || !file.exists()) {
        return ResponseEntity.notFound().build();
      }
      long ageSec = java.time.Duration.between(
          java.time.Instant.ofEpochMilli(file.lastModified()),
          java.time.Instant.now()
      ).toSeconds();
      if (ageSec > 45) {
        return ResponseEntity.status(HttpStatus.NOT_FOUND).build();
      }
      Resource resource = new UrlResource(file.toURI());
      return ResponseEntity.ok()
          .contentType(MediaType.IMAGE_JPEG)
          .cacheControl(CacheControl.noStore())
          .body(resource);
    } catch (Exception e) {
      return ResponseEntity.status(HttpStatus.INTERNAL_SERVER_ERROR).build();
    }
  }

  @PostMapping(value = "/evidences", consumes = {"image/jpeg", MediaType.APPLICATION_OCTET_STREAM_VALUE})
  public Map<String, Object> uploadEvidence(
      @RequestParam(required = false) String filename,
      @RequestBody byte[] imageBytes) {
    String url = store.saveEvidenceImage(filename, imageBytes);
    return Map.of("accepted", true, "evidenceUrl", url);
  }

  @GetMapping("/evidences/{filename:.+}")
  public ResponseEntity<Resource> getEvidence(@PathVariable String filename) {
    try {
      File file = store.getEvidenceFile(filename);
      if (file == null) {
        return ResponseEntity.notFound().build();
      }
      Resource resource = new UrlResource(file.toURI());
      return ResponseEntity.ok()
          .contentType(MediaType.IMAGE_JPEG)
          .body(resource);
    } catch (Exception e) {
      return ResponseEntity.status(HttpStatus.INTERNAL_SERVER_ERROR).build();
    }
  }

  @GetMapping("/stream/spaces")
  public SseEmitter streamSpaces() {
    return store.subscribeSpaces();
  }

  @GetMapping("/stream/events")
  public SseEmitter streamEvents() {
    return store.subscribeEvents();
  }

  @PostMapping("/nodes/{nodeId}/configuration-status")
  public ConfigurationApplyReport configurationStatus(
      @PathVariable String nodeId,
      @RequestBody ConfigurationApplyReport report) {
    return store.saveConfigurationReport(nodeId, report);
  }

  @GetMapping("/nodes/configuration-status")
  public Map<String, ConfigurationApplyReport> configurationStatuses() {
    return store.configurationReports();
  }

  @GetMapping("/audit")
  public List<OperationAuditRecord> auditRecords() {
    return store.auditRecords();
  }

  @PostMapping("/upgrade/manifest")
  public UpgradeManifest publishUpgradeManifest(
      @Valid @RequestBody UpgradeManifest manifest,
      HttpServletRequest request) {
    UpgradeManifest stored = store.saveUpgradeManifest(manifest);
    store.audit(actor(request), "UPGRADE_PUBLISH", "manifest:v" + stored.version(), stored.versionName());
    return stored;
  }

  @GetMapping("/upgrade/manifest")
  public ResponseEntity<UpgradeManifest> getUpgradeManifest() {
    UpgradeManifest manifest = store.getUpgradeManifest();
    return manifest != null ? ResponseEntity.ok(manifest) : ResponseEntity.noContent().build();
  }

  @PostMapping("/nodes/{nodeId}/upgrade-status")
  public UpgradeNodeReport nodeUpgradeStatus(
      @PathVariable String nodeId,
      @Valid @RequestBody UpgradeNodeReport report,
      HttpServletRequest request) {
    UpgradeNodeReport stored = store.saveNodeUpgradeReport(nodeId, report);
    store.audit(actor(request), "NODE_UPGRADE_STATUS", "node:" + nodeId, report.state());
    return stored;
  }

  @GetMapping("/nodes/upgrade-status")
  public Map<String, UpgradeNodeReport> nodeUpgradeStatuses() {
    return store.nodeUpgradeReports();
  }

  // ====== 多边缘终端管理与控制 ======

  @GetMapping("/sites/{siteId}/nodes")
  public List<EdgeNodeTelemetry> getNodesForSite(@PathVariable String siteId) {
    return store.getNodesForSite(siteId);
  }

  @PostMapping("/nodes/{nodeId}/reboot")
  public Map<String, Object> rebootNode(@PathVariable String nodeId, HttpServletRequest request) {
    boolean success = store.rebootNode(nodeId, actor(request));
    return Map.of("success", success, "nodeId", nodeId, "action", "REBOOT", "message", "已向边缘工控机下发远程重启指令");
  }

  @PostMapping("/nodes/{nodeId}/upgrade")
  public Map<String, Object> upgradeSingleNode(
      @PathVariable String nodeId,
      @RequestBody Map<String, Object> payload,
      HttpServletRequest request) {
    if (payload.get("version") == null) {
      throw new IllegalArgumentException("target version is required");
    }
    int version = ((Number) payload.get("version")).intValue();
    String packageUrl = payload.get("packageUrl") != null ? payload.get("packageUrl").toString() : "";
    String sha256 = payload.get("sha256Hash") != null ? payload.get("sha256Hash").toString() : "";
    boolean success = store.upgradeSingleNode(nodeId, version, packageUrl, sha256, actor(request));
    return Map.of("success", success, "nodeId", nodeId, "targetVersion", version, "message", "单台工控机定向升级指令已下发");
  }

  @PostMapping("/upgrade/batch")
  public Map<String, Object> batchUpgrade(
      @RequestBody Map<String, Object> payload,
      HttpServletRequest request) {
    if (payload.get("version") == null) {
      throw new IllegalArgumentException("target version is required");
    }
    int version = ((Number) payload.get("version")).intValue();
    String packageUrl = payload.get("packageUrl") != null ? payload.get("packageUrl").toString() : "";
    String sha256 = payload.get("sha256Hash") != null ? payload.get("sha256Hash").toString() : "";
    return store.batchUpgradeNodes(version, packageUrl, sha256, actor(request));
  }

  @PostMapping("/sites/{siteId}/nodes")
  public EdgeNodeTelemetry addOrUpdateNode(
      @PathVariable String siteId,
      @RequestBody EdgeNodeTelemetry node,
      HttpServletRequest request) {
    EdgeNodeTelemetry saved = store.addOrUpdateNode(siteId, node);
    store.audit(actor(request), "NODE_SAVE", "site:" + siteId + "/node:" + saved.getNodeId(), saved.getNodeName());
    return saved;
  }

  @PutMapping("/sites/{siteId}/nodes/{nodeId}")
  public EdgeNodeTelemetry updateNode(
      @PathVariable String siteId,
      @PathVariable String nodeId,
      @RequestBody EdgeNodeTelemetry node,
      HttpServletRequest request) {
    node.setNodeId(nodeId);
    EdgeNodeTelemetry saved = store.addOrUpdateNode(siteId, node);
    store.audit(actor(request), "NODE_UPDATE", "site:" + siteId + "/node:" + saved.getNodeId(), saved.getNodeName());
    return saved;
  }

  @DeleteMapping("/sites/{siteId}/nodes/{nodeId}")
  public Map<String, Object> deleteNode(
      @PathVariable String siteId,
      @PathVariable String nodeId,
      HttpServletRequest request) {
    boolean removed = store.deleteNode(siteId, nodeId);
    store.audit(actor(request), "NODE_DELETE", "site:" + siteId + "/node:" + nodeId, "解绑边缘工控机");
    return Map.of("siteId", siteId, "nodeId", nodeId, "removed", removed);
  }

  // ====== 摄像头接入配置 ======

  @GetMapping("/sites/{siteId}/cameras")
  public List<CameraConfig> getCamerasForSite(@PathVariable String siteId) {
    return store.getCamerasForSite(siteId);
  }

  @PostMapping("/sites/{siteId}/cameras")
  public CameraConfig saveCamera(
      @PathVariable String siteId,
      @RequestBody CameraConfig camera,
      HttpServletRequest request) {
    CameraConfig saved = store.saveCamera(siteId, camera);
    store.audit(actor(request), "CAMERA_SAVE", "site:" + siteId + "/cam:" + saved.getCameraId(), saved.getCameraName());
    return saved;
  }

  @PutMapping("/sites/{siteId}/cameras/{cameraId}")
  public CameraConfig updateCamera(
      @PathVariable String siteId,
      @PathVariable String cameraId,
      @RequestBody CameraConfig camera,
      HttpServletRequest request) {
    camera.setCameraId(cameraId);
    CameraConfig saved = store.saveCamera(siteId, camera);
    store.audit(actor(request), "CAMERA_UPDATE", "site:" + siteId + "/cam:" + saved.getCameraId(), saved.getCameraName());
    return saved;
  }

  @DeleteMapping("/sites/{siteId}/cameras/{cameraId}")
  public Map<String, Object> deleteCamera(
      @PathVariable String siteId,
      @PathVariable String cameraId,
      HttpServletRequest request) {
    boolean removed = store.deleteCamera(siteId, cameraId);
    store.audit(actor(request), "CAMERA_DELETE", "site:" + siteId + "/cam:" + cameraId, "删除摄像头配置");
    return Map.of("siteId", siteId, "cameraId", cameraId, "removed", removed);
  }

  @PostMapping("/sites/{siteId}/cameras/{cameraId}/test")
  public Map<String, Object> testCamera(
      @PathVariable String siteId,
      @PathVariable String cameraId) {
    return store.testCamera(siteId, cameraId);
  }

  // ====== 车位与声光设备联动绑定 ======

  @GetMapping("/sites/{siteId}/bindings")
  public List<DeviceBinding> getBindingsForSite(@PathVariable String siteId) {
    return store.getBindingsForSite(siteId);
  }

  @RequestMapping(value = {"/sites/{siteId}/bindings", "/sites/{siteId}/bindings/batch"}, method = {RequestMethod.POST, RequestMethod.PUT})
  public List<DeviceBinding> saveBindingsForSite(
      @PathVariable String siteId,
      @RequestBody List<DeviceBinding> bindings,
      HttpServletRequest request) {
    List<DeviceBinding> saved = store.saveBindingsForSite(siteId, bindings);
    store.audit(actor(request), "BINDINGS_UPDATE", "site:" + siteId, "批量更新车位设备绑定 (" + saved.size() + "个)");
    return saved;
  }

  @PostMapping("/sites/{siteId}/bindings/single")
  public DeviceBinding saveSingleBinding(
      @PathVariable String siteId,
      @RequestBody DeviceBinding binding,
      HttpServletRequest request) {
    DeviceBinding saved = store.saveSingleBinding(siteId, binding);
    store.audit(actor(request), "BINDING_SAVE", "site:" + siteId + "/space:" + saved.spaceId(), "绑定车位/继电器/声光喇叭");
    return saved;
  }

  @PutMapping("/sites/{siteId}/bindings/{spaceId}")
  public DeviceBinding updateSingleBinding(
      @PathVariable String siteId,
      @PathVariable String spaceId,
      @RequestBody DeviceBinding binding,
      HttpServletRequest request) {
    String effSiteId = (siteId == null || siteId.isBlank()) ? "default" : siteId;
    DeviceBinding target = new DeviceBinding(
        effSiteId,
        spaceId,
        binding.cameraId(),
        binding.chargerId(),
        binding.relayChannel(),
        binding.speakerChannel(),
        binding.enabled(),
        binding.chargerBrand(),
        binding.protocolId(),
        binding.connectorId(),
        binding.chargerParams()
    );
    DeviceBinding saved = store.saveSingleBinding(siteId, target);
    store.audit(actor(request), "BINDING_UPDATE", "site:" + siteId + "/space:" + saved.spaceId(), "更新车位硬件绑定");
    return saved;
  }

  @DeleteMapping("/sites/{siteId}/bindings/{spaceId}")
  public Map<String, Object> deleteBinding(
      @PathVariable String siteId,
      @PathVariable String spaceId,
      HttpServletRequest request) {
    boolean removed = store.deleteBinding(siteId, spaceId);
    store.audit(actor(request), "BINDING_DELETE", "site:" + siteId + "/space:" + spaceId, "解绑车位硬件");
    return Map.of("siteId", siteId, "spaceId", spaceId, "removed", removed);
  }

  // ====== AI 车牌感知与车位视觉矩阵 ======

  @PostMapping("/nodes/{nodeId}/lpr/records")
  public Map<String, Object> ingestRecognitionRecords(
      @PathVariable String nodeId,
      @RequestBody List<PlateRecognitionRecord> records) {
    store.addRecognitionRecords(nodeId, records);
    return Map.of("accepted", true, "count", records.size());
  }

  @GetMapping("/sites/{siteId}/lpr/records")
  public List<PlateRecognitionRecord> getRecognitionRecords(
      @PathVariable String siteId,
      @RequestParam(defaultValue = "50") int limit) {
    return store.getRecognitionRecords(siteId, limit);
  }

  @GetMapping("/sites/{siteId}/lpr/matrix")
  public List<Map<String, Object>> getSpaceVisionMatrix(@PathVariable String siteId) {
    return store.getSpaceVisionMatrix(siteId);
  }

  // ====== 充电桩接口协议管理与车位品牌联动 ======

  @GetMapping("/charger-protocols")
  public List<ChargerProtocol> listProtocols() {
    return store.listProtocols();
  }

  @GetMapping("/charger-protocols/{id}")
  public ResponseEntity<ChargerProtocol> getProtocol(@PathVariable String id) {
    ChargerProtocol p = store.getProtocol(id);
    return p != null ? ResponseEntity.ok(p) : ResponseEntity.notFound().build();
  }

  @PostMapping("/charger-protocols")
  public ChargerProtocol saveProtocol(@RequestBody ChargerProtocol protocol, HttpServletRequest request) {
    ChargerProtocol saved = store.saveProtocol(protocol);
    store.audit(actor(request), "CHARGER_PROTOCOL_SAVE", "protocol:" + saved.protocolId(), "保存充电桩接入协议 (" + saved.brandName() + ")");
    return saved;
  }

  @PutMapping("/charger-protocols/{id}")
  public ChargerProtocol updateProtocol(
      @PathVariable String id,
      @RequestBody ChargerProtocol protocol,
      HttpServletRequest request) {
    ChargerProtocol p = new ChargerProtocol(
        id,
        protocol.brandName(),
        protocol.protocolCategory(),
        protocol.description(),
        protocol.defaultPort(),
        protocol.connectionConfigSchema(),
        protocol.statusMapping(),
        protocol.dataParserRules(),
        protocol.isBuiltin(),
        protocol.enabled(),
        protocol.createdAt(),
        Instant.now()
    );
    ChargerProtocol saved = store.saveProtocol(p);
    store.audit(actor(request), "CHARGER_PROTOCOL_UPDATE", "protocol:" + saved.protocolId(), "修改充电桩接入协议 (" + saved.brandName() + ")");
    return saved;
  }

  @DeleteMapping("/charger-protocols/{id}")
  public Map<String, Object> deleteProtocol(@PathVariable String id, HttpServletRequest request) {
    boolean removed = store.deleteProtocol(id);
    store.audit(actor(request), "CHARGER_PROTOCOL_DELETE", "protocol:" + id, "删除充电桩接入协议");
    return Map.of("protocolId", id, "removed", removed);
  }

  @PostMapping("/charger-protocols/{id}/test")
  public Map<String, Object> testProtocol(
      @PathVariable String id,
      @RequestBody(required = false) Map<String, Object> sampleData) {
    return store.testChargerProtocol(id, sampleData);
  }

  @GetMapping("/sites/{siteId}/chargers/statuses")
  public List<Map<String, Object>> getChargerStatusesForSite(@PathVariable String siteId) {
    return store.getChargerStatusesForSite(siteId);
  }

  @PostMapping("/sites/{siteId}/chargers/{spaceId}/test")
  public Map<String, Object> testSpaceCharger(
      @PathVariable String siteId,
      @PathVariable String spaceId) {
    return store.testSpaceCharger(siteId, spaceId);
  }

  private boolean isDevelopmentUnsecured(HttpServletRequest request) {
    return request.getAttribute("authRole") == null && request.getHeader("X-Api-Key") == null;
  }

  private String actor(HttpServletRequest request) {
    Object username = request.getAttribute("authUsername");
    if (username != null && !username.toString().isBlank()) {
      return username.toString();
    }
    Object role = request.getAttribute("authRole");
    return role == null ? "LOCAL_DEV" : role.toString();
  }
}
