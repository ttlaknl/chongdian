"""Pure NumPy/SciPy ByteTrack multi-object tracker, trajectory-based quality-weighted plate voter, and 4-state parking space FSM.

Zero-torch runtime dependencies.
"""

from __future__ import annotations

import collections
import enum
import logging
import time
from typing import Any, Sequence

import numpy as np

try:
    from scipy.optimize import linear_sum_assignment

    _HAS_SCIPY = True
except ImportError:
    _HAS_SCIPY = False

logger = logging.getLogger("vision.space_tracker")


# ==============================================================================
# 1. Kalman Filter & Coordinate Geometry (Zero-Torch, Pure NumPy)
# ==============================================================================

def tlbr_to_xyah(tlbr: Sequence[float]) -> np.ndarray:
    """Convert [x1, y1, x2, y2] to [cx, cy, aspect_ratio, height]."""
    x1, y1, x2, y2 = tlbr[:4]
    w = max(1.0, float(x2 - x1))
    h = max(1.0, float(y2 - y1))
    cx = float(x1) + w / 2.0
    cy = float(y1) + h / 2.0
    a = w / h
    return np.array([cx, cy, a, h], dtype=np.float32)


def xyah_to_tlbr(xyah: Sequence[float]) -> np.ndarray:
    """Convert [cx, cy, aspect_ratio, height] to [x1, y1, x2, y2]."""
    cx, cy, a, h = xyah[:4]
    w = max(1.0, float(a * h))
    h = max(1.0, float(h))
    x1 = cx - w / 2.0
    y1 = cy - h / 2.0
    x2 = cx + w / 2.0
    y2 = cy + h / 2.0
    return np.array([x1, y1, x2, y2], dtype=np.float32)


def box_iou_matrix(boxes_a: np.ndarray, boxes_b: np.ndarray) -> np.ndarray:
    """Compute pairwise IoU between two arrays of [x1, y1, x2, y2] boxes."""
    if len(boxes_a) == 0 or len(boxes_b) == 0:
        return np.zeros((len(boxes_a), len(boxes_b)), dtype=np.float32)

    a = np.asarray(boxes_a, dtype=np.float32)
    b = np.asarray(boxes_b, dtype=np.float32)

    # Broadcast intersection
    ix1 = np.maximum(a[:, 0:1], b[:, 0:1].T)
    iy1 = np.maximum(a[:, 1:2], b[:, 1:2].T)
    ix2 = np.minimum(a[:, 2:3], b[:, 2:3].T)
    iy2 = np.minimum(a[:, 3:4], b[:, 3:4].T)

    iw = np.maximum(0.0, ix2 - ix1)
    ih = np.maximum(0.0, iy2 - iy1)
    inter_area = iw * ih

    area_a = (a[:, 2] - a[:, 0]) * (a[:, 3] - a[:, 1])
    area_b = (b[:, 2] - b[:, 0]) * (b[:, 3] - b[:, 1])
    union_area = area_a[:, None] + area_b[None, :] - inter_area
    union_area = np.maximum(1e-6, union_area)

    return inter_area / union_area


def linear_assignment(
    cost_matrix: np.ndarray,
    thresh: float,
) -> tuple[np.ndarray, np.ndarray, np.ndarray]:
    """Solve linear assignment using scipy.optimize.linear_sum_assignment or greedy fallback."""
    if cost_matrix.size == 0:
        return (
            np.empty((0, 2), dtype=int),
            np.arange(cost_matrix.shape[0]),
            np.arange(cost_matrix.shape[1]),
        )

    if _HAS_SCIPY:
        row_ind, col_ind = linear_sum_assignment(cost_matrix)
    else:
        # Pure NumPy greedy minimum cost fallback
        cost = cost_matrix.copy()
        row_ind_list = []
        col_ind_list = []
        while cost.size > 0:
            min_val = np.min(cost)
            if min_val > thresh or np.isinf(min_val):
                break
            r, c = np.unravel_index(np.argmin(cost), cost.shape)
            row_ind_list.append(r)
            col_ind_list.append(c)
            cost[r, :] = np.inf
            cost[:, c] = np.inf
        row_ind = np.array(row_ind_list, dtype=int)
        col_ind = np.array(col_ind_list, dtype=int)

    matches = []
    unmatched_rows = list(range(cost_matrix.shape[0]))
    unmatched_cols = list(range(cost_matrix.shape[1]))

    for r, c in zip(row_ind, col_ind):
        if cost_matrix[r, c] <= thresh:
            matches.append([r, c])
            if r in unmatched_rows:
                unmatched_rows.remove(r)
            if c in unmatched_cols:
                unmatched_cols.remove(c)

    matches_arr = np.array(matches, dtype=int) if matches else np.empty((0, 2), dtype=int)
    return matches_arr, np.array(unmatched_rows, dtype=int), np.array(unmatched_cols, dtype=int)


