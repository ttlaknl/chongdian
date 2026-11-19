package com.charging.cloud;

import com.fasterxml.jackson.annotation.JsonIgnore;
import jakarta.validation.Valid;
import jakarta.validation.constraints.AssertTrue;
import jakarta.validation.constraints.NotNull;
import jakarta.validation.constraints.Size;
import java.time.Instant;
import java.util.HashSet;
import java.util.List;
import java.util.Locale;
import java.util.Set;

public record PlatformConfiguration(
    long version,
    @NotNull Instant updatedAt,
    @Valid @NotNull RuleConfig rules,
    @NotNull @Size(min = 0, max = 128) List<@Valid DeviceBinding> bindings,
    List<@Valid WhitelistRecord> whitelist,
    List<CameraConfig> cameras) {

  public PlatformConfiguration {
    bindings = bindings == null ? List.of() : List.copyOf(bindings);
    whitelist = whitelist == null ? List.of() : List.copyOf(whitelist);
    cameras = cameras == null ? List.of() : List.copyOf(cameras);
  }

  public PlatformConfiguration(
      long version,
      Instant updatedAt,
      RuleConfig rules,
      List<DeviceBinding> bindings,
      List<WhitelistRecord> whitelist) {
    this(version, updatedAt, rules, bindings, whitelist, List.of());
  }

  public static PlatformConfiguration defaults() {
    return new PlatformConfiguration(0, Instant.now(), RuleConfig.defaults(), List.of(), List.of(), List.of());
  }

  public PlatformConfiguration withCameras(List<CameraConfig> nextCameras) {
    return new PlatformConfiguration(version, updatedAt, rules, bindings, whitelist, nextCameras);
  }

  public PlatformConfiguration sanitizedForPublic() {
    List<CameraConfig> maskedCameras = cameras.stream()
        .map(CameraConfig::withMaskedCredentials)
        .toList();
    return new PlatformConfiguration(version, updatedAt, rules, bindings, whitelist, maskedCameras);
  }

  @AssertTrue(message = "device and whitelist bindings must be unique and relay channels must be 0..15")
  @JsonIgnore
  public boolean isInternallyConsistent() {
    Set<String> spaces = new HashSet<>();
    Set<String> camerasSet = new HashSet<>();
    Set<String> chargers = new HashSet<>();
    Set<Integer> relays = new HashSet<>();
    for (DeviceBinding binding : bindings) {
      if (binding == null || binding.spaceId() == null || !spaces.add(binding.spaceId())) return false;
      if (Boolean.TRUE.equals(binding.enabled())) {
        if (binding.cameraId() == null || !camerasSet.add(binding.cameraId())) return false;
        if (binding.chargerId() == null || !chargers.add(binding.chargerId())) return false;
        if (binding.relayChannel() == null || binding.relayChannel() < 0 || binding.relayChannel() > 15 || !relays.add(binding.relayChannel())) return false;
      }
    }
    Set<String> plates = new HashSet<>();
    for (WhitelistRecord entry : whitelist) {
      if (entry == null || entry.plate() == null || !plates.add(entry.plate().trim().toUpperCase(Locale.ROOT))) return false;
    }
    return true;
  }
}
