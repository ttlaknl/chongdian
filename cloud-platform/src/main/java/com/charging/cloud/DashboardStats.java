package com.charging.cloud;

import java.util.Map;

public record DashboardStats(
    long totalEvents,
    long todayEvents,
    Map<String, Long> violationTypeCounts,
    Map<String, Long> spaceViolationCounts,
    int onlineNodes,
    int totalNodes) {}
