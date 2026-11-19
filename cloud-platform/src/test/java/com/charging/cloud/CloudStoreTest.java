package com.charging.cloud;

import static org.junit.jupiter.api.Assertions.*;
import java.time.Instant;
import java.util.UUID;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.io.TempDir;
import java.nio.file.Path;
import java.nio.file.Files;

class CloudStoreTest {
  @TempDir Path tempDir;

  private CloudStore store() {
    return new CloudStore(tempDir.resolve(UUID.randomUUID().toString()).toString());
  }

  @Test void storesEventsNewestFirstAndTracksHeartbeat() {
    CloudStore store = store();
    store.add(new ViolationEvent(UUID.randomUUID(), "A-01", "粤A12345", "FUEL", "FUEL_VEHICLE", Instant.parse("2026-01-01T00:00:00Z"), "/a.jpg"));
    store.add(new ViolationEvent(UUID.randomUUID(), "A-02", "粤A12346", "NEW_ENERGY", "NOT_CHARGING_OVERTIME", Instant.parse("2026-01-01T00:01:00Z"), "/b.jpg"));
    store.heartbeat("edge-01");
    assertEquals("A-02", store.events().getFirst().spaceId());
    assertTrue(store.heartbeats().containsKey("edge-01"));

    String csv = store.exportEventsCsv(null, null, null);
    assertTrue(csv.contains("事件ID,车位编号,车牌号码"));
    assertTrue(csv.contains("事件状态"));
    assertTrue(csv.contains("粤A12345"));
    assertTrue(csv.contains("燃油车"));
    assertTrue(csv.contains("燃油车占用充电位"));
  }

  @Test void updatesSameSessionInsteadOfCreatingDuplicateEvent() {
    CloudStore store = store();
    UUID firstId = UUID.randomUUID();
    Instant openedAt = Instant.parse("2026-09-08T01:00:00Z");
    store.add(new ViolationEvent(firstId, "A-01:100", "A-01", "粤A12345", "FUEL",
        "FUEL_VEHICLE", openedAt, null, EventStatus.ALARMING, null, openedAt));
    ViolationEvent cleared = store.add(new ViolationEvent(UUID.randomUUID(), "A-01:100", "A-01",
        "粤A12345", "FUEL", "FUEL_VEHICLE", openedAt, null, EventStatus.CLEARED,
        Instant.parse("2026-09-08T01:05:00Z"), Instant.parse("2026-09-08T01:05:00Z")));
    assertEquals(firstId, cleared.eventId());
    assertEquals(EventStatus.CLEARED, cleared.status());
    assertEquals(1, store.events().size());
  }

  @Test void fileStorageFailureIsNotSilentlyIgnored() throws Exception {
    Path invalidDirectory = tempDir.resolve("not-a-directory");
    Files.writeString(invalidDirectory, "file");
    CloudStore broken = new CloudStore(invalidDirectory.toString());
    PlatformConfiguration current = broken.configuration();
    assertThrows(IllegalStateException.class, () -> broken.updateConfiguration(current));
    assertEquals(0, broken.configuration().version());
  }

  @Test void auditLogAppendsAndRestoresAllRecords() {
    String directory = tempDir.resolve("audit-store").toString();
    CloudStore first = new CloudStore(directory);
    first.audit("ADMIN", "RULE_UPDATE", "rules", "v1");
    first.audit("ADMIN", "WHITELIST_UPSERT", "plate:A", "test");
    CloudStore restored = new CloudStore(directory);
    restored.init();
    assertEquals(2, restored.auditRecords().size());
  }