class KalmanFilter:
    """Discrete 8-state constant velocity Kalman filter for vehicle bounding box tracking."""

    def __init__(self) -> None:
        ndim, dt = 4, 1.0
        self._motion_mat = np.eye(2 * ndim, 2 * ndim, dtype=np.float32)
        for i in range(ndim):
            self._motion_mat[i, ndim + i] = dt
        self._update_mat = np.eye(ndim, 2 * ndim, dtype=np.float32)
        self._std_weight_position = 1.0 / 20.0
        self._std_weight_velocity = 1.0 / 160.0

    def initiate(self, measurement: np.ndarray) -> tuple[np.ndarray, np.ndarray]:
        mean_pos = measurement
        mean_vel = np.zeros_like(mean_pos)
        mean = np.r_[mean_pos, mean_vel]

        std = [
            2.0 * self._std_weight_position * measurement[3],
            2.0 * self._std_weight_position * measurement[3],
            1e-2,
            2.0 * self._std_weight_position * measurement[3],
            10.0 * self._std_weight_velocity * measurement[3],
            10.0 * self._std_weight_velocity * measurement[3],
            1e-5,
            10.0 * self._std_weight_velocity * measurement[3],
        ]
        covariance = np.diag(np.square(std))
        return mean, covariance

    def predict(self, mean: np.ndarray, covariance: np.ndarray) -> tuple[np.ndarray, np.ndarray]:
        std_pos = [
            self._std_weight_position * mean[3],
            self._std_weight_position * mean[3],
            1e-2,
            self._std_weight_position * mean[3],
        ]
        std_vel = [
            self._std_weight_velocity * mean[3],
            self._std_weight_velocity * mean[3],
            1e-5,
            self._std_weight_velocity * mean[3],
        ]
        motion_cov = np.diag(np.square(np.r_[std_pos, std_vel]))
        mean = np.dot(self._motion_mat, mean)
        covariance = np.linalg.multi_dot((self._motion_mat, covariance, self._motion_mat.T)) + motion_cov
        return mean, covariance

    def project(self, mean: np.ndarray, covariance: np.ndarray) -> tuple[np.ndarray, np.ndarray]:
        std = [
            self._std_weight_position * mean[3],
            self._std_weight_position * mean[3],
            1e-1,
            self._std_weight_position * mean[3],
        ]
        innovation_cov = np.diag(np.square(std))
        projected_mean = np.dot(self._update_mat, mean)
        projected_cov = np.linalg.multi_dot((self._update_mat, covariance, self._update_mat.T)) + innovation_cov
        return projected_mean, projected_cov

    def update(
        self,
        mean: np.ndarray,
        covariance: np.ndarray,
        measurement: np.ndarray,
    ) -> tuple[np.ndarray, np.ndarray]:
        projected_mean, projected_cov = self.project(mean, covariance)
        chol_factor, lower = np.linalg.cholesky(projected_cov), True
        kalman_gain = np.linalg.solve(chol_factor.T, np.linalg.solve(chol_factor, self._update_mat.dot(covariance))).T
        innovation = measurement - projected_mean
        new_mean = mean + np.dot(innovation, kalman_gain.T)
        new_covariance = covariance - np.linalg.multi_dot((kalman_gain, projected_cov, kalman_gain.T))
        return new_mean, new_covariance


# ==============================================================================
# 2. STrack (Single Target Track) & ByteTrack Implementation
# ==============================================================================

