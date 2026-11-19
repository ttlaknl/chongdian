package com.charging.cloud;

import com.fasterxml.jackson.annotation.JsonAlias;
import com.fasterxml.jackson.annotation.JsonIgnoreProperties;
import java.time.Instant;

@JsonIgnoreProperties(ignoreUnknown = true)
public class PlateRecognitionRecord {
    private String recordId;
    private String siteId;
    private String spaceId;
    private String cameraId;
    private String plate;
    private String plateColor; // GREEN, BLUE, YELLOW, WHITE, UNKNOWN
    private String vehicleType; // NEW_ENERGY, FUEL, UNKNOWN
    private double confidence; // 0.0 - 100.0 (or normalized 0..1 accepted on ingest)
    private String snapshotUrl;
    private String direction; // ENTER, LEAVE, STATIONARY
    private String judgmentStatus; // NORMAL_CHARGING, GRACE_PERIOD, VIOLATION_ALARMING, WHITELIST_ALLOWED, NORMAL
    private boolean simulated;
    private Instant recognizedAt;

    public PlateRecognitionRecord() {}

    public PlateRecognitionRecord(String recordId, String siteId, String spaceId, String cameraId,
                                  String plate, String plateColor, String vehicleType,
                                  double confidence, String snapshotUrl, String direction,
                                  String judgmentStatus, Instant recognizedAt) {
        this(recordId, siteId, spaceId, cameraId, plate, plateColor, vehicleType, confidence,
            snapshotUrl, direction, judgmentStatus, false, recognizedAt);
    }

    public PlateRecognitionRecord(String recordId, String siteId, String spaceId, String cameraId,
                                  String plate, String plateColor, String vehicleType,
                                  double confidence, String snapshotUrl, String direction,
                                  String judgmentStatus, boolean simulated, Instant recognizedAt) {
        this.recordId = recordId;
        this.siteId = siteId;
        this.spaceId = spaceId;
        this.cameraId = cameraId;
        this.plate = plate;
        this.plateColor = plateColor;
        this.vehicleType = vehicleType;
        this.confidence = confidence;
        this.snapshotUrl = snapshotUrl;
        this.direction = direction;
        this.judgmentStatus = judgmentStatus;
        this.simulated = simulated;
        this.recognizedAt = recognizedAt;
    }

    public String getRecordId() { return recordId; }
    public void setRecordId(String recordId) { this.recordId = recordId; }

    public String getSiteId() { return siteId; }
    public void setSiteId(String siteId) { this.siteId = siteId; }

    public String getSpaceId() { return spaceId; }
    public void setSpaceId(String spaceId) { this.spaceId = spaceId; }

    public String getCameraId() { return cameraId; }
    public void setCameraId(String cameraId) { this.cameraId = cameraId; }

    public String getPlate() { return plate; }
    public void setPlate(String plate) { this.plate = plate; }

    public String getPlateColor() { return plateColor; }
    public void setPlateColor(String plateColor) { this.plateColor = plateColor; }

    public String getVehicleType() { return vehicleType; }
    public void setVehicleType(String vehicleType) { this.vehicleType = vehicleType; }

    public double getConfidence() { return confidence; }
    public void setConfidence(double confidence) { this.confidence = confidence; }

    public String getSnapshotUrl() { return snapshotUrl; }
    public void setSnapshotUrl(String snapshotUrl) { this.snapshotUrl = snapshotUrl; }

    public String getDirection() { return direction; }
    public void setDirection(String direction) { this.direction = direction; }

    public String getJudgmentStatus() { return judgmentStatus; }
    public void setJudgmentStatus(String judgmentStatus) { this.judgmentStatus = judgmentStatus; }

    public boolean isSimulated() { return simulated; }
    @JsonAlias({"isSimulated", "simulated"})
    public void setSimulated(boolean simulated) { this.simulated = simulated; }

    public Instant getRecognizedAt() { return recognizedAt; }
    public void setRecognizedAt(Instant recognizedAt) { this.recognizedAt = recognizedAt; }

    // 前端兼容别名序列化
    public String getPlateNumber() { return plate; }
    public String getMotionType() { return direction; }
    public String getVerdictStatus() { return judgmentStatus; }
}
