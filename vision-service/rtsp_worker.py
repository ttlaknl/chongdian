"""Dedicated RTSP camera stream worker running YOLO26 & plate perception and pushing real JPEG snapshots + detections to Edge Node."""

from __future__ import annotations

import base64
import logging
import os
import threading
import time
from dataclasses import dataclass
from datetime import datetime, timezone
from typing import Any

import cv2
import requests

from config import VisionConfig
from ai_engine import UnifiedAiEngine, SpacePerception

logger = logging.getLogger("vision.worker")


@dataclass(frozen=True)
class CameraTarget:
    """Normalized vision camera subscription descriptor."""

    site_id: str
    space_id: str
    camera_id: str
    protocol: str
    stream_url: str
    roi_json: str | None
    enabled: bool


class RtspCameraWorker:
    """Runs an RTSP reader thread and a YOLO26 AI inference/upload thread for one camera."""

    def __init__(
        self,
        target: CameraTarget,
        config: VisionConfig,
        ai_engine: UnifiedAiEngine,
    ) -> None:
        self.target = target
        self.config = config
        self.ai_engine = ai_engine

        self._stop_event = threading.Event()
        self._frame_lock = threading.Lock()
        self._latest_frame: Any = None
        self._latest_frame_ts: float = 0.0
        self._latest_plate_jpeg: bytes | None = None
        self._session = requests.Session()

        self._connected = False
        self._last_error: str | None = None
        self._reconnect_count = 0
        self._last_plate: str | None = None
        self._last_vehicle_type: str = "UNKNOWN"
        self._last_has_vehicle: bool | None = None
        self._last_confidence: float = 0.0
        self._last_frame_iso: str | None = None
        self._last_label: str = ""

        self._reader_thread = threading.Thread(
            target=self._reader_loop,
            name=f"rtsp-read-{target.camera_id}",
            daemon=True,
        )
        self._infer_thread = threading.Thread(
            target=self._infer_loop,
            name=f"rtsp-infer-{target.camera_id}",
            daemon=True,
        )

    def start(self) -> None:
        logger.info(
            "starting worker camera=%s space=%s stream=%s (YOLO26-Perception)",
            self.target.camera_id,
            self.target.space_id,
            self._safe_stream_label(),
        )
        self._reader_thread.start()
        self._infer_thread.start()

    def stop(self) -> None:
        self._stop_event.set()
        self._reader_thread.join(timeout=3.0)
        self._infer_thread.join(timeout=3.0)
        self._session.close()

    def status_dict(self) -> dict[str, Any]:
        return {
            "cameraId": self.target.camera_id,
            "spaceId": self.target.space_id,
            "connected": self._connected,
            "lastFrameAt": self._last_frame_iso,
            "lastPlate": self._last_plate,
            "lastVehicleType": self._last_vehicle_type,
            "lastConfidence": self._last_confidence,
            "lastLabel": self._last_label,
            "reconnectCount": self._reconnect_count,
            "lastError": self._last_error,
        }

    def _safe_stream_label(self) -> str:
        url = self.target.stream_url
        if "@" in url and "://" in url:
            scheme, rest = url.split("://", 1)
            _, host = rest.split("@", 1)
            return f"{scheme}://***@{host}"
        return url

    def _headers(self, extra: dict[str, str] | None = None) -> dict[str, str]:
        headers: dict[str, str] = {}
        if self.config.edge_local_api_key:
            headers["X-Edge-Local-Key"] = self.config.edge_local_api_key
        if extra:
            headers.update(extra)
        return headers

    def _open_capture(self) -> cv2.VideoCapture:
        os.environ["OPENCV_FFMPEG_CAPTURE_OPTIONS"] = (
            f"rtsp_transport;{self.config.rtsp_transport}|stimeout;5000000"
        )
        cap = cv2.VideoCapture(self.target.stream_url, cv2.CAP_FFMPEG)
        cap.set(cv2.CAP_PROP_BUFFERSIZE, 1)
        return cap

    def _reader_loop(self) -> None:
        backoff = 2.0
        while not self._stop_event.is_set():
            cap = self._open_capture()
            if not cap.isOpened():
                cap.release()
                self._connected = False
                self._reconnect_count += 1
                self._last_error = f"failed to open RTSP stream ({self._safe_stream_label()})"
                logger.warning("camera=%s %s", self.target.camera_id, self._last_error)
                self._stop_event.wait(backoff)
                backoff = min(30.0, backoff * 2.0)
                continue

            backoff = 2.0
            self._connected = True
            self._last_error = None
            logger.info("camera=%s RTSP stream opened", self.target.camera_id)

            try:
                while not self._stop_event.is_set():
                    ok, frame = cap.read()
                    if not ok or frame is None:
                        self._connected = False
                        with self._frame_lock:
                            self._latest_frame = None
                            self._latest_frame_ts = 0.0
                        self._reconnect_count += 1
                        self._last_error = "RTSP frame read failed / stream EOF"
                        logger.warning("camera=%s %s", self.target.camera_id, self._last_error)
                        break

                    now_ts = time.monotonic()
                    with self._frame_lock:
                        self._latest_frame = frame
                        self._latest_frame_ts = now_ts
                    self._last_frame_iso = datetime.now(timezone.utc).isoformat()
            finally:
                cap.release()

            if not self._stop_event.is_set():
                self._stop_event.wait(backoff)
                backoff = min(30.0, backoff * 2.0)

    def _infer_loop(self) -> None:
        interval = 1.0 / self.config.infer_fps
        last_snapshot_push = 0.0
        last_heartbeat_push = 0.0
        last_detection_push = 0.0
        last_frame_seen_ts = 0.0

        while not self._stop_event.is_set():
            loop_started = time.monotonic()

            with self._frame_lock:
                frame = None if self._latest_frame is None else self._latest_frame.copy()
                frame_ts = self._latest_frame_ts

            if frame is None or frame_ts <= 0 or (loop_started - frame_ts) > 10.0:
                self._connected = False
                self._stop_event.wait(0.5)
                continue

            perception: SpacePerception | None = None
            try:
                perception = self.ai_engine.evaluate_space(
                    frame,
                    self.target.space_id,
                    self.target.roi_json,
                )
                if perception.has_vehicle:
                    # 如果有目标且存在识别框/证据帧，保存作为高质取证图
                    ev_frame = perception.evidence_frame if perception.evidence_frame is not None else frame
                    ok, encoded = cv2.imencode(".jpg", ev_frame, [cv2.IMWRITE_JPEG_QUALITY, 85])
                    if ok:
                        self._latest_plate_jpeg = encoded.tobytes()
                else:
                    self._latest_plate_jpeg = None
            except Exception as exc:  # noqa: BLE001
                self._last_error = f"YOLO26 inference error: {exc}"
                logger.exception("camera=%s inference failure", self.target.camera_id)

            if perception is None:
                self._stop_event.wait(0.5)
                continue

            state_changed = (
                self._last_has_vehicle != perception.has_vehicle
                or self._last_plate != perception.plate
                or self._last_vehicle_type != perception.vehicle_type
            )

            if state_changed:
                if perception.has_vehicle:
                    logger.info(
                        "camera=%s space=%s >>> DETECTED OCCUPANCY: plate=%s, color=%s, type=%s, viol=%s, label=%s, conf=%.2f <<<",
                        self.target.camera_id,
                        self.target.space_id,
                        perception.plate,
                        perception.plate_color,
                        perception.vehicle_type,
                        perception.violation_type,
                        perception.category_label,
                        perception.confidence,
                    )
                else:
                    logger.info(
                        "camera=%s space=%s >>> SPACE CLEARED (empty) <<<",
                        self.target.camera_id,
                        self.target.space_id,
                    )

            self._last_has_vehicle = perception.has_vehicle
            self._last_plate = perception.plate
            self._last_vehicle_type = perception.vehicle_type
            self._last_confidence = perception.confidence
            self._last_label = perception.category_label

            now = time.monotonic()

            # Push real JPEG frame to Edge when snapshot cadence arrives or vehicle/plate state changes.
            if (now - last_snapshot_push) >= self.config.snapshot_seconds or state_changed:
                if perception.has_vehicle and self._latest_plate_jpeg is not None:
                    self._push_snapshot_bytes(self._latest_plate_jpeg, force_cloud=state_changed)
                else:
                    self._push_snapshot(frame, force_cloud=state_changed)
                last_snapshot_push = now

            # Push detection on state transition or periodic refresh every 3 seconds.
            if frame_ts > last_frame_seen_ts and (
                state_changed or (now - last_detection_push) >= 3.0
            ):
                self._push_detection(perception, frame)
                last_detection_push = now
                last_frame_seen_ts = frame_ts

            if (now - last_heartbeat_push) >= self.config.heartbeat_seconds:
                self._push_heartbeat()
                last_heartbeat_push = now

            spent = time.monotonic() - loop_started
            remaining = interval - spent
            if remaining > 0:
                self._stop_event.wait(remaining)

    def _push_snapshot_bytes(self, jpeg_bytes: bytes, force_cloud: bool = False) -> None:
        if not self._connected:
            return
        url = f"{self.config.edge_url}/api/v1/spaces/{self.target.space_id}/snapshot"
        extra = {"Content-Type": "image/jpeg"}
        if force_cloud:
            extra["X-Force-Cloud-Sync"] = "true"
        try:
            resp = self._session.post(
                url,
                data=jpeg_bytes,
                headers=self._headers(extra),
                timeout=3.0,
            )
            if resp.status_code >= 400:
                logger.warning("snapshot upload HTTP %s for %s", resp.status_code, self.target.space_id)
        except requests.RequestException as exc:
            logger.debug("snapshot upload error for %s: %s", self.target.space_id, exc)

    def _push_snapshot(self, frame: Any, force_cloud: bool = False) -> None:
        ok, encoded = cv2.imencode(".jpg", frame, [cv2.IMWRITE_JPEG_QUALITY, 82])
        if not ok:
            return
        self._push_snapshot_bytes(encoded.tobytes(), force_cloud=force_cloud)

    def _push_detection(self, perception: SpacePerception, current_frame: Any = None) -> None:
        url = f"{self.config.edge_url}/api/v1/spaces/{self.target.space_id}/detections"
        payload = {
            "spaceId": self.target.space_id,
            "hasVehicle": perception.has_vehicle,
            "plate": perception.plate,
            "vehicleType": perception.vehicle_type,
            "capturedAt": datetime.now(timezone.utc).isoformat(),
            "isSimulated": False,
            "dataSource": "RTSP_YOLO26_AI",
            "confidence": perception.confidence,
            "plateColor": perception.plate_color,
            "cameraId": self.target.camera_id,
        }

        frame_bytes = None
        if perception.has_vehicle and self._latest_plate_jpeg is not None:
            frame_bytes = self._latest_plate_jpeg
        elif current_frame is not None:
            ok, enc = cv2.imencode(".jpg", current_frame, [cv2.IMWRITE_JPEG_QUALITY, 80])
            if ok:
                frame_bytes = enc.tobytes()

        if frame_bytes is not None:
            payload["imageBase64"] = base64.b64encode(frame_bytes).decode("ascii")

        try:
            resp = self._session.post(
                url,
                json=payload,
                headers=self._headers(),
                timeout=3.0,
            )
            if resp.status_code >= 400:
                logger.warning(
                    "detection push HTTP %s for %s: %s",
                    resp.status_code,
                    self.target.space_id,
                    resp.text[:200],
                )
        except requests.RequestException as exc:
            logger.warning("detection push error for %s: %s", self.target.space_id, exc)

    def _push_heartbeat(self) -> None:
        if not self._connected:
            return
        url = f"{self.config.edge_url}/api/v1/cameras/{self.target.camera_id}/heartbeat"
        try:
            self._session.post(url, json={}, headers=self._headers(), timeout=3.0)
        except requests.RequestException:
            pass
