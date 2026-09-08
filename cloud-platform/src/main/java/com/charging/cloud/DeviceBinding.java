package com.charging.cloud;

import jakarta.validation.constraints.Min;
import jakarta.validation.constraints.NotBlank;
import jakarta.validation.constraints.NotNull;

public record DeviceBinding(
    @NotBlank String siteId,
    @NotBlank String spaceId,
    @NotBlank String cameraId,
    @NotBlank String chargerId,
    @NotNull @Min(0) Integer relayChannel,
    @NotNull Boolean enabled) {}
