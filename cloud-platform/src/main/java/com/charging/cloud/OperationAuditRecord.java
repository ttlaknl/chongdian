package com.charging.cloud;
import java.time.Instant;
public record OperationAuditRecord(String actor, String action, String resource, String detail, Instant occurredAt) {}
