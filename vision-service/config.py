"""Configuration loader for the RTSP vision sidecar."""

from __future__ import annotations

import json
import os
from dataclasses import dataclass, field
from typing import Any


def _env_float(name: str, default: float, min_val: float = 0.01) -> float:
    raw = os.getenv(name, "").strip()
    if not raw:
        return default
    try:
        return max(min_val, float(raw))
    except ValueError:
        return default


def _env_int(name: str, default: int, min_val: int = 1) -> int:
    raw = os.getenv(name, "").strip()
    if not raw:
        return default
    try:
        return max(min_val, int(raw))
    except ValueError:
        return default


@dataclass(frozen=True)
class VisionConfig:
    """Runtime settings sourced purely from environment variables."""

    edge_url: str = "http://127.0.0.1:8081"
    edge_local_api_key: str | None = None
    target_poll_seconds: float = 10.0
    infer_fps: float = 1.0
    snapshot_seconds: float = 5.0
    heartbeat_seconds: float = 15.0
    min_confidence: float = 0.72
    vote_window: int = 5
    vote_min_hits: int = 2
    empty_clear_frames: int = 4
    rtsp_transport: str = "tcp"
    health_port: int = 8090
    static_cameras: list[dict[str, Any]] = field(default_factory=list)

    @classmethod
    def from_env(cls) -> "VisionConfig":
        raw_cams = os.getenv("VISION_CAMERAS_JSON", "").strip()
        static_cameras: list[dict[str, Any]] = []
        if raw_cams:
            parsed = json.loads(raw_cams)
            if isinstance(parsed, list):
                static_cameras = [item for item in parsed if isinstance(item, dict)]

        local_key = os.getenv("EDGE_LOCAL_API_KEY", "").strip() or None
        transport = os.getenv("VISION_RTSP_TRANSPORT", "tcp").strip().lower()
        if transport not in {"tcp", "udp"}:
            transport = "tcp"

        return cls(
            edge_url=os.getenv("EDGE_URL", "http://127.0.0.1:8081").strip().rstrip("/"),
            edge_local_api_key=local_key,
            target_poll_seconds=_env_float("VISION_TARGET_POLL_SECONDS", 10.0, 1.0),
            infer_fps=_env_float("VISION_INFER_FPS", 1.0, 0.1),
            snapshot_seconds=_env_float("VISION_SNAPSHOT_SECONDS", 5.0, 1.0),
            heartbeat_seconds=_env_float("VISION_HEARTBEAT_SECONDS", 15.0, 3.0),
            min_confidence=_env_float("VISION_MIN_CONFIDENCE", 0.72, 0.1),
            vote_window=_env_int("VISION_VOTE_WINDOW", 5, 1),
            vote_min_hits=_env_int("VISION_VOTE_MIN_HITS", 2, 1),
            empty_clear_frames=_env_int("VISION_EMPTY_CLEAR_FRAMES", 4, 1),
            rtsp_transport=transport,
            health_port=_env_int("VISION_HEALTH_PORT", 8090, 1),
            static_cameras=static_cameras,
        )
