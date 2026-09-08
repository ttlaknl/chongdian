package com.charging.cloud;

import static org.springframework.test.web.servlet.request.MockMvcRequestBuilders.*;
import static org.springframework.test.web.servlet.result.MockMvcResultMatchers.*;
import org.junit.jupiter.api.Test;
import org.springframework.beans.factory.annotation.Autowired;
import org.springframework.boot.test.autoconfigure.web.servlet.WebMvcTest;
import org.springframework.context.annotation.Import;
import org.springframework.http.MediaType;
import org.springframework.test.web.servlet.MockMvc;

@WebMvcTest(CloudController.class)
@Import(CloudStore.class)
class CloudControllerTest {
  @Autowired MockMvc mvc;

  @Test void acceptsCamelCaseEdgeEventAndReturnsIt() throws Exception {
    var event = """
      {"eventId":"d6f7d1f6-2a4c-4c38-a4f8-6c1b0e60f6b4","spaceId":"A-01","plate":"粤A12345","vehicleType":"FUEL","violationType":"FUEL_VEHICLE","occurredAt":"2026-09-07T10:11:00Z","evidenceUrl":null}
      """;
    mvc.perform(post("/api/v1/events").contentType(MediaType.APPLICATION_JSON).content(event))
      .andExpect(status().isCreated()).andExpect(jsonPath("$.spaceId").value("A-01"));
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
}
