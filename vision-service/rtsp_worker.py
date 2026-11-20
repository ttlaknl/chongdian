"""RTSP camera worker running OpenVINO AI perception with Dual-Stream (Main/Sub) Variable-Frequency Scheduling.

Milestone 1B Features:
- Dual-stream variable-frequency scheduler:
    * IDLE mode: inspects sub-stream at SUB_STREAM_FPS (default 1.0) for low CPU usage
    * ACTIVE_BURST mode: triggered by unconfirmed vehicles or pre_entry_polygon, pulls high-res main-stream at MAIN_BURST_FPS (default 4.0)
    * Graceful single-stream fallback when sub_stream_url is not configured
- Zero-PyTorch runtime: pure OpenVINO inference
- Exponential backoff reconnection for resilient RTSP streaming
"""

from __future__ import annotations

import base64
import json
import logging
import os
import threading
import time
from dataclasses import dataclass
from datetime import datetime, timezone
from typing import Any

import cv2
import numpy as np
import requests

from config import VisionConfig
from ai_engine import UnifiedAiEngine, SpacePerception
from geometry import (
    assign_vehicle_to_space,
    compute_ground_band,
    get_bottom_anchors,
    is_in_osd_mask,
    parse_spaces_from_roi_json,
    to_pixel_coords,
)
from plate_postprocess import is_valid_chinese_plate
from space_tracker import (
    ByteTracker,
    FsmEvent,
    ParkingSpaceFSM,
    STrack,
    SpaceState,
    TrackPlateVoter,
)

logger = logging.getLogger("vision.worker")


@dataclass(frozen=True)
class CameraTarget:
    """Normalized vision camera subscription descriptor."""

    site_id: str
    space_id: str
    camera_id: str
    protocol: str
    stream_url: str                                # Main stream (1080p/4K)
    sub_stream_url: str | None = None              # Sub stream (360p/720p, optional)
    roi_json: str | None = None                    # Space polygon(s)
    pre_entry_polygon: str | list | None = None    # Pre-entry warning polygon (optional)
    enabled: bool = True


class PlateVoteDebouncer:
    """Per-space sliding-window multi-frame vote debounce (Milestone 1A/1B)."""

    def __init__(self, window_size: int = 3, min_hits: int = 2, clear_frames: int = 4) -> None:
        from collections import deque, Counter
        self._window_size = max(1, window_size)
        self._min_hits = max(1, min(min_hits, self._window_size))
        self._clear_frames = max(1, clear_frames)
        self._history: deque[str | None] = deque(maxlen=self._window_size)
        self._empty_streak: int = 0
        self._confirmed_plate: str | None = None
        self._Counter = Counter

    def reset(self) -> None:
        self._history.clear()
        self._empty_streak = 0
        self._confirmed_plate = None

    @property
    def confirmed_plate(self) -> str | None:
        return self._confirmed_plate

    def update(self, plate: str | None) -> str | None:
        if plate and is_valid_chinese_plate(plate):
            self._empty_streak = 0
            self._history.append(plate.upper())

            counts = self._Counter(p for p in self._history if p is not None)
            if counts:
                best_plate, best_hits = counts.most_common(1)[0]
                if best_hits >= self._min_hits:
                    self._confirmed_plate = best_plate
                    return best_plate

            return self._confirmed_plate
        else:
            self._history.append(None)
            self._empty_streak += 1
            if self._empty_streak >= self._clear_frames:
                self._history.clear()
                self._confirmed_plate = None
            return self._confirmed_plate


PlateDebounceVoter = PlateVoteDebouncer