class TrackState(enum.Enum):
    Tracked = 1
    Lost = 2
    Removed = 3


class STrack:
    """Individual vehicle trajectory tracked across frames."""

    _count = 0

    def __init__(
        self,
        tlbr: Sequence[float],
        score: float,
        category_type: str = "CAR",
        class_name: str = "car",
    ) -> None:
        self._tlbr = np.asarray(tlbr, dtype=np.float32)
        self.score = float(score)
        self.category_type = category_type
        self.class_name = class_name

        self.kalman_filter = KalmanFilter()
        self.mean: np.ndarray | None = None
        self.covariance: np.ndarray | None = None

        self.track_id = 0
        self.is_activated = False
        self.state = TrackState.Tracked

        self.frame_id = 0
        self.start_frame = 0
        self.time_since_update = 0
        self.last_seen_ts = 0.0

    @classmethod
    def reset_counter(cls) -> None:
        cls._count = 0

    @classmethod
    def next_id(cls) -> int:
        cls._count += 1
        return cls._count

    @property
    def tlbr(self) -> np.ndarray:
        if self.mean is None:
            return self._tlbr
        ret = xyah_to_tlbr(self.mean[:4])
        return ret

    def activate(self, kalman_filter: KalmanFilter, frame_id: int, timestamp: float) -> None:
        self.kalman_filter = kalman_filter
        self.track_id = self.next_id()
        self.mean, self.covariance = self.kalman_filter.initiate(tlbr_to_xyah(self._tlbr))
        self.frame_id = frame_id
        self.start_frame = frame_id
        self.last_seen_ts = timestamp
        self.time_since_update = 0
        self.state = TrackState.Tracked
        self.is_activated = True

    def re_activate(self, new_track: "STrack", frame_id: int, timestamp: float, new_id: bool = False) -> None:
        measurement = tlbr_to_xyah(new_track.tlbr)
        if self.mean is not None and self.covariance is not None:
            self.mean, self.covariance = self.kalman_filter.update(self.mean, self.covariance, measurement)
        else:
            self.mean, self.covariance = self.kalman_filter.initiate(measurement)
        self.frame_id = frame_id
        self.last_seen_ts = timestamp
        self.time_since_update = 0
        self.state = TrackState.Tracked
        self.is_activated = True
        self.score = new_track.score
        if new_id:
            self.track_id = self.next_id()

    def update(self, new_track: "STrack", frame_id: int, timestamp: float) -> None:
        measurement = tlbr_to_xyah(new_track.tlbr)
        if self.mean is not None and self.covariance is not None:
            self.mean, self.covariance = self.kalman_filter.update(self.mean, self.covariance, measurement)
        else:
            self.mean, self.covariance = self.kalman_filter.initiate(measurement)
        self.frame_id = frame_id
        self.last_seen_ts = timestamp
        self.time_since_update = 0
        self.state = TrackState.Tracked
        self.is_activated = True
        self.score = new_track.score

    def predict(self) -> None:
        if self.mean is not None and self.covariance is not None:
            if self.state != TrackState.Tracked:
                self.mean[7] = 0.0
            self.mean, self.covariance = self.kalman_filter.predict(self.mean, self.covariance)

    def mark_lost(self) -> None:
        self.state = TrackState.Lost

    def mark_removed(self) -> None:
        self.state = TrackState.Removed


