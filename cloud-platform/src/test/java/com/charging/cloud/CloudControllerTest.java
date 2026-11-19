package com.charging.cloud;

import static org.springframework.test.web.servlet.request.MockMvcRequestBuilders.*;
import static org.springframework.test.web.servlet.result.MockMvcResultMatchers.*;
import org.junit.jupiter.api.Test;
import org.springframework.beans.factory.annotation.Autowired;
import org.springframework.boot.test.autoconfigure.web.servlet.WebMvcTest;
import org.springframework.context.annotation.Import;
import org.springframework.http.MediaType;
import org.springframework.test.web.servlet.MockMvc;
import org.springframework.test.context.DynamicPropertyRegistry;
import org.springframework.test.context.DynamicPropertySource;
import java.nio.file.Path;
import java.util.UUID;

@WebMvcTest(CloudController.class)
@Import(CloudStore.class)
class CloudControllerTest {
  private static final Path TEST_STORAGE = Path.of("target", "test-data", "controller-" + UUID.randomUUID());

  @DynamicPropertySource
  static void storageProperties(DynamicPropertyRegistry registry) {
    registry.add("app.storage-dir", TEST_STORAGE::toString);
  }

  @Autowired MockMvc mvc;

  @Test void acceptsCamelCaseEdgeEventAndReturnsIt() throws Exception {
    var event = """
      {"eventId":"d6f7d1f6-2a4c-4c38-a4f8-6c1b0e60f6b4","sessionId":"A-01:100","spaceId":"A-01","plate":"粤A12345","vehicleType":"FUEL","violationType":"FUEL_VEHICLE","occurredAt":"2026-09-07T10:11:00Z","evidenceUrl":null,"status":"ALARMING","clearedAt":null,"updatedAt":"2026-09-07T10:11:00Z"}
      """;
    mvc.perform(post("/api/v1/events").contentType(MediaType.APPLICATION_JSON).content(event))
      .andExpect(status().isCreated()).andExpect(jsonPath("$.spaceId").value("A-01"));
    mvc.perform(post("/api/v1/events/d6f7d1f6-2a4c-4c38-a4f8-6c1b0e60f6b4/acknowledge"))
      .andExpect(status().isOk()).andExpect(jsonPath("$.status").value("ACKNOWLEDGED"));
    mvc.perform(get("/api/v1/events")).andExpect(status().isOk()).andExpect(jsonPath("$[0].eventId").value("d6f7d1f6-2a4c-4c38-a4f8-6c1b0e60f6b4"));
  }

  @Test void rejectsInvalidRuleThreshold() throws Exception {
    var rules = """
      {"unchargedGraceSeconds":-1,"completedGraceSeconds":900,"vehicleEnterConfirmSeconds":3,"vehicleLeaveConfirmSeconds":5,"alarmRepeatSeconds":60,"alarmAutoStopSeconds":300,"unknownVehicleAction":"WAIT","unknownStatusAction":"WAIT","faultStatusAction":"ALLOW"}
      """;
    mvc.perform(put("/api/v1/rules").contentType(MediaType.APPLICATION_JSON).content(rules)).andExpect(status().isBadRequest());
  }

  @Test void returnsVersionedConfigurationWithDeviceBinding() throws Exception {
    mvc.perform(get("/api/v1/configuration")).andExpect(status().isOk())
      .andExpect(jsonPath("$.version").isNumber())
      .andExpect(jsonPath("$.bindings").isArray());
  }

  @Test void updatesConfigurationAndIncrementsVersion() throws Exception {
    var configuration = """
      {"version":0,"updatedAt":"2026-09-07T10:00:00Z","rules":{"unchargedGraceSeconds":600,"completedGraceSeconds":900,"vehicleEnterConfirmSeconds":3,"vehicleLeaveConfirmSeconds":5,"alarmRepeatSeconds":60,"alarmAutoStopSeconds":300,"unknownVehicleAction":"WAIT","unknownStatusAction":"WAIT","faultStatusAction":"ALLOW"},"bindings":[{"siteId":"site-1","spaceId":"B-01","cameraId":"camera-01","chargerId":"charger-01","relayChannel":1,"enabled":true}]}
      """;
    mvc.perform(put("/api/v1/configuration").contentType(MediaType.APPLICATION_JSON).content(configuration))
      .andExpect(status().isOk()).andExpect(jsonPath("$.version").isNumber())
      .andExpect(jsonPath("$.bindings[0].spaceId").value("B-01"));
  }

  @Test void rejectsDuplicateEnabledDeviceBindings() throws Exception {
    var configuration = """
      {"version":0,"updatedAt":"2026-09-07T10:00:00Z","rules":{"unchargedGraceSeconds":600,"completedGraceSeconds":900,"vehicleEnterConfirmSeconds":3,"vehicleLeaveConfirmSeconds":5,"alarmRepeatSeconds":60,"alarmAutoStopSeconds":300,"unknownVehicleAction":"WAIT","unknownStatusAction":"WAIT","faultStatusAction":"ALLOW"},"bindings":[{"siteId":"site-1","spaceId":"A-01","cameraId":"camera-01","chargerId":"charger-01","relayChannel":1,"enabled":true},{"siteId":"site-1","spaceId":"A-02","cameraId":"camera-01","chargerId":"charger-02","relayChannel":2,"enabled":true}],"whitelist":[]}
      """;
    mvc.perform(put("/api/v1/configuration").contentType(MediaType.APPLICATION_JSON).content(configuration))
      .andExpect(status().isBadRequest());
  }

