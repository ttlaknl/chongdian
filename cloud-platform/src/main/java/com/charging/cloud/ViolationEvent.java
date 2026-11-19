package com.charging.cloud;

import jakarta.validation.constraints.NotBlank;
import jakarta.validation.constraints.NotNull;
import java.time.Instant;
import java.util.UUID;

public record ViolationEvent(
    UUID eventId,
    @NotBlank String sessionId,
    String siteId,
    @NotBlank String spaceId,
    String plate,
    @NotBlank String vehicleType,
    @NotBlank String violationType,
    Instant occurredAt,
    String evidenceUrl,
    @NotNull EventStatus status,
    Instant clearedAt,
    Instant updatedAt,
    String acknowledgedBy,
    Instant acknowledgedAt) {

  public ViolationEvent {
    siteId = siteId == null || siteId.isBlank() ? "default" : siteId;
    occurredAt = occurredAt == null ? Instant.now() : occurredAt;
    updatedAt = updatedAt == null ? Instant.now() : updatedAt;
  }

  public ViolationEvent(
      UUID eventId,
      String sessionId,
      String siteId,
      String spaceId,
      String plate,
      String vehicleType,
      String violationType,
      Instant occurredAt,
      String evidenceUrl,
      EventStatus status,
      Instant clearedAt,
      Instant updatedAt) {
    this(eventId, sessionId, siteId, spaceId, plate, vehicleType, violationType,
        occurredAt, evidenceUrl, status, clearedAt, updatedAt, null, null);
  }

  public ViolationEvent(
      UUID eventId,
      String sessionId,
      String spaceId,
      String plate,
      String vehicleType,
      String violationType,
      Instant occurredAt,
      String evidenceUrl,
      EventStatus status,
      Instant clearedAt,
      Instant updatedAt) {
    this(eventId, sessionId, "default", spaceId, plate, vehicleType, violationType,
        occurredAt, evidenceUrl, status, clearedAt, updatedAt, null, null);
  }

  public ViolationEvent(
      UUID eventId,
      String spaceId,
      String plate,
      String vehicleType,
      String violationType,
      Instant occurredAt,
      String evidenceUrl) {
    this(eventId, "", "default", spaceId, plate, vehicleType, violationType,
        occurredAt, evidenceUrl, EventStatus.ALARMING, null, occurredAt, null, null);
  }

  public ViolationEvent withSiteId(String value) {
    return new ViolationEvent(eventId, sessionId, value, spaceId, plate, vehicleType,
        violationType, occurredAt, evidenceUrl, status, clearedAt, updatedAt, acknowledgedBy, acknowledgedAt);
  }
}
