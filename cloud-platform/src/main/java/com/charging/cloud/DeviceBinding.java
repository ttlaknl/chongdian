package com.charging.cloud;

import jakarta.validation.constraints.Max;
import jakarta.validation.constraints.Min;
import jakarta.validation.constraints.NotBlank;
import jakarta.validation.constraints.NotNull;

public record DeviceBinding(
    @NotBlank String siteId,
    @NotBlank String spaceId,
    @NotBlank String cameraId,
    @NotBlank String chargerId,
    @NotNull @Min(0) @Max(15) Integer relayChannel,
    @Min(0) @Max(15) Integer speakerChannel,
    @NotNull Boolean enabled,
    String chargerBrand,
    String protocolId,
    String connectorId,
    String chargerParams) {

  public DeviceBinding(
      String siteId,
      String spaceId,
      String cameraId,
      String chargerId,
      Integer relayChannel,
      Integer speakerChannel,
      Boolean enabled) {
    this(siteId, spaceId, cameraId, chargerId, relayChannel, speakerChannel, enabled,
        "GENERIC", "proto-modbus-tcp", "1", "{}");
  }

  public DeviceBinding(
      String siteId,
      String spaceId,
      String cameraId,
      String chargerId,
      Integer relayChannel,
      Boolean enabled) {
    this(siteId, spaceId, cameraId, chargerId, relayChannel, relayChannel, enabled,
        "GENERIC", "proto-modbus-tcp", "1", "{}");
  }

  @Override
  public Integer speakerChannel() {
    return speakerChannel != null ? speakerChannel : relayChannel;
  }

  @Override
  public String chargerBrand() {
    return chargerBrand != null && !chargerBrand.isBlank() ? chargerBrand : "GENERIC";
  }

  @Override
  public String protocolId() {
    return protocolId != null && !protocolId.isBlank() ? protocolId : "proto-modbus-tcp";
  }

  @Override
  public String connectorId() {
    return connectorId != null && !connectorId.isBlank() ? connectorId : "1";
  }

  @Override
  public String chargerParams() {
    return chargerParams != null && !chargerParams.isBlank() ? chargerParams : "{}";
  }
}
