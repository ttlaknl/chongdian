package com.charging.cloud;

import java.time.Instant;
import java.util.regex.Matcher;
import java.util.regex.Pattern;

public class CameraConfig {
    private static final Pattern URL_CREDENTIAL_PATTERN =
        Pattern.compile("^([a-zA-Z][a-zA-Z0-9+.-]*://)([^/@]+)@(.+)$");

    private String cameraId;
    private String cameraName;
    private String siteId;
    private String spaceId;
    private String nodeId; // 归属边缘工控机
    private String protocol; // RTSP, GB28181
    private String streamUrl;
    private String sipDeviceId;
    private String resolution;
    private String codec; // H.264, H.265
    private String status; // ONLINE, PULLING, OFFLINE
    private Instant lastFrameAt;
    private String roiJson; // JSON array [x1, y1, x2, y2] in 0.0..1.0
    private Boolean enabled = Boolean.TRUE;

    public CameraConfig() {}

    public CameraConfig(String cameraId, String cameraName, String siteId, String spaceId,
                        String protocol, String streamUrl, String sipDeviceId,
                        String resolution, String codec, String status, Instant lastFrameAt) {
        this(cameraId, cameraName, siteId, spaceId, null, protocol, streamUrl, sipDeviceId,
            resolution, codec, status, lastFrameAt, "", Boolean.TRUE);
    }

    public CameraConfig(String cameraId, String cameraName, String siteId, String spaceId,
                        String nodeId, String protocol, String streamUrl, String sipDeviceId,
                        String resolution, String codec, String status, Instant lastFrameAt,
                        String roiJson, Boolean enabled) {
        this.cameraId = cameraId;
        this.cameraName = cameraName;
        this.siteId = siteId;
        this.spaceId = spaceId;
        this.nodeId = nodeId;
        this.protocol = protocol;
        this.streamUrl = streamUrl;
        this.sipDeviceId = sipDeviceId;
        this.resolution = resolution;
        this.codec = codec;
        this.status = status;
        this.lastFrameAt = lastFrameAt;
        this.roiJson = roiJson == null ? "" : roiJson;
        this.enabled = enabled == null ? Boolean.TRUE : enabled;
    }

    public CameraConfig withMaskedCredentials() {
        return new CameraConfig(
            cameraId,
            cameraName,
            siteId,
            spaceId,
            nodeId,
            protocol,
            maskStreamUrlCredentials(streamUrl),
            sipDeviceId,
            resolution,
            codec,
            status,
            lastFrameAt,
            roiJson,
            enabled
        );
    }

    public static String maskStreamUrlCredentials(String url) {
        if (url == null || url.isBlank()) return "";
        Matcher matcher = URL_CREDENTIAL_PATTERN.matcher(url.trim());
        if (matcher.matches()) {
            return matcher.group(1) + "***:***@" + matcher.group(3);
        }
        return url;
    }

    public String getCameraId() { return cameraId; }
    public void setCameraId(String cameraId) { this.cameraId = cameraId; }

    public String getCameraName() { return cameraName; }
    public void setCameraName(String cameraName) { this.cameraName = cameraName; }

    public String getSiteId() { return siteId; }
    public void setSiteId(String siteId) { this.siteId = siteId; }

    public String getSpaceId() { return spaceId; }
    public void setSpaceId(String spaceId) { this.spaceId = spaceId; }

    public String getNodeId() { return nodeId; }
    public void setNodeId(String nodeId) { this.nodeId = nodeId; }

    public String getProtocol() { return protocol; }
    public void setProtocol(String protocol) { this.protocol = protocol; }

    public String getStreamUrl() { return streamUrl; }
    public void setStreamUrl(String streamUrl) { this.streamUrl = streamUrl; }

    public String getSipDeviceId() { return sipDeviceId; }
    public void setSipDeviceId(String sipDeviceId) { this.sipDeviceId = sipDeviceId; }

    public String getResolution() { return resolution; }
    public void setResolution(String resolution) { this.resolution = resolution; }

    public String getCodec() { return codec; }
    public void setCodec(String codec) { this.codec = codec; }

    public String getStatus() { return status; }
    public void setStatus(String status) { this.status = status; }

    public Instant getLastFrameAt() { return lastFrameAt; }
    public void setLastFrameAt(Instant lastFrameAt) { this.lastFrameAt = lastFrameAt; }

    public String getRoiJson() { return roiJson; }
    public void setRoiJson(String roiJson) { this.roiJson = roiJson == null ? "" : roiJson; }

    public Boolean getEnabled() { return enabled == null ? Boolean.TRUE : enabled; }
    public void setEnabled(Boolean enabled) { this.enabled = enabled == null ? Boolean.TRUE : enabled; }
}
