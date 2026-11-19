package com.charging.cloud;

import java.time.Instant;
import java.util.UUID;

public record SpaceRuntimeStatus(
    String nodeId,
    String siteId,
    String spaceId,
    String phase,
    String chargerStatus,
    String plate,
    String vehicleType,
    boolean alarming,
    UUID activeEventId,
    Instant observedAt,
    Boolean isSimulated,
    String streamType,
    String snapshotUrl,
    Double confidence) {

  public SpaceRuntimeStatus {
    isSimulated = isSimulated != null ? isSimulated : Boolean.FALSE;
    streamType = streamType != null ? streamType : "";
    snapshotUrl = snapshotUrl != null ? snapshotUrl : "";
    confidence = confidence != null ? confidence : 0.0;
  }

  public SpaceRuntimeStatus(
      String nodeId,
      String siteId,
      String spaceId,
      String phase,
      String chargerStatus,
      String plate,
      String vehicleType,
      boolean alarming,
      UUID activeEventId,
      Instant observedAt,
      Boolean isSimulated,
      String streamType) {
    this(nodeId, siteId, spaceId, phase, chargerStatus, plate, vehicleType, alarming,
        activeEventId, observedAt, isSimulated, streamType, "", 0.0);
  }

  public SpaceRuntimeStatus(
      String nodeId,
      String siteId,
      String spaceId,
      String phase,
      String chargerStatus,
      String plate,
      String vehicleType,
      boolean alarming,
      UUID activeEventId,
      Instant observedAt) {
    this(nodeId, siteId, spaceId, phase, chargerStatus, plate, vehicleType, alarming,
        activeEventId, observedAt, Boolean.FALSE, "", "", 0.0);
  }

  public SpaceRuntimeStatus withNodeId(String value) {
    return new SpaceRuntimeStatus(value, siteId, spaceId, phase, chargerStatus, plate,
        vehicleType, alarming, activeEventId, observedAt, isSimulated, streamType, snapshotUrl, confidence);
  }

  public SpaceRuntimeStatus withNodeAndSite(String nodeValue, String siteValue) {
    return new SpaceRuntimeStatus(nodeValue, siteValue, spaceId, phase, chargerStatus, plate,
        vehicleType, alarming, activeEventId, observedAt, isSimulated, streamType, snapshotUrl, confidence);
  }

  public SpaceRuntimeStatus withSnapshotUrl(String nextSnapshotUrl) {
    return new SpaceRuntimeStatus(nodeId, siteId, spaceId, phase, chargerStatus, plate,
        vehicleType, alarming, activeEventId, observedAt, isSimulated, streamType, nextSnapshotUrl, confidence);
  }
}
