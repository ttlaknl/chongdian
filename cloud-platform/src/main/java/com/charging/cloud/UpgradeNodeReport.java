package com.charging.cloud;

import jakarta.validation.constraints.NotBlank;
import java.time.Instant;

public record UpgradeNodeReport(
    @NotBlank String nodeId,
    long currentVersion,
    Long targetVersion,
    @NotBlank String state,
    String message,
    String lastError,
    Instant updatedAt
) {}