  @Test void spaceStatusesStoreAndRetrieveWithProvenance() {
    CloudStore store = store();
    SpaceRuntimeStatus realStatus = new SpaceRuntimeStatus(
        "edge-01", "site-01", "A-01", "OCCUPIED", "CHARGING", "粤B12345",
        "NEW_ENERGY", false, null, Instant.now(), false, "GB28181"
    );
    SpaceRuntimeStatus mockStatus = new SpaceRuntimeStatus(
        "edge-01", "site-01", "A-02", "OCCUPIED", "IDLE", "粤B99999",
        "FUEL", true, UUID.randomUUID(), Instant.now(), true, "[MOCK_SIMULATED] 录像回放数据集"
    );

    store.updateSpaceStatuses("edge-01", java.util.List.of(realStatus, mockStatus));
    java.util.List<SpaceRuntimeStatus> statuses = store.spaceStatuses();
    assertEquals(2, statuses.size());

    SpaceRuntimeStatus retrievedReal = statuses.stream().filter(s -> "A-01".equals(s.spaceId())).findFirst().orElseThrow();
    assertFalse(retrievedReal.isSimulated());
    assertEquals("GB28181", retrievedReal.streamType());

    SpaceRuntimeStatus retrievedMock = statuses.stream().filter(s -> "A-02".equals(s.spaceId())).findFirst().orElseThrow();
    assertTrue(retrievedMock.isSimulated());
    assertEquals("[MOCK_SIMULATED] 录像回放数据集", retrievedMock.streamType());
  }

  @Test void fileModePersistsAllEntitiesAndDoesNotFabricateManifestOrNodes() {
    String directory = tempDir.resolve("full-persist").toString();
    CloudStore first = new CloudStore(directory);
    first.init();

    // No fake upgrade manifest or fake totalNodes=1 on clean init
    assertNull(first.getUpgradeManifest());
    assertEquals(0, first.dashboardStats().totalNodes());

    Site site2 = first.createSite(Site.create("site-2", "二期站", "B区"));
    assertEquals("site-2", site2.siteId());
    first.addOrUpdateNode("site-2", new EdgeNodeTelemetry(
        "edge-i3-01", "site-2", "I3 测试节点", "192.168.1.50",
        10.0, 20.0, 40.0, "v1", "ONLINE", Instant.now(), java.util.List.of("B-01")
    ));
    CameraConfig cam = new CameraConfig(
        "CAM-B01", "B01枪机", "site-2", "B-01", "edge-i3-01", "RTSP",
        "rtsp://admin:secret123@192.168.1.64:554/ch1", "", "1920x1080", "H.264",
        "REGISTERED", null, "[0.1,0.1,0.9,0.9]", true
    );
    first.saveCamera("site-2", cam);
    first.saveSingleBinding("site-2", new DeviceBinding("site-2", "B-01", "CAM-B01", "CHG-B01", 0, true));
    first.saveWhitelist("site-2", WhitelistRecord.create("粤B77777", "site2-vip"));
    assertFalse(first.isWhitelisted("default", "粤B77777"));
    assertTrue(first.isWhitelisted("site-2", "粤B77777"));

    byte[] minimalJpeg = new byte[] { (byte) 0xFF, (byte) 0xD8, (byte) 0xFF, (byte) 0xD9 };
    String snapUrl = first.saveSpaceSnapshot("site-2", "B-01", minimalJpeg);
    assertEquals("/api/v1/sites/site-2/spaces/B-01/snapshot", snapUrl);

    first.addRecognitionRecords("edge-i3-01", java.util.List.of(new PlateRecognitionRecord(
        "rec-1", "site-2", "B-01", "CAM-B01", "粤BD12345", "GREEN", "NEW_ENERGY",
        96.5, "", "ENTER", "NORMAL_CHARGING", false, Instant.now()
    )));

    // Verify public config masks camera credentials
    PlatformConfiguration masked = first.configurationForNode("edge-i3-01").sanitizedForPublic();
    assertEquals("rtsp://***:***@192.168.1.64:554/ch1", masked.cameras().getFirst().getStreamUrl());

    // Reopen store from disk and verify everything restored cleanly
    CloudStore restored = new CloudStore(directory);
    restored.init();
    assertNotNull(restored.getSite("site-2"));
    assertEquals(1, restored.getNodesForSite("site-2").size());
    assertEquals(1, restored.getCamerasForSite("site-2").size());
    assertEquals("[0.1,0.1,0.9,0.9]", restored.getCamerasForSite("site-2").getFirst().getRoiJson());
    assertTrue(restored.isWhitelisted("site-2", "粤B77777"));
    assertEquals(1, restored.getRecognitionRecords("site-2", 10).size());
    assertEquals("粤BD12345", restored.getRecognitionRecords("site-2", 10).getFirst().getPlate());
    assertNotNull(restored.getSpaceSnapshotFile("site-2", "B-01"));
  }
}