class ByteTracker:
    """Pure NumPy two-stage ByteTrack multi-vehicle tracker."""

    def __init__(
        self,
        track_high_thresh: float = 0.45,
        track_low_thresh: float = 0.20,
        match_thresh: float = 0.80,
        track_max_lost_sec: float = 3.0,
    ) -> None:
        self.track_high_thresh = track_high_thresh
        self.track_low_thresh = track_low_thresh
        self.match_thresh = match_thresh
        self.track_max_lost_sec = track_max_lost_sec

        self.tracked_stracks: list[STrack] = []
        self.lost_stracks: list[STrack] = []
        self.removed_stracks: list[STrack] = []

        self.frame_id = 0
        self.kalman_filter = KalmanFilter()

    def reset(self) -> None:
        self.tracked_stracks.clear()
        self.lost_stracks.clear()
        self.removed_stracks.clear()
        self.frame_id = 0
        STrack.reset_counter()

    def update(
        self,
        detections: list[dict[str, Any]],
        timestamp: float | None = None,
    ) -> list[STrack]:
        """Update tracker with bounding boxes from vehicle detection.

        Args:
            detections: List of dicts with 'bbox' [x1, y1, x2, y2], 'confidence', 'category_type', 'class_name'.
            timestamp: Monotonic or virtual test timestamp (seconds).

        Returns:
            List of active confirmed STrack instances.
        """
        self.frame_id += 1
        now_ts = timestamp if timestamp is not None else time.monotonic()

        activated_starcks: list[STrack] = []
        refind_stracks: list[STrack] = []
        lost_stracks: list[STrack] = []
        removed_stracks: list[STrack] = []

        # 1. Separate detections into high-score and low-score subsets
        dets_high: list[STrack] = []
        dets_low: list[STrack] = []
        for det in detections:
            bbox = det["bbox"]
            conf = float(det.get("confidence", 0.0))
            cat = str(det.get("category_type", "CAR"))
            cls_name = str(det.get("class_name", "car"))
            strack = STrack(bbox, conf, category_type=cat, class_name=cls_name)
            if conf >= self.track_high_thresh:
                dets_high.append(strack)
            elif conf >= self.track_low_thresh:
                dets_low.append(strack)

        # 2. Predict positions of existing tracks
        for track in self.tracked_stracks:
            track.predict()
        for track in self.lost_stracks:
            track.predict()

        # 3. First stage association: match tracked tracks with high-confidence detections
        track_pool = [t for t in self.tracked_stracks if t.is_activated]
        boxes_track = np.array([t.tlbr for t in track_pool]) if track_pool else np.empty((0, 4))
        boxes_high = np.array([d.tlbr for d in dets_high]) if dets_high else np.empty((0, 4))

        cost_matrix = 1.0 - box_iou_matrix(boxes_track, boxes_high)
        matches_a, u_track, u_det_high = linear_assignment(cost_matrix, thresh=self.match_thresh)

        for itracked, idet in matches_a:
            track = track_pool[itracked]
            det = dets_high[idet]
            track.update(det, self.frame_id, now_ts)
            activated_starcks.append(track)

        # 4. Second stage association: match remaining tracks with low-confidence detections
        r_tracked_stracks = [track_pool[i] for i in u_track]
        boxes_r_track = np.array([t.tlbr for t in r_tracked_stracks]) if r_tracked_stracks else np.empty((0, 4))
        boxes_low = np.array([d.tlbr for d in dets_low]) if dets_low else np.empty((0, 4))

        cost_matrix_low = 1.0 - box_iou_matrix(boxes_r_track, boxes_low)
        matches_b, u_r_track, _ = linear_assignment(cost_matrix_low, thresh=0.50)

        for itracked, idet in matches_b:
            track = r_tracked_stracks[itracked]
            det = dets_low[idet]
            track.update(det, self.frame_id, now_ts)
            activated_starcks.append(track)

        for it in u_r_track:
            track = r_tracked_stracks[it]
            track.mark_lost()
            lost_stracks.append(track)

        # 5. Third stage association: match unmatched high-confidence detections with lost tracks
        unmatched_high_dets = [dets_high[i] for i in u_det_high]
        boxes_lost = np.array([t.tlbr for t in self.lost_stracks]) if self.lost_stracks else np.empty((0, 4))
        boxes_u_high = np.array([d.tlbr for d in unmatched_high_dets]) if unmatched_high_dets else np.empty((0, 4))

        cost_matrix_lost = 1.0 - box_iou_matrix(boxes_lost, boxes_u_high)
        matches_c, u_lost, u_detections = linear_assignment(cost_matrix_lost, thresh=0.70)

        for ilost, idet in matches_c:
            track = self.lost_stracks[ilost]
            det = unmatched_high_dets[idet]
            track.re_activate(det, self.frame_id, now_ts)
            refind_stracks.append(track)

        # 6. Initialize new tracks for remaining unmatched high-confidence detections
        for inew in u_detections:
            track = unmatched_high_dets[inew]
            if track.score >= self.track_high_thresh:
                track.activate(self.kalman_filter, self.frame_id, now_ts)
                activated_starcks.append(track)

        # 7. Update lost tracks and remove expired tracks
        for ilost in u_lost:
            track = self.lost_stracks[ilost]
            lost_stracks.append(track)

        # Filter out tracks that have been lost for longer than track_max_lost_sec
        alive_lost: list[STrack] = []
        for track in lost_stracks:
            if (now_ts - track.last_seen_ts) > self.track_max_lost_sec:
                track.mark_removed()
                removed_stracks.append(track)
            else:
                alive_lost.append(track)

        self.tracked_stracks = [t for t in (activated_starcks + refind_stracks) if t.state == TrackState.Tracked]
        self.lost_stracks = alive_lost
        self.removed_stracks.extend(removed_stracks)

        return self.tracked_stracks


