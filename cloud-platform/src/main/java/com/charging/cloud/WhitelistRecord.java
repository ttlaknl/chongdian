package com.charging.cloud;

import jakarta.validation.constraints.NotBlank;
import java.time.Instant;

public record WhitelistRecord(
    @NotBlank String plate,
    String vehicleType,
    String reason,
    Instant addedAt,
    Instant expiresAt,
    boolean enabled) {
  public static WhitelistRecord create(String plate, String reason) {
    return new WhitelistRecord(plate, "ANY", reason, Instant.now(), null, true);
  }
}
