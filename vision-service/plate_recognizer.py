"""Chinese license plate validation, color classification, ROI cropping, and rolling voting."""

from __future__ import annotations

import json
from collections import Counter, deque
from dataclasses import dataclass
from typing import Any, Sequence

import cv2
import numpy as np

from geometry import split_and_merge_double_plate, warp_plate_perspective
from plate_postprocess import (
    assess_plate_illumination,
    enhance_backlit_plate,
    is_valid_chinese_plate,
    normalize_plate_text,
    STANDARD_PLATE_RE,
    NEV_PLATE_RE,
    MILITARY_PLATE_RE,
    WJ_PLATE_RE,
)


@dataclass(frozen=True)
class PlateDetection:
    """Single-frame recognition candidate."""

    plate: str
    plate_color: str
    vehicle_type: str
    confidence: float
    # Milestone 2A metadata
    laplacian_var: float = 0.0
    plate_area: int = 0
    warped: bool = False
    enhanced: bool = False
    mean_luma: float = 0.0
    std_luma: float = 0.0


def parse_plate_detection_output(
    detection: dict[str, Any] | Sequence[float | int],
    has_keypoints_mode: str = "auto",
) -> dict[str, Any]:
    """Parse plate detector output in either 4-corner keypoint or traditional 4-box bounding format.

    Supports:
    - Dict format: {'bbox': [...], 'confidence': ..., 'layer_type': ..., 'landmarks': [...]}
    - Flat sequence: [x1, y1, x2, y2, conf, cls, x_tl, y_tl, x_tr, y_tr, x_br, y_br, x_bl, y_bl] (14+ items)
    - Flat sequence: [x1, y1, x2, y2, conf, cls] (6 items)
    - Flat sequence: [x1, y1, x2, y2, conf] (5 items)

    Returns:
        Standardized dict with bbox, confidence, layer_type, is_double_layer, landmarks, has_keypoints.
    """
    kpt_mode = str(has_keypoints_mode).strip().lower()

    if isinstance(detection, dict):
        raw_box = detection.get("bbox") or [0, 0, 0, 0]
        bbox = [int(round(float(v))) for v in raw_box[:4]]
        conf = float(detection.get("confidence", 0.0))
        layer_type = int(detection.get("layer_type", 0))
        raw_landmarks = detection.get("landmarks")
    elif isinstance(detection, (list, tuple, np.ndarray)):
        arr = [float(v) for v in detection]
        bbox = [int(round(v)) for v in arr[:4]]
        conf = float(arr[4]) if len(arr) >= 5 else 1.0
        layer_type = int(arr[5]) if len(arr) >= 6 else 0
        if len(arr) >= 14:
            # 8 coordinates: [x_tl, y_tl, x_tr, y_tr, x_br, y_br, x_bl, y_bl]
            raw_landmarks = [
                [int(round(arr[6])), int(round(arr[7]))],
                [int(round(arr[8])), int(round(arr[9]))],
                [int(round(arr[10])), int(round(arr[11]))],
                [int(round(arr[12])), int(round(arr[13]))],
            ]
        else:
            raw_landmarks = None
    else:
        bbox = [0, 0, 0, 0]
        conf = 0.0
        layer_type = 0
        raw_landmarks = None

    # Aspect ratio & double layer determination
    bw = max(1.0, float(bbox[2] - bbox[0]))
    bh = max(1.0, float(bbox[3] - bbox[1]))
    aspect_ratio = bw / bh
    is_double = (layer_type == 1) or (aspect_ratio < 2.1)

    # Keypoints handling
    landmarks: list[list[int]] | None = None
    if kpt_mode != "false" and raw_landmarks is not None and len(raw_landmarks) >= 4:
        landmarks = [[int(pt[0]), int(pt[1])] for pt in raw_landmarks[:4]]

    return {
        "bbox": bbox,
        "confidence": conf,
        "layer_type": layer_type,
        "is_double_layer": is_double,
        "landmarks": landmarks,
        "has_keypoints": landmarks is not None,
    }