# ==============================================================================
# 3. TrackPlateVoter (Trajectory Quality-Weighted Plate Voting Pool)
# ==============================================================================

class TrackPlateVoter:
    """Trajectory-based quality-weighted license plate voting pool.

    Maintains plate recognition history per track_id, computes multi-factor quality scores,
    and locks high-confidence consensus plates while preserving the clearest snapshot.
    """

    def __init__(
        self,
        min_hits: int = 2,
        lock_score_thresh: float = 1.50,
        max_history_per_track: int = 15,
    ) -> None:
        self.min_hits = max(1, min_hits)
        self.lock_score_thresh = lock_score_thresh
        self.max_history = max_history_per_track

        # Per-track storage: track_id -> dict
        # {
        #   "scores": dict[plate_no, float],
        #   "hits": dict[plate_no, int],
        #   "colors": dict[plate_no, dict[color, float]],
        #   "best_frames": dict[plate_no, tuple[float, Any]],
        #   "plate_locked": bool,
        #   "locked_plate": str | None,
        #   "locked_color": str,
        #   "locked_vehicle_type": str,
        #   "best_evidence": Any | None,
        # }
        self._tracks: dict[int, dict[str, Any]] = {}

    def reset(self) -> None:
        self._tracks.clear()

    @staticmethod
    def compute_quality_score(
        laplacian_var: float = 0.0,
        plate_area: float = 0.0,
        plate_det_conf: float = 0.80,
        ocr_avg_conf: float = 0.85,
    ) -> float:
        """Calculate normalized multi-factor plate quality score in [0.0, 1.0]."""
        norm_lap = min(max(0.0, float(laplacian_var)) / 300.0, 1.0)
        norm_area = min(max(0.0, float(plate_area)) / (160.0 * 45.0), 1.0)
        det_c = min(max(0.0, float(plate_det_conf)), 1.0)
        ocr_c = min(max(0.0, float(ocr_avg_conf)), 1.0)

        score = (
            0.20 * norm_lap
            + 0.20 * norm_area
            + 0.25 * det_c
            + 0.35 * ocr_c
        )
        return float(np.clip(score, 0.0, 1.0))

    def _ensure_track(self, track_id: int) -> dict[str, Any]:
        if track_id not in self._tracks:
            self._tracks[track_id] = {
                "scores": collections.defaultdict(float),
                "hits": collections.defaultdict(int),
                "colors": collections.defaultdict(lambda: collections.defaultdict(float)),
                "best_frames": {},
                "plate_locked": False,
                "locked_plate": None,
                "locked_color": "UNKNOWN",
                "locked_vehicle_type": "UNKNOWN",
                "best_evidence": None,
            }
        return self._tracks[track_id]

    def is_locked(self, track_id: int) -> bool:
        rec = self._tracks.get(track_id)
        return bool(rec and rec["plate_locked"])

    def add_sample(
        self,
        track_id: int,
        plate_info: dict[str, Any],
        frame: Any = None,
        timestamp: float | None = None,
    ) -> tuple[str | None, bool]:
        """Add recognition observation for a specific track_id.

        Args:
            track_id: ByteTrack tracking ID.
            plate_info: Dict with 'plate', 'plate_color', 'vehicle_type', 'confidence', 'ocr_confidence', 'laplacian_var', 'plate_area'.
            frame: Raw BGR frame or encoded JPEG bytes.
            timestamp: Monotonic or virtual test timestamp.

        Returns:
            (locked_plate, is_locked)
        """
        rec = self._ensure_track(track_id)
        if rec["plate_locked"]:
            return rec["locked_plate"], True

        plate_no = str(plate_info.get("plate") or "").strip().upper()
        if not plate_no:
            return None, False

        color = str(plate_info.get("plate_color") or "UNKNOWN").strip().upper()
        v_type = str(plate_info.get("vehicle_type") or "FUEL").strip().upper()
        det_c = float(plate_info.get("confidence", 0.70))
        ocr_c = float(plate_info.get("ocr_confidence", 0.75))
        lap_v = float(plate_info.get("laplacian_var", 50.0))
        area = float(plate_info.get("plate_area", 2000.0))

        score = self.compute_quality_score(lap_v, area, det_c, ocr_c)

        rec["scores"][plate_no] += score
        rec["hits"][plate_no] += 1
        rec["colors"][plate_no][color] += score

        # Store best frame snapshot for this plate
        curr_best = rec["best_frames"].get(plate_no)
        if curr_best is None or score > curr_best[0]:
            rec["best_frames"][plate_no] = (score, frame)

        # Check lock conditions
        cum_score = rec["scores"][plate_no]
        hits = rec["hits"][plate_no]

        if hits >= self.min_hits and cum_score >= self.lock_score_thresh:
            rec["plate_locked"] = True
            rec["locked_plate"] = plate_no
            # Select color with maximum accumulated weighted score
            color_scores = rec["colors"][plate_no]
            best_color = max(color_scores.items(), key=lambda kv: kv[1])[0] if color_scores else color
            rec["locked_color"] = best_color
            rec["locked_vehicle_type"] = v_type
            rec["best_evidence"] = rec["best_frames"][plate_no][1]

            logger.info(
                "track_id=%d >>> PLATE LOCKED: %s (%s, %s) hits=%d, cum_score=%.2f <<<",
                track_id,
                plate_no,
                best_color,
                v_type,
                hits,
                cum_score,
            )
            return plate_no, True

        return None, False

    def get_locked_plate(self, track_id: int) -> tuple[str | None, str, str, Any | None, bool]:
        """Return (locked_plate, plate_color, vehicle_type, best_evidence_frame, is_locked)."""
        rec = self._tracks.get(track_id)
        if not rec:
            return None, "UNKNOWN", "UNKNOWN", None, False

        if rec["plate_locked"]:
            return (
                rec["locked_plate"],
                rec["locked_color"],
                rec["locked_vehicle_type"],
                rec["best_evidence"],
                True,
            )

        # If not locked yet, pick current highest scoring candidate if hits >= 1
        if rec["scores"]:
            best_p, best_s = max(rec["scores"].items(), key=lambda kv: kv[1])
            c_dict = rec["colors"][best_p]
            best_c = max(c_dict.items(), key=lambda kv: kv[1])[0] if c_dict else "UNKNOWN"
            best_fr = rec["best_frames"].get(best_p, (0.0, None))[1]
            return best_p, best_c, "FUEL", best_fr, False

        return None, "UNKNOWN", "UNKNOWN", None, False

    def cleanup_tracks(self, active_track_ids: set[int]) -> None:
        """Garbage collect tracks that are no longer active in tracker."""
        dead_ids = [tid for tid in self._tracks if tid not in active_track_ids]
        for tid in dead_ids:
            del self._tracks[tid]