class RtspCameraWorker:
    """Manages main & sub stream RTSP capture and variable-frequency AI perception."""

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
        self._session = requests.Session()

        # State: "IDLE" or "ACTIVE_BURST"
        self._mode = "IDLE"
        self._burst_start_ts = 0.0

        # Frame buffers & locks
        self._main_lock = threading.Lock()
        self._main_frame: np.ndarray | None = None
        self._main_frame_ts: float = 0.0

        self._sub_lock = threading.Lock()
        self._sub_frame: np.ndarray | None = None
        self._sub_frame_ts: float = 0.0

        # Connection states
        self._main_connected = False
        self._sub_connected = False
        self._last_error: str | None = None
        self._reconnect_count = 0

        # Detection cache
        self._last_plate: str | None = None
        self._last_vehicle_type: str = "UNKNOWN"
        self._last_has_vehicle: bool | None = None
        self._last_confidence: float = 0.0
        self._last_frame_iso: str | None = None
        self._last_label: str = ""
        self._latest_plate_jpeg: bytes | None = None

        # Multi-frame voting debouncer (legacy compatible)
        self._plate_voter = PlateVoteDebouncer(
            window_size=config.plate_vote_window,
            min_hits=config.plate_vote_min_hits,
            clear_frames=config.vote_empty_clear_frames,
        )

        # Milestone 2B: Pure NumPy ByteTrack, TrackPlateVoter, ParkingSpaceFSM
        self._tracker = ByteTracker(
            track_high_thresh=config.track_high_thresh,
            track_low_thresh=config.track_low_thresh,
            track_max_lost_sec=config.track_max_lost_sec,
        )
        self._track_voter = TrackPlateVoter(
            min_hits=config.plate_vote_min_hits,
            lock_score_thresh=config.plate_lock_score_threshold,
        )
        self._space_fsms: dict[str, ParkingSpaceFSM] = {
            self.target.space_id: ParkingSpaceFSM(
                space_id=self.target.space_id,
                enter_confirm_sec=config.enter_confirm_sec,
                occupied_confirm_sec=config.occupied_confirm_sec,
                leave_confirm_sec=config.leave_confirm_sec,
            )
        }
        self._spaces_polygons: dict[str, np.ndarray] | None = None

        # Pre-entry polygon parsed
        self._pre_entry_poly: np.ndarray | None = None

        # Threads
        self._has_sub_stream = bool(self.target.sub_stream_url and self.target.sub_stream_url.strip())
        self._main_reader_thread = threading.Thread(
            target=self._main_reader_loop,
            name=f"rtsp-main-{target.camera_id}",
            daemon=True,
        )
        self._sub_reader_thread = (
            threading.Thread(
                target=self._sub_reader_loop,
                name=f"rtsp-sub-{target.camera_id}",
                daemon=True,
            )
            if self._has_sub_stream
            else None
        )
        self._infer_thread = threading.Thread(
            target=self._infer_loop,
            name=f"rtsp-infer-{target.camera_id}",
            daemon=True,
        )

    def start(self) -> None:
        logger.info(
            "starting camera=%s space=%s (main=%s, sub=%s, mode=%s)",
            self.target.camera_id,
            self.target.space_id,
            self._safe_url(self.target.stream_url),
            self._safe_url(self.target.sub_stream_url) if self._has_sub_stream else "None",
            self.config.main_stream_mode,
        )
        self._main_reader_thread.start()
        if self._sub_reader_thread is not None:
            self._sub_reader_thread.start()
        self._infer_thread.start()

    def stop(self) -> None:
        self._stop_event.set()
        self._main_reader_thread.join(timeout=3.0)
        if self._sub_reader_thread is not None:
            self._sub_reader_thread.join(timeout=3.0)
        self._infer_thread.join(timeout=3.0)
        self._session.close()

    def status_dict(self) -> dict[str, Any]:
        primary_fsm = self._space_fsms.get(self.target.space_id)
        fsm_state_str = primary_fsm.state.value if primary_fsm else "FREE"
        return {
            "cameraId": self.target.camera_id,
            "spaceId": self.target.space_id,
            "mode": self._mode,
            "fsmState": fsm_state_str,
            "connected": self._main_connected or self._sub_connected,
            "mainConnected": self._main_connected,
            "subConnected": self._sub_connected,
            "hasSubStream": self._has_sub_stream,
            "lastFrameAt": self._last_frame_iso,
            "lastPlate": primary_fsm.confirmed_plate if primary_fsm and primary_fsm.confirmed_plate else self._last_plate,
            "lastVehicleType": primary_fsm.confirmed_vehicle_type if primary_fsm and primary_fsm.confirmed_vehicle_type != "UNKNOWN" else self._last_vehicle_type,
            "lastConfidence": self._last_confidence,
            "lastLabel": self._last_label,
            "reconnectCount": self._reconnect_count,
            "lastError": self._last_error,
        }

    def _safe_url(self, url: str | None) -> str:
        if not url:
            return ""
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

    def _open_capture(self, url: str) -> cv2.VideoCapture:
        os.environ["OPENCV_FFMPEG_CAPTURE_OPTIONS"] = (
            f"rtsp_transport;{self.config.rtsp_transport}|stimeout;5000000"
        )
        cap = cv2.VideoCapture(url, cv2.CAP_FFMPEG)
        cap.set(cv2.CAP_PROP_BUFFERSIZE, 1)
        return cap

    def _main_reader_loop(self) -> None:
        """Main stream reader thread with grab_idle low-overhead mode."""
        backoff = 2.0
        while not self._stop_event.is_set():
            if self.config.main_stream_mode == "on_demand" and self._mode == "IDLE" and self._has_sub_stream:
                self._stop_event.wait(0.5)
                continue

            cap = self._open_capture(self.target.stream_url)
            if not cap.isOpened():
                cap.release()
                self._main_connected = False
                self._reconnect_count += 1
                self._last_error = f"failed to open main RTSP ({self._safe_url(self.target.stream_url)})"
                self._stop_event.wait(backoff)
                backoff = min(30.0, backoff * 2.0)
                continue

            backoff = 2.0
            self._main_connected = True
            logger.info("camera=%s main RTSP stream opened", self.target.camera_id)

            try:
                while not self._stop_event.is_set():
                    # If in IDLE and sub stream is active: just grab to drop buffer with minimal CPU
                    if self._has_sub_stream and self._mode == "IDLE":
                        grabbed = cap.grab()
                        if not grabbed:
                            break
                        self._stop_event.wait(0.04)  # ~25 fps grab cadence
                        continue

                    ok, frame = cap.read()
                    if not ok or frame is None:
                        break

                    now_ts = time.monotonic()
                    with self._main_lock:
                        self._main_frame = frame
                        self._main_frame_ts = now_ts
                    self._last_frame_iso = datetime.now(timezone.utc).isoformat()
            finally:
                cap.release()
                self._main_connected = False

            if not self._stop_event.is_set():
                self._stop_event.wait(backoff)
                backoff = min(30.0, backoff * 2.0)

    def _sub_reader_loop(self) -> None:
        """Sub stream reader thread running continuously at low resolution."""
        if not self.target.sub_stream_url:
            return

        backoff = 2.0
        while not self._stop_event.is_set():
            cap = self._open_capture(self.target.sub_stream_url)
            if not cap.isOpened():
                cap.release()
                self._sub_connected = False
                self._stop_event.wait(backoff)
                backoff = min(30.0, backoff * 2.0)
                continue

            backoff = 2.0
            self._sub_connected = True
            logger.info("camera=%s sub RTSP stream opened", self.target.camera_id)

            try:
                while not self._stop_event.is_set():
                    ok, frame = cap.read()
                    if not ok or frame is None:
                        break

                    now_ts = time.monotonic()
                    with self._sub_lock:
                        self._sub_frame = frame
                        self._sub_frame_ts = now_ts
            finally:
                cap.release()
                self._sub_connected = False

            if not self._stop_event.is_set():
                self._stop_event.wait(backoff)
                backoff = min(30.0, backoff * 2.0)

    def _parse_pre_entry(self, width: int, height: int) -> np.ndarray | None:
        """Helper to parse pre_entry_polygon into pixel coordinates."""
        raw = self.target.pre_entry_polygon
        if not raw:
            return None
        try:
            poly = json.loads(raw) if isinstance(raw, str) else raw
            if isinstance(poly, dict):
                poly = poly.get("polygon") or poly.get("points")
            if isinstance(poly, list) and len(poly) >= 3:
                return to_pixel_coords(poly, width, height)
        except Exception:
            pass
        return None

    def _draw_debug_overlay(
        self,
        frame: np.ndarray,
        spaces_polygons: dict[str, np.ndarray],
        active_tracks: list[STrack],
        space_to_track: dict[str, STrack],
    ) -> np.ndarray:
        """Render diagnostic overlays for camera calibration when DEBUG_DRAW_BOXES=True."""
        vis = frame.copy()

        # 1. Pre-entry polygon
        if self._pre_entry_poly is not None and len(self._pre_entry_poly) >= 3:
            cv2.polylines(vis, [self._pre_entry_poly], isClosed=True, color=(255, 160, 0), thickness=2)
            p0 = self._pre_entry_poly[0]
            cv2.putText(
                vis,
                "PRE-ENTRY",
                (int(p0[0]), max(15, int(p0[1]) - 6)),
                cv2.FONT_HERSHEY_SIMPLEX,
                0.55,
                (255, 160, 0),
                2,
            )

        # 2. Parking space polygons & FSM states
        state_colors = {
            SpaceState.FREE: (0, 220, 0),
            SpaceState.ENTERING: (0, 220, 255),
            SpaceState.OCCUPIED: (0, 0, 255),
            SpaceState.LEAVING: (255, 0, 255),
        }
        for sp_id, poly in spaces_polygons.items():
            if poly is None or len(poly) < 3:
                continue
            fsm = self._space_fsms.get(sp_id)
            f_state = fsm.state if fsm else SpaceState.FREE
            color = state_colors.get(f_state, (200, 200, 200))
            cv2.polylines(vis, [poly], isClosed=True, color=color, thickness=2)

            label_parts = [sp_id, f"[{f_state.value}]"]
            if fsm and fsm.confirmed_plate:
                label_parts.append(fsm.confirmed_plate)
            txt = " ".join(label_parts)
            pt = poly[0]
            cv2.putText(
                vis,
                txt,
                (int(pt[0]), max(20, int(pt[1]) - 8)),
                cv2.FONT_HERSHEY_SIMPLEX,
                0.6,
                color,
                2,
            )

        # 3. Active vehicle tracks, bounding boxes, ground bands, and voter status
        for tr in active_tracks:
            x1, y1, x2, y2 = [int(v) for v in tr.tlbr]
            # Vehicle bounding box
            cv2.rectangle(vis, (x1, y1), (x2, y2), (255, 220, 50), 2)

            # Ground band rectangle
            gx1, gy1, gx2, gy2 = compute_ground_band(tr.tlbr, self.config.ground_band_ratio)
            cv2.rectangle(vis, (gx1, gy1), (gx2, gy2), (0, 165, 255), 2)

            # 3 bottom anchors
            for ax, ay in get_bottom_anchors(tr.tlbr):
                cv2.circle(vis, (int(ax), int(ay)), 4, (0, 0, 255), -1)

            # Trajectory voter info
            l_plate, l_col, _, _, is_locked = self._track_voter.get_locked_plate(tr.track_id)
            if is_locked:
                v_info = f"LOCK:{l_plate}({l_col})"
                txt_col = (0, 255, 0)
            elif l_plate:
                rec = self._track_voter._tracks.get(tr.track_id, {})
                sc = rec.get("scores", {}).get(l_plate, 0.0)
                ht = rec.get("hits", {}).get(l_plate, 0)
                v_info = f"VOTE:{l_plate}({ht}h,{sc:.1f}s)"
                txt_col = (0, 255, 255)
            else:
                v_info = "NO_PLATE"
                txt_col = (200, 200, 200)

            tr_label = f"ID:{tr.track_id} {tr.category_type} {v_info}"
            cv2.putText(
                vis,
                tr_label,
                (x1, max(15, y1 - 6)),
                cv2.FONT_HERSHEY_SIMPLEX,
                0.5,
                txt_col,
                2,
            )

        return vis

    def _dispatch_fsm_event(
        self,
        event: FsmEvent,
        payload: dict[str, Any] | None,
        current_frame: np.ndarray | None,
    ) -> None:
        """Dispatch transition events from ParkingSpaceFSM to Edge Node."""
        if payload is None:
            return

        space_id = payload.get("spaceId", self.target.space_id)
        has_vehicle = payload.get("hasVehicle", False)
        plate = payload.get("plate")
        v_type = payload.get("vehicleType", "UNKNOWN")
        p_color = payload.get("plateColor", "UNKNOWN")
        best_ev = payload.get("bestEvidence")

        ev_bytes: bytes | None = None
        ev_frame = best_ev if best_ev is not None else current_frame
        if has_vehicle and ev_frame is not None:
            ok, encoded = cv2.imencode(".jpg", ev_frame, [cv2.IMWRITE_JPEG_QUALITY, 85])
            if ok:
                ev_bytes = encoded.tobytes()
                self._latest_plate_jpeg = ev_bytes
        elif not has_vehicle:
            self._latest_plate_jpeg = None

        if space_id == self.target.space_id:
            self._last_has_vehicle = has_vehicle
            self._last_plate = plate
            self._last_vehicle_type = v_type
            self._last_confidence = 0.90 if has_vehicle else 0.0
            if has_vehicle:
                self._last_label = f"[{event.value}] {plate or '车辆在位'}"
            else:
                self._last_label = f"[{event.value}] 车位空闲"

        logger.info(
            "camera=%s space=%s >>> DISPATCH FSM EVENT: %s (plate=%s, color=%s, type=%s) <<<",
            self.target.camera_id,
            space_id,
            event.value,
            plate,
            p_color,
            v_type,
        )
        self._push_detection_payload(
            space_id=space_id,
            has_vehicle=has_vehicle,
            plate=plate,
            vehicle_type=v_type,
            plate_color=p_color,
            confidence=0.90 if has_vehicle else 0.0,
            frame_bytes=ev_bytes,
            force_cloud=True,
        )

    def _infer_loop(self) -> None:
        """Dual-stream variable-frequency inference loop with ByteTrack, TrackPlateVoter, and FSM."""
        last_snapshot_push = 0.0
        last_heartbeat_push = 0.0
        last_detection_push = 0.0

        while not self._stop_event.is_set():
            loop_started = time.monotonic()
            is_burst = (self._mode == "ACTIVE_BURST")
            target_fps = self.config.main_burst_fps if is_burst else self.config.sub_stream_fps
            interval = 1.0 / max(0.1, target_fps)

            # 1. Fetch appropriate frame
            frame: np.ndarray | None = None
            frame_ts = 0.0

            if not is_burst and self._has_sub_stream:
                with self._sub_lock:
                    if self._sub_frame is not None:
                        frame = self._sub_frame.copy()
                        frame_ts = self._sub_frame_ts
            else:
                with self._main_lock:
                    if self._main_frame is not None:
                        frame = self._main_frame.copy()
                        frame_ts = self._main_frame_ts
                # Single-stream fallback if main frame not ready
                if frame is None and self._has_sub_stream:
                    with self._sub_lock:
                        if self._sub_frame is not None:
                            frame = self._sub_frame.copy()
                            frame_ts = self._sub_frame_ts

            if frame is None or frame_ts <= 0 or (loop_started - frame_ts) > 10.0:
                self._stop_event.wait(0.2)
                continue

            h, w = frame.shape[:2]
            if self._pre_entry_poly is None and self.target.pre_entry_polygon:
                self._pre_entry_poly = self._parse_pre_entry(w, h)

            spaces_polygons = parse_spaces_from_roi_json(self.target.roi_json, self.target.space_id, w, h)
            self._spaces_polygons = spaces_polygons

            for sp_id in spaces_polygons:
                if sp_id not in self._space_fsms:
                    self._space_fsms[sp_id] = ParkingSpaceFSM(
                        space_id=sp_id,
                        enter_confirm_sec=self.config.enter_confirm_sec,
                        occupied_confirm_sec=self.config.occupied_confirm_sec,
                        leave_confirm_sec=self.config.leave_confirm_sec,
                    )

            now_ts = time.monotonic()

            # 2. Object detection & ByteTrack tracking
            try:
                detected_objs = self.ai_engine.detect_objects(frame, conf=self.config.track_low_thresh)
                veh_objs = [o for o in detected_objs if o["category_type"] in {"CAR", "HEAVY_VEHICLE"}]
            except Exception as exc:
                logger.warning("object detection failure: %s", exc)
                veh_objs = []

            active_tracks = self._tracker.update(veh_objs, timestamp=now_ts)

            # 3. Spatial Containment & Space Assignment
            tracks_in_pre_entry: set[int] = set()
            space_to_track: dict[str, STrack] = {}

            for tr in active_tracks:
                if self._pre_entry_poly is not None and len(self._pre_entry_poly) >= 3:
                    bx1, by1, bx2, by2 = tr.tlbr
                    bc = ((bx1 + bx2) / 2.0, float(by2))
                    if cv2.pointPolygonTest(self._pre_entry_poly, bc, False) >= 0:
                        tracks_in_pre_entry.add(tr.track_id)

                sp_id = assign_vehicle_to_space(
                    tr.tlbr,
                    spaces_polygons,
                    ground_band_ratio=self.config.ground_band_ratio,
                    min_ioa=self.config.ground_min_ioa,
                )
                if sp_id is not None and sp_id in self._space_fsms:
                    if sp_id not in space_to_track or tr.score > space_to_track[sp_id].score:
                        space_to_track[sp_id] = tr

            # 4. Mode Switching & Trigger Checks
            should_burst = False
            for tr in active_tracks:
                is_locked = self._track_voter.is_locked(tr.track_id)
                if not is_locked:
                    sp_assigned = assign_vehicle_to_space(
                        tr.tlbr,
                        spaces_polygons,
                        ground_band_ratio=self.config.ground_band_ratio,
                        min_ioa=self.config.ground_min_ioa,
                    )
                    if tr.track_id in tracks_in_pre_entry or sp_assigned is not None or not self._has_sub_stream:
                        should_burst = True
                        break

            if not is_burst and self._has_sub_stream:
                if should_burst:
                    self._mode = "ACTIVE_BURST"
                    self._burst_start_ts = now_ts
                    logger.info(
                        "camera=%s >>> SWITCH TO ACTIVE_BURST MODE (unlocked vehicle track detected) <<<",
                        self.target.camera_id,
                    )

            # 5. Plate Recognition on active tracks (in ACTIVE_BURST or single-stream)
            if is_burst or not self._has_sub_stream:
                for tr in active_tracks:
                    if self._track_voter.is_locked(tr.track_id):
                        continue
                    sp_assigned = assign_vehicle_to_space(
                        tr.tlbr,
                        spaces_polygons,
                        ground_band_ratio=self.config.ground_band_ratio,
                        min_ioa=self.config.ground_min_ioa,
                    )
                    if tr.track_id in tracks_in_pre_entry or sp_assigned is not None or not self._has_sub_stream:
                        tb = [int(v) for v in tr.tlbr]
                        try:
                            veh_plates = self.ai_engine._detect_plates_in_vehicle(frame, tb)
                            if not veh_plates:
                                veh_plates = self.ai_engine._try_hyperlpr_in_vehicle(frame, tb)
                            for vp in veh_plates:
                                self._track_voter.add_sample(tr.track_id, vp, frame=frame, timestamp=now_ts)
                                self._plate_voter.update(vp.get("plate"))
                        except Exception as exc:
                            logger.debug("plate recognition on track %d error: %s", tr.track_id, exc)

                # Check ACTIVE_BURST exit condition
                if is_burst and self._has_sub_stream:
                    burst_elapsed = now_ts - self._burst_start_ts
                    timeout_reached = (burst_elapsed >= self.config.main_burst_max_sec)
                    if timeout_reached or not should_burst:
                        self._mode = "IDLE"
                        logger.info(
                            "camera=%s >>> BURST FINISHED -> IDLE (unlocked=%s, elapsed=%.1fs) <<<",
                            self.target.camera_id,
                            should_burst,
                            burst_elapsed,
                        )

            # 6. Update ParkingSpaceFSM for all spaces
            for sp_id, fsm in self._space_fsms.items():
                assigned_track = space_to_track.get(sp_id)
                event, payload = fsm.update(assigned_track, plate_voter=self._track_voter, timestamp=now_ts)
                if event != FsmEvent.NONE:
                    self._dispatch_fsm_event(event, payload, frame)

            # 7. Update status fields from primary space
            primary_fsm = self._space_fsms.get(self.target.space_id)
            if primary_fsm:
                self._last_has_vehicle = (primary_fsm.state == SpaceState.OCCUPIED)
                self._last_plate = primary_fsm.confirmed_plate
                self._last_vehicle_type = primary_fsm.confirmed_vehicle_type
                self._last_confidence = 0.90 if self._last_has_vehicle else 0.0
                if self._last_has_vehicle:
                    self._last_label = f"[{primary_fsm.state.value}] {primary_fsm.confirmed_plate or '车辆在位'}"
                else:
                    self._last_label = f"[{primary_fsm.state.value}] 车位空闲"

            # 8. Garbage collect dead tracks in voter
            active_ids = {t.track_id for t in active_tracks}
            self._track_voter.cleanup_tracks(active_ids)

            # 9. Debug Overlay Visualization (if DEBUG_DRAW_BOXES)
            debug_frame = None
            if self.config.debug_draw_boxes:
                debug_frame = self._draw_debug_overlay(frame, spaces_polygons, active_tracks, space_to_track)

            # 10. Snapshots, Heartbeats, and Periodic Detection Refreshes
            snap_frame = debug_frame if debug_frame is not None else frame
            if (now_ts - last_snapshot_push) >= self.config.snapshot_seconds:
                if self._last_has_vehicle and self._latest_plate_jpeg is not None and not self.config.debug_draw_boxes:
                    self._push_snapshot_bytes(self._latest_plate_jpeg, space_id=self.target.space_id)
                else:
                    self._push_snapshot(snap_frame, space_id=self.target.space_id)
                last_snapshot_push = now_ts

            if (now_ts - last_detection_push) >= 3.0:
                for sp_id, fsm in self._space_fsms.items():
                    if fsm.state == SpaceState.OCCUPIED:
                        self._push_detection_payload(
                            space_id=sp_id,
                            has_vehicle=True,
                            plate=fsm.confirmed_plate,
                            vehicle_type=fsm.confirmed_vehicle_type,
                            plate_color=fsm.confirmed_color,
                            confidence=0.90,
                            frame_bytes=self._latest_plate_jpeg,
                            force_cloud=False,
                        )
                last_detection_push = now_ts

            if (now_ts - last_heartbeat_push) >= self.config.heartbeat_seconds:
                self._push_heartbeat()
                last_heartbeat_push = now_ts

            spent = time.monotonic() - loop_started
            remaining = interval - spent
            if remaining > 0:
                self._stop_event.wait(remaining)

    def _push_snapshot_bytes(self, jpeg_bytes: bytes, space_id: str | None = None, force_cloud: bool = False) -> None:
        if not (self._main_connected or self._sub_connected):
            return
        sid = space_id or self.target.space_id
        url = f"{self.config.edge_url}/api/v1/spaces/{sid}/snapshot"
        extra = {"Content-Type": "image/jpeg"}
        if force_cloud:
            extra["X-Force-Cloud-Sync"] = "true"
        try:
            resp = self._session.post(url, data=jpeg_bytes, headers=self._headers(extra), timeout=3.0)
            if resp.status_code >= 400:
                logger.warning("snapshot upload HTTP %s for %s", resp.status_code, sid)
        except requests.RequestException as exc:
            logger.debug("snapshot upload error for %s: %s", sid, exc)

    def _push_snapshot(self, frame: Any, space_id: str | None = None, force_cloud: bool = False) -> None:
        ok, encoded = cv2.imencode(".jpg", frame, [cv2.IMWRITE_JPEG_QUALITY, 82])
        if ok:
            self._push_snapshot_bytes(encoded.tobytes(), space_id=space_id, force_cloud=force_cloud)

    def _push_detection_payload(
        self,
        space_id: str,
        has_vehicle: bool,
        plate: str | None,
        vehicle_type: str = "UNKNOWN",
        plate_color: str = "UNKNOWN",
        confidence: float = 0.90,
        frame_bytes: bytes | None = None,
        force_cloud: bool = False,
    ) -> None:
        url = f"{self.config.edge_url}/api/v1/spaces/{space_id}/detections"
        payload = {
            "spaceId": space_id,
            "hasVehicle": has_vehicle,
            "plate": plate,
            "vehicleType": vehicle_type,
            "capturedAt": datetime.now(timezone.utc).isoformat(),
            "isSimulated": False,
            "dataSource": "RTSP_OPENVINO_AI",
            "confidence": confidence,
            "plateColor": plate_color,
            "cameraId": self.target.camera_id,
        }
        if frame_bytes is not None:
            payload["imageBase64"] = base64.b64encode(frame_bytes).decode("ascii")

        try:
            resp = self._session.post(url, json=payload, headers=self._headers(), timeout=3.0)
            if resp.status_code >= 400:
                logger.warning("detection push HTTP %s for %s: %s", resp.status_code, space_id, resp.text[:200])
        except requests.RequestException as exc:
            logger.warning("detection push error for %s: %s", space_id, exc)

        if frame_bytes is not None and force_cloud:
            self._push_snapshot_bytes(frame_bytes, space_id=space_id, force_cloud=True)

    def _push_detection(self, perception: SpacePerception, current_frame: Any = None) -> None:
        """Legacy push detection interface for SpacePerception compatibility."""
        ev_bytes = None
        if perception.has_vehicle and self._latest_plate_jpeg is not None:
            ev_bytes = self._latest_plate_jpeg
        elif current_frame is not None:
            ok, enc = cv2.imencode(".jpg", current_frame, [cv2.IMWRITE_JPEG_QUALITY, 80])
            if ok:
                ev_bytes = enc.tobytes()

        self._push_detection_payload(
            space_id=perception.space_id,
            has_vehicle=perception.has_vehicle,
            plate=perception.plate,
            vehicle_type=perception.vehicle_type,
            plate_color=perception.plate_color,
            confidence=perception.confidence,
            frame_bytes=ev_bytes,
            force_cloud=False,
        )

    def _push_heartbeat(self) -> None:
        if not (self._main_connected or self._sub_connected):
            return
        url = f"{self.config.edge_url}/api/v1/cameras/{self.target.camera_id}/heartbeat"
        try:
            self._session.post(url, json={}, headers=self._headers(), timeout=3.0)
        except requests.RequestException:
            pass
