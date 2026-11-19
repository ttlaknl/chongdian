package com.charging.cloud;

import java.time.Instant;

public record ChargerProtocol(
    String protocolId,
    String brandName,
    String protocolCategory,
    String description,
    Integer defaultPort,
    String connectionConfigSchema,
    String statusMapping,
    String dataParserRules,
    Boolean isBuiltin,
    Boolean enabled,
    Instant createdAt,
    Instant updatedAt) {

  public ChargerProtocol {
    if (brandName == null || brandName.isBlank()) {
      brandName = "通用自定义协议";
    }
    if (protocolCategory == null || protocolCategory.isBlank()) {
      protocolCategory = "HTTP_API";
    }
    if (defaultPort == null) {
      defaultPort = 80;
    }
    if (isBuiltin == null) {
      isBuiltin = false;
    }
    if (enabled == null) {
      enabled = true;
    }
    if (createdAt == null) {
      createdAt = Instant.now();
    }
    if (updatedAt == null) {
      updatedAt = Instant.now();
    }
  }

  public ChargerProtocol(
      String protocolId,
      String brandName,
      String protocolCategory,
      String description,
      Integer defaultPort,
      String connectionConfigSchema,
      String statusMapping,
      String dataParserRules,
      Boolean isBuiltin,
      Boolean enabled) {
    this(protocolId, brandName, protocolCategory, description, defaultPort,
        connectionConfigSchema, statusMapping, dataParserRules, isBuiltin, enabled,
        Instant.now(), Instant.now());
  }
}
