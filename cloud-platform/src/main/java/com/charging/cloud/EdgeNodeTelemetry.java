package com.charging.cloud;

import java.time.Instant;
import java.util.ArrayList;
import java.util.List;

public class EdgeNodeTelemetry {
    private String nodeId;
    private String siteId;
    private String nodeName;
    private String authKey; // 边缘机器授权码
    private String ipAddress;
    private double cpuUsage;
    private double memoryUsage;
    private double temperature;
    private long uptimeSeconds;
    private long framesProcessed;
    private long violationsDetected;
    private long alarmsTriggered;
    private double diskWatermark;
    private String currentVersion;
    private String status;
    private Instant lastHeartbeat;
    private List<String> assignedSpaces = new ArrayList<>();
    private Instant rebootRequestedAt;

    public EdgeNodeTelemetry() {}

    public EdgeNodeTelemetry(String nodeId, String siteId, String nodeName, String ipAddress,
                             double cpuUsage, double memoryUsage, double temperature,
                             String currentVersion, String status, Instant lastHeartbeat,
                             List<String> assignedSpaces) {
        this.nodeId = nodeId;
        this.siteId = siteId;
        this.nodeName = nodeName;
        this.ipAddress = ipAddress;
        this.cpuUsage = cpuUsage;
        this.memoryUsage = memoryUsage;
        this.temperature = temperature;
        this.currentVersion = currentVersion;
        this.status = status;
        this.lastHeartbeat = lastHeartbeat;
        this.assignedSpaces = assignedSpaces != null ? assignedSpaces : new ArrayList<>();
    }

    public String getNodeId() { return nodeId; }
    public void setNodeId(String nodeId) { this.nodeId = nodeId; }

    public String getSiteId() { return siteId; }
    public void setSiteId(String siteId) { this.siteId = siteId; }

    public String getNodeName() { return nodeName; }
    public void setNodeName(String nodeName) { this.nodeName = nodeName; }

    public String getIpAddress() { return ipAddress; }
    public void setIpAddress(String ipAddress) { this.ipAddress = ipAddress; }

    public double getCpuUsage() { return cpuUsage; }
    public void setCpuUsage(double cpuUsage) { this.cpuUsage = cpuUsage; }

    public double getMemoryUsage() { return memoryUsage; }
    public void setMemoryUsage(double memoryUsage) { this.memoryUsage = memoryUsage; }

    public double getTemperature() { return temperature; }
    public void setTemperature(double temperature) { this.temperature = temperature; }

    public String getCurrentVersion() { return currentVersion; }
    public void setCurrentVersion(String currentVersion) { this.currentVersion = currentVersion; }

    public String getStatus() { return status; }
    public void setStatus(String status) { this.status = status; }

    public Instant getLastHeartbeat() { return lastHeartbeat; }
    public void setLastHeartbeat(Instant lastHeartbeat) { this.lastHeartbeat = lastHeartbeat; }

    public List<String> getAssignedSpaces() { return assignedSpaces; }
    public void setAssignedSpaces(List<String> assignedSpaces) { this.assignedSpaces = assignedSpaces; }

    public Instant getRebootRequestedAt() { return rebootRequestedAt; }
    public void setRebootRequestedAt(Instant rebootRequestedAt) { this.rebootRequestedAt = rebootRequestedAt; }

    public String getAuthKey() { return authKey; }
    public void setAuthKey(String authKey) { this.authKey = authKey; }

    public long getUptimeSeconds() { return uptimeSeconds; }
    public void setUptimeSeconds(long uptimeSeconds) { this.uptimeSeconds = uptimeSeconds; }

    public long getFramesProcessed() { return framesProcessed; }
    public void setFramesProcessed(long framesProcessed) { this.framesProcessed = framesProcessed; }

    public long getViolationsDetected() { return violationsDetected; }
    public void setViolationsDetected(long violationsDetected) { this.violationsDetected = violationsDetected; }

    public long getAlarmsTriggered() { return alarmsTriggered; }
    public void setAlarmsTriggered(long alarmsTriggered) { this.alarmsTriggered = alarmsTriggered; }

    public double getDiskWatermark() { return diskWatermark; }
    public void setDiskWatermark(double diskWatermark) { this.diskWatermark = diskWatermark; }
}
