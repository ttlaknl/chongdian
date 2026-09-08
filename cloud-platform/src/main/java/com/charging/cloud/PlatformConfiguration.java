package com.charging.cloud;

import jakarta.validation.Valid;
import jakarta.validation.constraints.NotNull;
import jakarta.validation.constraints.Size;
import java.time.Instant;
import java.util.List;
import java.util.stream.IntStream;

public record PlatformConfiguration(
    long version,
    @NotNull Instant updatedAt,
    @Valid @NotNull RuleConfig rules,
    @NotNull @Size(min = 1, max = 16) List<@Valid DeviceBinding> bindings) {
  public static PlatformConfiguration defaults() {
    List<DeviceBinding> defaultBindings = IntStream.rangeClosed(1, 6)
        .mapToObj(i -> new DeviceBinding(
            "site-demo",
            String.format("A-%02d", i),
            String.format("camera-sim-%02d", i),
            String.format("charger-sim-%02d", i),
            i - 1,
            true))
        .toList();
    return new PlatformConfiguration(0, Instant.now(), RuleConfig.defaults(), defaultBindings);
  }
}
