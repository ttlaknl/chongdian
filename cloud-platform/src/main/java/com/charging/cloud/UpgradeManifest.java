package com.charging.cloud;

import jakarta.validation.constraints.NotBlank;
import jakarta.validation.constraints.NotNull;
import java.time.Instant;

public record UpgradeManifest(
    long version,
    @NotBlank String versionName,
    @NotBlank String packageUrl,
    @NotBlank String sha256Hash,
    String signature,
    long minCompatibleVersion,
    String releaseNotes,
    Instant createdAt
) {
  public UpgradeManifest {
    createdAt = createdAt == null ? Instant.now() : createdAt;
  }
}
