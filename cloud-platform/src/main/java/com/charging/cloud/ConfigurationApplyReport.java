package com.charging.cloud;

import java.time.Instant;

public record ConfigurationApplyReport(
    String nodeId,
    long version,
    String status,
    String message,
    Instant reportedAt) {
  public ConfigurationApplyReport withNodeId(String value) {
    return new ConfigurationApplyReport(value, version, status, message,
        reportedAt == null ? Instant.now() : reportedAt);
  }
}
