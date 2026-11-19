package com.charging.cloud;

import static org.springframework.test.web.servlet.request.MockMvcRequestBuilders.*;
import static org.springframework.test.web.servlet.result.MockMvcResultMatchers.*;
import java.nio.file.Path;
import java.util.UUID;
import org.junit.jupiter.api.Test;
import org.springframework.beans.factory.annotation.Autowired;
import org.springframework.boot.test.autoconfigure.web.servlet.WebMvcTest;
import org.springframework.context.annotation.Import;
import org.springframework.http.MediaType;
import org.springframework.mock.web.MockHttpSession;
import org.springframework.test.context.DynamicPropertyRegistry;
import org.springframework.test.context.DynamicPropertySource;
import org.springframework.test.web.servlet.MockMvc;

@WebMvcTest(AuthController.class)
@Import({CloudStore.class, ApiKeyAuthenticationFilter.class, TokenService.class})
class AuthControllerTest {
  private static final Path TEST_STORAGE = Path.of("target", "test-data", "auth-" + UUID.randomUUID());

  @DynamicPropertySource
  static void properties(DynamicPropertyRegistry registry) {
    registry.add("app.storage-dir", TEST_STORAGE::toString);
    registry.add("app.security.enabled", () -> true);
    registry.add("app.security.admin-username", () -> "admin");
    registry.add("app.security.admin-password", () -> "Admin@2026!");
    registry.add("app.security.edge-api-key", () -> "edge-secret");
  }

  @Autowired MockMvc mvc;

  @Test void rejectsInvalidKeyAndCreatesAdminSessionForValidCredentials() throws Exception {
    mvc.perform(post("/api/v1/auth/login").contentType(MediaType.APPLICATION_JSON)
        .content("{\"username\":\"admin\",\"password\":\"wrong\"}"))
      .andExpect(status().isUnauthorized());

    MockHttpSession session = (MockHttpSession) mvc.perform(post("/api/v1/auth/login")
        .contentType(MediaType.APPLICATION_JSON).content("{\"username\":\"admin\",\"password\":\"Admin@2026!\"}"))
      .andExpect(status().isOk()).andExpect(jsonPath("$.role").value("ADMIN"))
      .andReturn().getRequest().getSession(false);
    org.junit.jupiter.api.Assertions.assertEquals("ADMIN", session.getAttribute("authRole"));

    // Test session endpoint with active session
    mvc.perform(get("/api/v1/auth/session").session(session))
      .andExpect(status().isOk())
      .andExpect(jsonPath("$.authenticated").value(true))
      .andExpect(jsonPath("$.role").value("ADMIN"))
      .andExpect(jsonPath("$.canConfigure").value(true));

    // Test session endpoint without session (GUEST)
    mvc.perform(get("/api/v1/auth/session"))
      .andExpect(status().isOk())
      .andExpect(jsonPath("$.authenticated").value(false))
      .andExpect(jsonPath("$.role").value("GUEST"))
      .andExpect(jsonPath("$.canConfigure").value(false));
  }
}