def extract_plate_patch(
    image: np.ndarray,
    parsed_plate: dict[str, Any],
    safe_pad_ratio: float = 0.04,
    target_single: tuple[int, int] = (168, 48),
    target_double: tuple[int, int] = (168, 96),
) -> tuple[np.ndarray, bool]:
    """Extract standard-sized rectified plate patch from image.

    If 4 keypoints are present, uses perspective transform;
    otherwise falls back to safe 4% padding horizontal box cropping and resize.

    Returns:
        (patch_image, was_warped)
    """
    if image is None or image.size == 0:
        return np.zeros((target_single[1], target_single[0], 3), dtype=np.uint8), False

    landmarks = parsed_plate.get("landmarks")
    is_double = parsed_plate.get("is_double_layer", False)
    ih, iw = image.shape[:2]

    if landmarks is not None and len(landmarks) >= 4:
        try:
            warped = warp_plate_perspective(
                image,
                landmarks,
                is_double_layer=is_double,
                target_single=target_single,
                target_double=target_double,
            )
            return warped, True
        except Exception:
            pass

    # Traditional 4-box fallback: expand by safe_pad_ratio (4%)
    x1, y1, x2, y2 = parsed_plate["bbox"]
    bw = max(1, x2 - x1)
    bh = max(1, y2 - y1)
    dx = int(round(bw * safe_pad_ratio))
    dy = int(round(bh * safe_pad_ratio))
    px1 = max(0, x1 - dx)
    py1 = max(0, y1 - dy)
    px2 = min(iw, x2 + dx)
    py2 = min(ih, y2 + dy)
    crop = image[py1:py2, px1:px2]

    if crop.size == 0:
        crop = image[max(0, y1) : min(ih, y2), max(0, x1) : min(iw, x2)]

    if crop.size == 0:
        return np.zeros((target_single[1], target_single[0], 3), dtype=np.uint8), False

    if is_double:
        crop = split_and_merge_double_plate(crop)

    resized = cv2.resize(crop, target_single, interpolation=cv2.INTER_LINEAR)
    return resized, False


@dataclass(frozen=True)
class VoteResult:
    """Stabilized space detection state after multi-frame consensus voting."""

    has_vehicle: bool
    plate: str | None
    plate_color: str
    vehicle_type: str
    confidence: float
    confirmed: bool


def normalize_plate(raw: str | None) -> str:
    """Normalize whitespace, separators, and case for a plate string."""
    return normalize_plate_text(raw)


def classify_plate(
    raw_plate: str | None,
    raw_type: Any = None,
    color_hint: str | None = None,
) -> tuple[str, str]:
    """Map HyperLPR3 plate type + plate regex structure into (plate_color, vehicle_type)."""
    plate = normalize_plate(raw_plate)

    # 1. 军车与武警判定
    if MILITARY_PLATE_RE.match(plate) or WJ_PLATE_RE.match(plate) or "警" in plate:
        return ("WHITE", "SPECIAL_VEHICLE")

    # 2. 8-character EV plate format overrides ambiguous color detector outputs.
    if plate and NEV_PLATE_RE.match(plate):
        return ("GREEN", "NEW_ENERGY")

    type_map = {
        0: ("BLUE", "FUEL"),
        1: ("YELLOW", "FUEL"),
        2: ("GREEN", "NEW_ENERGY"),
        3: ("GREEN", "NEW_ENERGY"),
        4: ("WHITE", "FUEL"),
        "0": ("BLUE", "FUEL"),
        "1": ("YELLOW", "FUEL"),
        "2": ("GREEN", "NEW_ENERGY"),
        "3": ("GREEN", "NEW_ENERGY"),
        "4": ("WHITE", "FUEL"),
        "BLUE": ("BLUE", "FUEL"),
        "YELLOW": ("YELLOW", "FUEL"),
        "GREEN": ("GREEN", "NEW_ENERGY"),
        "WHITE": ("WHITE", "FUEL"),
    }

    if raw_type in type_map:
        return type_map[raw_type]

    if isinstance(raw_type, str) and raw_type.strip().upper() in type_map:
        return type_map[raw_type.strip().upper()]

    if color_hint:
        hint = color_hint.strip().upper()
        if hint in type_map:
            return type_map[hint]

    if plate and STANDARD_PLATE_RE.match(plate):
        return ("BLUE", "FUEL")

    return ("UNKNOWN", "UNKNOWN")


