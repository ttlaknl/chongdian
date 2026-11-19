"""RTSP vision sidecar entrypoint: polls Edge camera targets, manages workers, and serves /health."""

from __future__ import annotations

import json
import logging
import signal
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any

import requests

from config import VisionConfig
from plate_recognizer import HyperLpr3Recognizer
from rtsp_worker import CameraTarget, RtspCameraWorker

logging.basicConfig(
    level=logging.INFO,
    format="%(asctime)s [%(levelname)s] %(name)s: %(message)s",
)
logger = logging.getLogger("vision.main")


class VisionManager:
    """Synchronizes RTSP workers with Edge Node target configuration and optional env overrides."""

    def __init__(self, config: VisionConfig) -> None:
        self.config = config
        self.recognizer = HyperLpr3Recognizer(min_confidence=config.min_confidence)
        self._lock = threading.Lock()
        self._workers: dict[str, RtspCameraWorker] = {}
        self._stop_event = threading.Event()
        self._last_poll_error: str | None = None
        self._last_poll_ok_at: float | None = None

    def stop(self) -> None:
        self._stop_event.set()
        with self._lock:
            for worker in self._workers.values():
                worker.stop()
            self._workers.clear()

    def health_payload(self) -> dict[str, Any]:
        with self._lock:
            workers = [worker.status_dict() for worker in self._workers.values()]
        return {
            "status": "UP",
            "service": "vision-service",
            "edgeUrl": self.config.edge_url,
            "activeWorkers": len(workers),
            "lastPollOkAt": self._last_poll_ok_at,
            "lastPollError": self._last_poll_error,
            "workers": workers,
        }

    def _merge_targets(self, edge_targets: list[dict[str, Any]]) -> dict[str, CameraTarget]:
        merged: dict[str, CameraTarget] = {}

        def ingest(raw: dict[str, Any]) -> None:
            cam_id = str(raw.get("cameraId") or "").strip()
            space_id = str(raw.get("spaceId") or "").strip()
            stream_url = str(raw.get("streamUrl") or "").strip()
            enabled = bool(raw.get("enabled", True))
            protocol = str(raw.get("protocol") or "RTSP").strip().upper()
            roi_val = raw.get("roiJson")
            roi_json = (
                json.dumps(roi_val, ensure_ascii=False)
                if isinstance(roi_val, dict)
                else (str(roi_val).strip() if roi_val else None)
            )
            if not cam_id or not space_id or not stream_url or not enabled:
                return
            if not stream_url.lower().startswith(("rtsp://", "rtsps://", "http://", "https://")):
                return
            merged[cam_id] = CameraTarget(
                site_id=str(raw.get("siteId") or "default").strip(),
                space_id=space_id,
                camera_id=cam_id,
                protocol=protocol,
                stream_url=stream_url,
                roi_json=roi_json,
                enabled=enabled,
            )

        for item in edge_targets:
            if isinstance(item, dict):
                ingest(item)
        for item in self.config.static_cameras:
            ingest(item)

        return merged

    def _reconcile_workers(self, desired: dict[str, CameraTarget]) -> None:
        with self._lock:
            existing_ids = set(self._workers.keys())
            desired_ids = set(desired.keys())

            for removed_id in existing_ids - desired_ids:
                logger.info("stopping removed camera worker %s", removed_id)
                self._workers.pop(removed_id).stop()

            for cam_id, target in desired.items():
                current = self._workers.get(cam_id)
                if current is not None and current.target == target:
                    continue
                if current is not None:
                    logger.info("restarting updated camera worker %s", cam_id)
                    self._workers.pop(cam_id).stop()
                worker = RtspCameraWorker(target, self.config, self.recognizer)
                worker.start()
                self._workers[cam_id] = worker

    def poll_loop(self) -> None:
        session = requests.Session()
        headers: dict[str, str] = {}
        if self.config.edge_local_api_key:
            headers["X-Edge-Local-Key"] = self.config.edge_local_api_key

        while not self._stop_event.is_set():
            edge_targets: list[dict[str, Any]] = []
            try:
                resp = session.get(
                    f"{self.config.edge_url}/api/v1/vision/targets",
                    headers=headers,
                    timeout=4.0,
                )
                resp.raise_for_status()
                payload = resp.json()
                if isinstance(payload, list):
                    edge_targets = payload
                self._last_poll_error = None
                self._last_poll_ok_at = time.time()
            except requests.RequestException as exc:
                self._last_poll_error = str(exc)
                logger.warning("edge vision targets poll failed: %s", exc)
            except ValueError as exc:
                self._last_poll_error = f"invalid JSON from edge targets: {exc}"

            desired = self._merge_targets(edge_targets)
            self._reconcile_workers(desired)
            self._stop_event.wait(self.config.target_poll_seconds)

        session.close()


def start_health_server(manager: VisionManager, port: int) -> ThreadingHTTPServer:
    """Expose lightweight HTTP health telemetry for Docker healthchecks and ops."""

    class HealthHandler(BaseHTTPRequestHandler):
        def do_GET(self) -> None:  # noqa: N802
            if self.path not in {"/health", "/"}:
                self.send_response(404)
                self.end_headers()
                return
            body = json.dumps(manager.health_payload(), ensure_ascii=False).encode("utf-8")
            self.send_response(200)
            self.send_header("Content-Type", "application/json; charset=utf-8")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, format: str, *args: Any) -> None:  # noqa: A002
            return

    server = ThreadingHTTPServer(("0.0.0.0", port), HealthHandler)
    thread = threading.Thread(target=server.serve_forever, name="vision-health", daemon=True)
    thread.start()
    return server


def main() -> None:
    config = VisionConfig.from_env()
    manager = VisionManager(config)
    server = start_health_server(manager, config.health_port)
    logger.info(
        "vision-service started edge_url=%s health_port=%s transport=%s",
        config.edge_url,
        config.health_port,
        config.rtsp_transport,
    )

    def handle_signal(_signum: int, _frame: Any) -> None:
        logger.info("shutdown signal received")
        manager.stop()

    signal.signal(signal.SIGINT, handle_signal)
    signal.signal(signal.SIGTERM, handle_signal)

    try:
        manager.poll_loop()
    finally:
        manager.stop()
        server.shutdown()


if __name__ == "__main__":
    main()