# ==============================================================================
# 4. ParkingSpaceFSM (4-State Parking Space Finite State Machine)
# ==============================================================================

class SpaceState(enum.Enum):
    FREE = "FREE"
    ENTERING = "ENTERING"
    OCCUPIED = "OCCUPIED"
    LEAVING = "LEAVING"


class FsmEvent(enum.Enum):
    NONE = "NONE"
    VEHICLE_ENTERED = "VEHICLE_ENTERED"
    PLATE_UPDATED = "PLATE_UPDATED"
    VEHICLE_LEFT = "VEHICLE_LEFT"


class ParkingSpaceFSM:
    """Individual parking space 4-state state machine (FREE -> ENTERING -> OCCUPIED -> LEAVING)."""

    def __init__(
        self,
        space_id: str,
        enter_confirm_sec: float = 3.0,
        occupied_confirm_sec: float = 8.0,
        leave_confirm_sec: float = 5.0,
    ) -> None:
        self.space_id = space_id
        self.enter_confirm_sec = enter_confirm_sec
        self.occupied_confirm_sec = occupied_confirm_sec
        self.leave_confirm_sec = leave_confirm_sec

        self.state = SpaceState.FREE
        self.occupied_track_id: int | None = None
        self.confirmed_plate: str | None = None
        self.confirmed_color: str = "UNKNOWN"
        self.confirmed_vehicle_type: str = "UNKNOWN"
        self.best_evidence: Any | None = None

        self.enter_candidate_since: float | None = None
        self.occupied_since: float | None = None
        self.leave_candidate_since: float | None = None

    def reset(self) -> None:
        self.state = SpaceState.FREE
        self.occupied_track_id = None
        self.confirmed_plate = None
        self.confirmed_color = "UNKNOWN"
        self.confirmed_vehicle_type = "UNKNOWN"
        self.best_evidence = None
        self.enter_candidate_since = None
        self.occupied_since = None
        self.leave_candidate_since = None

    def update(
        self,
        assigned_track: STrack | None,
        plate_voter: TrackPlateVoter | None = None,
        timestamp: float | None = None,
    ) -> tuple[FsmEvent, dict[str, Any] | None]:
        """Drive state machine transition based on ground-band spatial containment.

        Args:
            assigned_track: STrack instance assigned to this space by ground band IoA, or None.
            plate_voter: TrackPlateVoter instance holding trajectory-locked plates.
            timestamp: Monotonic or virtual test timestamp.

        Returns:
            (event, payload)
        """
        now = timestamp if timestamp is not None else time.monotonic()

        # ----------------------------------------------------------------------
        # State 1: FREE
        # ----------------------------------------------------------------------
        if self.state == SpaceState.FREE:
            if assigned_track is not None:
                if self.enter_candidate_since is None:
                    self.enter_candidate_since = now
                    self.occupied_track_id = assigned_track.track_id
                else:
                    elapsed = now - self.enter_candidate_since
                    if elapsed >= self.enter_confirm_sec:
                        self.state = SpaceState.ENTERING
                        self.occupied_track_id = assigned_track.track_id
                        logger.info("space=%s >>> ENTERING (dwell=%.1fs) <<<", self.space_id, elapsed)
            else:
                self.enter_candidate_since = None
                self.occupied_track_id = None
            return FsmEvent.NONE, None

        # ----------------------------------------------------------------------
        # State 2: ENTERING (Anti-jitter confirmation for drive-throughs)
        # ----------------------------------------------------------------------
        if self.state == SpaceState.ENTERING:
            if assigned_track is not None:
                self.occupied_track_id = assigned_track.track_id
                enter_start = self.enter_candidate_since if self.enter_candidate_since is not None else now
                elapsed = now - enter_start

                if elapsed >= self.occupied_confirm_sec:
                    self.state = SpaceState.OCCUPIED
                    self.occupied_since = enter_start
                    self.leave_candidate_since = None

                    # Inherit trajectory-locked plate from plate voter
                    if plate_voter is not None and self.occupied_track_id is not None:
                        l_plate, l_col, v_type, ev_frame, _ = plate_voter.get_locked_plate(self.occupied_track_id)
                        self.confirmed_plate = l_plate
                        self.confirmed_color = l_col
                        self.confirmed_vehicle_type = v_type
                        self.best_evidence = ev_frame

                    logger.info(
                        "space=%s >>> OCCUPIED: plate=%s, color=%s, type=%s, track_id=%s <<<",
                        self.space_id,
                        self.confirmed_plate,
                        self.confirmed_color,
                        self.confirmed_vehicle_type,
                        self.occupied_track_id,
                    )

                    payload = {
                        "spaceId": self.space_id,
                        "hasVehicle": True,
                        "plate": self.confirmed_plate,
                        "plateColor": self.confirmed_color,
                        "vehicleType": self.confirmed_vehicle_type,
                        "occupiedSince": self.occupied_since,
                        "bestEvidence": self.best_evidence,
                        "trackId": self.occupied_track_id,
                    }
                    return FsmEvent.VEHICLE_ENTERED, payload
            else:
                # Vehicle left before occupied_confirm_sec -> Transient drive-through, revert to FREE
                logger.info("space=%s transient vehicle left during ENTERING, revert to FREE", self.space_id)
                self.state = SpaceState.FREE
                self.enter_candidate_since = None
                self.occupied_track_id = None
            return FsmEvent.NONE, None

        # ----------------------------------------------------------------------
        # State 3: OCCUPIED (Stationary parking with delayed plate update support)
        # ----------------------------------------------------------------------
        if self.state == SpaceState.OCCUPIED:
            if assigned_track is not None:
                self.leave_candidate_since = None
                self.occupied_track_id = assigned_track.track_id

                # Delayed plate resolution: if initially entered with no plate, check if plate is now locked
                if self.confirmed_plate is None and plate_voter is not None and self.occupied_track_id is not None:
                    l_plate, l_col, v_type, ev_frame, is_locked = plate_voter.get_locked_plate(self.occupied_track_id)
                    if l_plate is not None:
                        self.confirmed_plate = l_plate
                        self.confirmed_color = l_col
                        self.confirmed_vehicle_type = v_type
                        self.best_evidence = ev_frame
                        logger.info("space=%s delayed plate resolved: %s", self.space_id, l_plate)
                        payload = {
                            "spaceId": self.space_id,
                            "hasVehicle": True,
                            "plate": self.confirmed_plate,
                            "plateColor": self.confirmed_color,
                            "vehicleType": self.confirmed_vehicle_type,
                            "trackId": self.occupied_track_id,
                            "bestEvidence": self.best_evidence,
                        }
                        return FsmEvent.PLATE_UPDATED, payload
            else:
                # Vehicle disappeared: initiate leave debounce
                if self.leave_candidate_since is None:
                    self.leave_candidate_since = now
                elapsed_leave = now - self.leave_candidate_since
                if elapsed_leave >= 1.0:
                    self.state = SpaceState.LEAVING
                    logger.info("space=%s vehicle missing for %.1fs -> LEAVING", self.space_id, elapsed_leave)
            return FsmEvent.NONE, None

        # ----------------------------------------------------------------------
        # State 4: LEAVING (Anti-jitter confirmation for transient occlusions)
        # ----------------------------------------------------------------------
        if self.state == SpaceState.LEAVING:
            if assigned_track is not None:
                # Vehicle reappeared within leave_confirm_sec -> Revert to OCCUPIED
                logger.info("space=%s vehicle reappeared within leave confirmation -> OCCUPIED", self.space_id)
                self.state = SpaceState.OCCUPIED
                self.leave_candidate_since = None
                self.occupied_track_id = assigned_track.track_id
            else:
                leave_start = self.leave_candidate_since if self.leave_candidate_since is not None else now
                elapsed_leave = now - leave_start

                if elapsed_leave >= self.leave_confirm_sec:
                    self.state = SpaceState.FREE
                    occupied_duration = (now - self.occupied_since) if self.occupied_since else 0.0

                    logger.info(
                        "space=%s >>> VACATED: duration=%.1fs, was plate=%s <<<",
                        self.space_id,
                        occupied_duration,
                        self.confirmed_plate,
                    )

                    payload = {
                        "spaceId": self.space_id,
                        "hasVehicle": False,
                        "plate": self.confirmed_plate,
                        "occupiedDurationSeconds": int(occupied_duration),
                        "trackId": self.occupied_track_id,
                    }

                    # Reset space state
                    self.occupied_track_id = None
                    self.confirmed_plate = None
                    self.confirmed_color = "UNKNOWN"
                    self.confirmed_vehicle_type = "UNKNOWN"
                    self.best_evidence = None
                    self.enter_candidate_since = None
                    self.occupied_since = None
                    self.leave_candidate_since = None

                    return FsmEvent.VEHICLE_LEFT, payload

            return FsmEvent.NONE, None

        return FsmEvent.NONE, None