  @Test void managesWhitelistAndRetrievesDashboardStats() throws Exception {
    var wl = """
      {"plate":"粤B88888","vehicleType":"FUEL","reason":"园区巡逻工程车","addedAt":"2026-09-08T00:00:00Z","expiresAt":null,"enabled":true}
      """;
    mvc.perform(post("/api/v1/whitelist").contentType(MediaType.APPLICATION_JSON).content(wl))
      .andExpect(status().isCreated()).andExpect(jsonPath("$.plate").value("粤B88888"));

    mvc.perform(get("/api/v1/whitelist"))
      .andExpect(status().isOk()).andExpect(jsonPath("$[0].plate").value("粤B88888"));

    mvc.perform(get("/api/v1/dashboard/stats"))
      .andExpect(status().isOk()).andExpect(jsonPath("$.totalEvents").isNumber());

    mvc.perform(delete("/api/v1/whitelist/粤B88888"))
      .andExpect(status().isOk()).andExpect(jsonPath("$.removed").value(true));
  }

  @Test void exportsViolationEventsAsCsv() throws Exception {
    mvc.perform(get("/api/v1/events/export"))
      .andExpect(status().isOk())
      .andExpect(header().string("Content-Disposition", "attachment; filename=violation_ledger.csv"))
      .andExpect(content().contentTypeCompatibleWith("text/csv"));
  }

  @Test void acceptsRealtimeSpaceStatusAndConfigurationReport() throws Exception {
    var statuses = """
      [{"nodeId":"ignored","siteId":"site-1","spaceId":"A-01","phase":"OCCUPIED","chargerStatus":"IDLE","plate":"粤A12345","vehicleType":"FUEL","alarming":true,"activeEventId":"d6f7d1f6-2a4c-4c38-a4f8-6c1b0e60f6b4","observedAt":"2026-09-08T03:00:00Z"}]
      """;
    mvc.perform(post("/api/v1/nodes/edge-01/spaces/status").contentType(MediaType.APPLICATION_JSON).content(statuses))
      .andExpect(status().isOk()).andExpect(jsonPath("$.count").value(1));
    mvc.perform(get("/api/v1/spaces/status"))
      .andExpect(status().isOk()).andExpect(jsonPath("$[0].nodeId").value("edge-01"))
      .andExpect(jsonPath("$[0].alarming").value(true));

    var report = """
      {"nodeId":"edge-01","version":3,"status":"APPLIED","message":"ok","reportedAt":"2026-09-08T03:00:00Z"}
      """;
    mvc.perform(post("/api/v1/nodes/edge-01/configuration-status").contentType(MediaType.APPLICATION_JSON).content(report))
      .andExpect(status().isOk()).andExpect(jsonPath("$.version").value(3));
    mvc.perform(get("/api/v1/nodes/configuration-status"))
      .andExpect(status().isOk()).andExpect(jsonPath("$.edge-01.status").value("APPLIED"));
  }

  @Test void exposesOperationAuditRecords() throws Exception {
    mvc.perform(get("/api/v1/audit"))
      .andExpect(status().isOk()).andExpect(jsonPath("$").isArray());
  }

  @Test void exposesSseStreamsForSpacesAndEvents() throws Exception {
    mvc.perform(get("/api/v1/stream/spaces"))
      .andExpect(status().isOk())
      .andExpect(request().asyncStarted());

    mvc.perform(get("/api/v1/stream/events"))
      .andExpect(status().isOk())
      .andExpect(request().asyncStarted());
  }

  @Test void uploadsAndServesSnapshotsEvidencesAndSiteScopedWhitelist() throws Exception {
    byte[] jpeg = new byte[] { (byte) 0xFF, (byte) 0xD8, (byte) 0xFF, (byte) 0xD9 };
    mvc.perform(post("/api/v1/nodes/edge-i3/spaces/A-01/snapshot")
        .contentType(MediaType.IMAGE_JPEG)
        .content(jpeg))
      .andExpect(status().isOk())
      .andExpect(jsonPath("$.snapshotUrl").value("/api/v1/sites/default/spaces/A-01/snapshot"));

    mvc.perform(get("/api/v1/sites/default/spaces/A-01/snapshot"))
      .andExpect(status().isOk())
      .andExpect(content().contentTypeCompatibleWith(MediaType.IMAGE_JPEG));

    mvc.perform(post("/api/v1/evidences?filename=ev_01.jpg")
        .contentType(MediaType.IMAGE_JPEG)
        .content(jpeg))
      .andExpect(status().isOk())
      .andExpect(jsonPath("$.evidenceUrl").value("/api/v1/evidences/ev_01.jpg"));

    mvc.perform(get("/api/v1/evidences/ev_01.jpg"))
      .andExpect(status().isOk())
      .andExpect(content().contentTypeCompatibleWith(MediaType.IMAGE_JPEG));

    mvc.perform(get("/api/v1/upgrade/manifest"))
      .andExpect(status().isNoContent());

    mvc.perform(get("/api/v1/sites/summary"))
      .andExpect(status().isOk())
      .andExpect(jsonPath("$[0].siteId").value("default"));
  }
}
