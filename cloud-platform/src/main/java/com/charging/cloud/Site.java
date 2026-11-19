package com.charging.cloud;

import jakarta.validation.constraints.NotBlank;
import jakarta.validation.constraints.Size;
import java.time.Instant;

public record Site(
    @NotBlank @Size(max = 64) String siteId,
    @NotBlank @Size(max = 128) String siteName,
    @Size(max = 256) String address,
    Boolean enabled,
    Instant createdAt) {

  public Site {
    address = address == null ? "" : address;
    enabled = enabled == null ? true : enabled;
    createdAt = createdAt == null ? Instant.now() : createdAt;
  }

  public static Site create(String siteId, String siteName, String address) {
    return new Site(siteId, siteName, address, true, Instant.now());
  }
}
