package com.charging.cloud;
import java.time.Instant; import java.util.UUID;
public record ViolationEvent(UUID eventId, String spaceId, String plate, String vehicleType, String violationType, Instant occurredAt, String evidenceUrl) {}