def _frame_dimensions(frame: Any) -> tuple[int, int]:
    if hasattr(frame, "shape") and len(frame.shape) >= 2:
        return int(frame.shape[0]), int(frame.shape[1])
    if isinstance(frame, (list, tuple)) and frame:
        h = len(frame)
        w = len(frame[0]) if isinstance(frame[0], (list, tuple)) else 0
        return h, w
    return 0, 0


def _slice_frame(frame: Any, x1: int, y1: int, x2: int, y2: int) -> Any:
    if hasattr(frame, "shape"):
        return frame[y1:y2, x1:x2]
    return [row[x1:x2] for row in frame[y1:y2]]


def crop_roi(frame: Any, roi_json: str | dict[str, Any] | None) -> Any:
    """Safely crop a frame according to normalized or pixel ROI JSON configuration."""
    if frame is None or not roi_json:
        return frame
    h, w = _frame_dimensions(frame)
    if h <= 1 or w <= 1:
        return frame

    try:
        roi = json.loads(roi_json) if isinstance(roi_json, str) else roi_json
    except (TypeError, ValueError):
        return frame
    if not isinstance(roi, dict):
        return frame

    if isinstance(roi.get("points"), list) and len(roi["points"]) >= 2:
        xs: list[float] = []
        ys: list[float] = []
        for pt in roi["points"]:
            if isinstance(pt, (list, tuple)) and len(pt) >= 2:
                xs.append(float(pt[0]))
                ys.append(float(pt[1]))
        if not xs or not ys:
            return frame
        x_min, x_max = min(xs), max(xs)
        y_min, y_max = min(ys), max(ys)
        normalized = 0.0 <= x_min and x_max <= 1.0 and 0.0 <= y_min and y_max <= 1.0
        px1 = int(round(x_min * w)) if normalized else int(round(x_min))
        py1 = int(round(y_min * h)) if normalized else int(round(y_min))
        px2 = int(round(x_max * w)) if normalized else int(round(x_max))
        py2 = int(round(y_max * h)) if normalized else int(round(y_max))
    elif all(k in roi for k in ("x", "y", "w", "h")):
        rx = float(roi["x"])
        ry = float(roi["y"])
        rw = float(roi["w"])
        rh = float(roi["h"])
        normalized = (
            0.0 <= rx <= 1.0
            and 0.0 <= ry <= 1.0
            and 0.0 < rw <= 1.0
            and 0.0 < rh <= 1.0
        )
        px1 = int(round(rx * w)) if normalized else int(round(rx))
        py1 = int(round(ry * h)) if normalized else int(round(ry))
        px2 = int(round((rx + rw) * w)) if normalized else int(round(rx + rw))
        py2 = int(round((ry + rh) * h)) if normalized else int(round(ry + rh))
    else:
        return frame

    px1 = max(0, min(w - 1, px1))
    py1 = max(0, min(h - 1, py1))
    px2 = max(px1 + 1, min(w, px2))
    py2 = max(py1 + 1, min(h, py2))
    if px2 <= px1 or py2 <= py1:
        return frame
    return _slice_frame(frame, px1, py1, px2, py2)


