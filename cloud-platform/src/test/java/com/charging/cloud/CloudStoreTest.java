package com.charging.cloud;

import static org.junit.jupiter.api.Assertions.*;
import java.time.Instant;
import java.util.UUID;
import org.junit.jupiter.api.Test;

class CloudStoreTest {
  @Test void storesEventsNewestFirstAndTracksHeartbeat() {
    CloudStore store = new CloudStore();
    store.add(new ViolationEvent(UUID.randomUUID(), "A-01", "粤A12345", "FUEL", "FUEL_VEHICLE", Instant.parse("2026-01-01T00:00:00Z"), "/a.jpg"));
    store.add(new ViolationEvent(UUID.randomUUID(), "A-02", "粤A12346", "NEW_ENERGY", "NOT_CHARGING_OVERTIME", Instant.parse("2026-01-01T00:01:00Z"), "/b.jpg"));
    store.heartbeat("edge-01");
    assertEquals("A-02", store.events().getFirst().spaceId());
    assertTrue(store.heartbeats().containsKey("edge-01"));

    String csv = store.exportEventsCsv(null, null, null);
    assertTrue(csv.contains("事件ID,车位编号,车牌号码"));
    assertTrue(csv.contains("粤A12345"));
    assertTrue(csv.contains("燃油车"));
    assertTrue(csv.contains("燃油车占用充电位"));
  }
}
