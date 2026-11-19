package com.charging.cloud;

import java.time.Instant;
import java.util.Map;
import org.springframework.dao.DataAccessException;
import org.springframework.http.HttpStatus;
import org.springframework.http.ResponseEntity;
import org.springframework.web.bind.annotation.ExceptionHandler;
import org.springframework.web.bind.annotation.RestControllerAdvice;

@RestControllerAdvice
public class ApiExceptionHandler {
  @ExceptionHandler(IllegalArgumentException.class)
  public ResponseEntity<Map<String, Object>> badRequest(IllegalArgumentException error) {
    return ResponseEntity.status(HttpStatus.BAD_REQUEST).body(Map.of(
        "error", "bad_request",
        "message", error.getMessage() != null ? error.getMessage() : "Invalid argument",
        "timestamp", Instant.now().toString()));
  }

  @ExceptionHandler(java.util.NoSuchElementException.class)
  public ResponseEntity<Map<String, Object>> notFound(java.util.NoSuchElementException error) {
    return ResponseEntity.status(HttpStatus.NOT_FOUND).body(Map.of(
        "error", "not_found",
        "message", error.getMessage() != null ? error.getMessage() : "Resource not found",
        "timestamp", Instant.now().toString()));
  }

  @ExceptionHandler({IllegalStateException.class, DataAccessException.class})
  public ResponseEntity<Map<String, Object>> storageFailure(RuntimeException error) {
    return ResponseEntity.status(HttpStatus.SERVICE_UNAVAILABLE).body(Map.of(
        "error", "storage_unavailable",
        "message", error.getMessage() != null ? error.getMessage() : "Storage failure",
        "timestamp", Instant.now().toString()));
  }
}