class PlateVoteBuffer:
    """Rolling confidence consensus + empty hysteresis filter to suppress OCR jitter."""

    def __init__(
        self,
        window_size: int = 5,
        min_hits: int = 2,
        min_confidence: float = 0.72,
        empty_clear_frames: int = 4,
    ) -> None:
        self.window_size = max(1, window_size)
        self.min_hits = max(1, min(min_hits, self.window_size))
        self.min_confidence = min_confidence
        self.empty_clear_frames = max(1, empty_clear_frames)
        self._history: deque[PlateDetection] = deque(maxlen=self.window_size)
        self._empty_streak = 0
        self._confirmed: VoteResult | None = None

    def reset(self) -> None:
        self._history.clear()
        self._empty_streak = 0
        self._confirmed = None

    def update(self, candidate: PlateDetection | None) -> VoteResult:
        if (
            candidate is not None
            and candidate.confidence >= self.min_confidence
            and is_valid_chinese_plate(candidate.plate)
        ):
            self._empty_streak = 0
            normalized = normalize_plate(candidate.plate)
            clean = PlateDetection(
                plate=normalized,
                plate_color=candidate.plate_color,
                vehicle_type=candidate.vehicle_type,
                confidence=candidate.confidence,
            )
            self._history.append(clean)

            counts = Counter((item.plate, item.vehicle_type) for item in self._history)
            best_key, best_hits = counts.most_common(1)[0]
            if best_hits >= self.min_hits:
                matching = [
                    item
                    for item in self._history
                    if (item.plate, item.vehicle_type) == best_key
                ]
                avg_conf = sum(item.confidence for item in matching) / len(matching)
                result = VoteResult(
                    has_vehicle=True,
                    plate=best_key[0],
                    plate_color=matching[-1].plate_color,
                    vehicle_type=best_key[1],
                    confidence=round(avg_conf, 4),
                    confirmed=True,
                )
                self._confirmed = result
                return result

            if self._confirmed is not None:
                return self._confirmed
            return VoteResult(
                has_vehicle=True,
                plate=None,
                plate_color="UNKNOWN",
                vehicle_type="UNKNOWN",
                confidence=round(clean.confidence, 4),
                confirmed=False,
            )

        self._empty_streak += 1
        if self._empty_streak >= self.empty_clear_frames:
            self._history.clear()
            self._confirmed = None
            return VoteResult(
                has_vehicle=False,
                plate=None,
                plate_color="UNKNOWN",
                vehicle_type="UNKNOWN",
                confidence=0.0,
                confirmed=True,
            )

        if self._confirmed is not None:
            return self._confirmed

        return VoteResult(
            has_vehicle=False,
            plate=None,
            plate_color="UNKNOWN",
            vehicle_type="UNKNOWN",
            confidence=0.0,
            confirmed=False,
        )


class HyperLpr3Recognizer:
    """Lazy-initialized HyperLPR3 recognizer selecting best validated plate candidate."""

    def __init__(self, min_confidence: float = 0.72) -> None:
        self.min_confidence = min_confidence
        self._catcher: Any = None

    def _ensure_catcher(self) -> Any:
        if self._catcher is None:
            import hyperlpr3 as lpr3  # noqa: PLC0415

            self._catcher = lpr3.LicensePlateCatcher(detect_level=lpr3.DETECT_LEVEL_LOW)
        return self._catcher

    def recognize(self, frame: Any) -> PlateDetection | None:
        catcher = self._ensure_catcher()
        raw_results = catcher(frame) or []
        best: PlateDetection | None = None

        for item in raw_results:
            if isinstance(item, dict):
                raw_code = item.get("code") or item.get("plate")
                conf = float(item.get("confidence", 0.0))
                raw_type = item.get("type")
            elif isinstance(item, (list, tuple)) and len(item) >= 3:
                raw_code = item[0]
                conf = float(item[1])
                raw_type = item[2]
            else:
                continue

            if conf < self.min_confidence or not is_valid_chinese_plate(raw_code):
                continue
            plate = normalize_plate(raw_code)
            plate_color, vehicle_type = classify_plate(plate, raw_type)
            if best is None or conf > best.confidence:
                best = PlateDetection(
                    plate=plate,
                    plate_color=plate_color,
                    vehicle_type=vehicle_type,
                    confidence=round(conf, 4),
                )
        return best
