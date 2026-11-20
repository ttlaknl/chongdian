"""Unified AI Perception Engine powered by OpenVINO (Zero-PyTorch Runtime).

Features:
- Pure OpenVINO / ONNX runtime for YOLOv8 object detection, YOLO-Pose plate detection, and CRNN OCR
- Intel iGPU / CPU acceleration via OVModel with automatic fallback
- 1-to-2 Ground Band (轮胎接地带) space assignment preventing adjacent space bleed
- Strict vehicle-first cascade gating: no plate detection without confirmed vehicle in space
- Geometry aspect ratio checks, dual confidence gating, and top OSD exclusion
- Strict Chinese license plate regex validation (Civilian 7-char, NEV 8-char, Military, WJ)
- Mock backend support (INFERENCE_BACKEND=mock) for lightweight CI and unit testing
"""

from __future__ import annotations

import logging
import os
import re
from dataclasses import dataclass
from typing import Any, TYPE_CHECKING

import cv2
import numpy as np

from backends.openvino_backend import (
    OVModel,
    OpenVinoYoloDetector,
    OpenVinoPlateDetector,
    OpenVinoPlateRecognizer,
)
from geometry import (
    assign_vehicle_to_space,
    compute_ground_band,
    is_in_osd_mask,
    order_plate_corners,
    parse_spaces_from_roi_json,
    split_and_merge_double_plate,
    warp_plate_perspective,
)
from plate_postprocess import (
    assess_plate_illumination,
    enhance_backlit_plate,
    is_valid_chinese_plate,
    is_valid_plate_geometry,
    normalize_plate_text,
    STANDARD_PLATE_RE,
    NEV_PLATE_RE,
    MILITARY_PLATE_RE,
    WJ_PLATE_RE,
)
from plate_recognizer import (
    extract_plate_patch,
    parse_plate_detection_output,
)

if TYPE_CHECKING:
    from config import VisionConfig

logger = logging.getLogger("vision.ai_engine")

# ---------------------------------------------------------------------------
# COCO 80 class IDs
# ---------------------------------------------------------------------------
COCO_BICYCLE = 1
COCO_CAR = 2
COCO_MOTORCYCLE = 3
COCO_BUS = 5
COCO_TRUCK = 7
COCO_BENCH = 13
COCO_BACKPACK = 24
COCO_SUITCASE = 28
COCO_CHAIR = 56
COCO_COUCH = 57
COCO_TABLE = 60

TARGET_CLASSES = {
    COCO_CAR: ("car", "CAR"),
    COCO_BUS: ("bus", "HEAVY_VEHICLE"),
    COCO_TRUCK: ("truck", "HEAVY_VEHICLE"),
    COCO_BICYCLE: ("bicycle", "NON_MOTOR"),
    COCO_MOTORCYCLE: ("motorcycle", "NON_MOTOR"),
    COCO_CHAIR: ("chair", "OBSTACLE"),
    COCO_BENCH: ("bench", "OBSTACLE"),
    COCO_TABLE: ("dining table", "OBSTACLE"),
    COCO_COUCH: ("couch", "OBSTACLE"),
    COCO_SUITCASE: ("suitcase", "OBSTACLE"),
    COCO_BACKPACK: ("backpack", "OBSTACLE"),
}

VEHICLE_CLASSES = {COCO_CAR, COCO_BUS, COCO_TRUCK}


def four_point_transform(image: np.ndarray, pts: np.ndarray) -> np.ndarray:
    """Warp quadrilateral plate region to flat rectangle using 4 corner keypoints."""
    rect = np.asarray(pts, dtype=np.float32)
    tl, tr, br, bl = rect

    width_a = np.sqrt(((br[0] - bl[0]) ** 2) + ((br[1] - bl[1]) ** 2))
    width_b = np.sqrt(((tr[0] - tl[0]) ** 2) + ((tr[1] - tl[1]) ** 2))
    max_width = max(int(width_a), int(width_b))

    height_a = np.sqrt(((tr[0] - br[0]) ** 2) + ((tr[1] - br[1]) ** 2))
    height_b = np.sqrt(((tl[0] - bl[0]) ** 2) + ((tl[1] - bl[1]) ** 2))
    max_height = max(int(height_a), int(height_b))

    if max_width < 10 or max_height < 5:
        return image

    dst = np.array(
        [
            [0, 0],
            [max_width - 1, 0],
            [max_width - 1, max_height - 1],
            [0, max_height - 1],
        ],
        dtype=np.float32,
    )
    matrix = cv2.getPerspectiveTransform(rect, dst)
    return cv2.warpPerspective(image, matrix, (max_width, max_height))


def split_and_merge_double_plate(img: np.ndarray) -> np.ndarray:
    """Split double-layer plate horizontally and merge side-by-side."""
    h, w = img.shape[:2]
    img_upper = img[0 : int(5 / 12 * h), :]
    img_lower = img[int(1 / 3 * h) :, :]
    img_upper = cv2.resize(img_upper, (img_lower.shape[1], img_lower.shape[0]))
    return np.hstack((img_upper, img_lower))


def normalize_plate_color(color_name: str | None) -> str:
    """Standardize plate color string into GREEN, BLUE, YELLOW, WHITE, BLACK, UNKNOWN."""
    if not color_name:
        return "UNKNOWN"
    c = str(color_name).strip()
    if "绿" in c or "GREEN" in c.upper():
        return "GREEN"
    if "蓝" in c or "BLUE" in c.upper():
        return "BLUE"
    if "黄" in c or "YELLOW" in c.upper():
        return "YELLOW"
    if "白" in c or "WHITE" in c.upper():
        return "WHITE"
    if "黑" in c or "BLACK" in c.upper():
        return "BLACK"
    return "UNKNOWN"


def clean_plate_text(raw: str | None) -> str:
    return normalize_plate_text(raw)


def classify_plate_attributes(plate: str, color_hint: str) -> tuple[str, str, str | None]:
    """Classify plate into (plate_color, vehicle_type, violation_type)."""
    p = clean_plate_text(plate)
    color = normalize_plate_color(color_hint)

    # 1. 军车与武警判定
    if MILITARY_PLATE_RE.match(p) or WJ_PLATE_RE.match(p) or "警" in p:
        return "WHITE", "SPECIAL_VEHICLE", None

    # 2. 新能源绿牌判定
    if color == "GREEN" or NEV_PLATE_RE.match(p):
        return "GREEN", "NEW_ENERGY", None

    # 3. 燃油车蓝牌 / 黄牌判定
    if color == "BLUE" or STANDARD_PLATE_RE.match(p) or len(p) == 7:
        return "BLUE", "FUEL", "FUEL_VEHICLE"

    if color == "YELLOW":
        return "YELLOW", "FUEL", "FUEL_VEHICLE"

    return color, "UNKNOWN", None


def _compute_ioa(plate_box: list[int], vehicle_box: list[int]) -> float:
    """Compute Intersection-over-Area of plate_box inside vehicle_box."""
    px1, py1, px2, py2 = plate_box
    vx1, vy1, vx2, vy2 = vehicle_box

    ix1 = max(px1, vx1)
    iy1 = max(py1, vy1)
    ix2 = min(px2, vx2)
    iy2 = min(py2, vy2)

    if ix2 <= ix1 or iy2 <= iy1:
        return 0.0

    inter_area = (ix2 - ix1) * (iy2 - iy1)
    plate_area = max(1, (px2 - px1) * (py2 - py1))
    return inter_area / plate_area


def _pad_bbox(bbox: list[int], pad_ratio: float, img_w: int, img_h: int) -> list[int]:
    """Expand bounding box by pad_ratio on all sides, clamped to image bounds."""
    x1, y1, x2, y2 = bbox
    bw = x2 - x1
    bh = y2 - y1
    dx = int(bw * pad_ratio)
    dy = int(bh * pad_ratio)
    return [
        max(0, x1 - dx),
        max(0, y1 - dy),
        min(img_w, x2 + dx),
        min(img_h, y2 + dy),
    ]


def _resolve_model_path(path: str) -> str:
    """Find corresponding .xml or .onnx file if path points to .pt/.pth or doesn't exist directly."""
    if os.path.exists(path) and (path.endswith(".xml") or path.endswith(".onnx")):
        return path

    base, _ = os.path.splitext(path)
    for candidate in [f"{base}.xml", f"{base}.onnx", path]:
        if os.path.exists(candidate):
            return candidate

    return path


@dataclass
class SpacePerception:
    space_id: str
    has_vehicle: bool
    plate: str | None
    plate_color: str
    vehicle_type: str
    violation_type: str | None
    confidence: float
    confirmed: bool = False
    evidence_frame: np.ndarray | None = None
    bbox: list[int] | None = None
    category_label: str = ""


class UnifiedAiEngine:
    """OpenVINO Unified AI Engine for vehicle detection, spatial assignment, and plate recognition."""

    def __init__(
        self,
        yolo_weights: str = "weights/yolov8n.xml",
        plate_detect_weights: str = "weights/yolo26s-plate-detect.xml",
        plate_rec_weights: str = "weights/plate_rec_color.xml",
        device: str | None = None,
        config: "VisionConfig | None" = None,
    ) -> None:
        if config is None:
            from config import VisionConfig
            config = VisionConfig.from_env()
        self.config = config

        self.device = device or self.config.ov_device
        self.is_mock = self.config.inference_backend.lower() == "mock"

        self.yolo_model: OVModel | None = None
        self.plate_detector_model: OVModel | None = None
        self.plate_rec_model: OVModel | None = None

        self.yolo_detector: OpenVinoYoloDetector | None = None
        self.plate_detector: OpenVinoPlateDetector | None = None
        self.plate_recognizer: OpenVinoPlateRecognizer | None = None
        self.hyperlpr_catcher: Any = None
        self._init_success = False

        self._load_models(yolo_weights, plate_detect_weights, plate_rec_weights)

    def _load_models(
        self,
        yolo_weights: str,
        plate_detect_weights: str,
        plate_rec_weights: str,
    ) -> None:
        try:
            resolved_yolo = _resolve_model_path(yolo_weights)
            resolved_plate_det = _resolve_model_path(plate_detect_weights)
            resolved_plate_rec = _resolve_model_path(plate_rec_weights)

            logger.info("loading OpenVINO models device=%s backend=%s", self.device, self.config.inference_backend)

            self.yolo_model = OVModel(
                resolved_yolo,
                device=self.device,
                num_streams=self.config.ov_num_streams,
                is_mock=self.is_mock,
            )
            self.yolo_detector = OpenVinoYoloDetector(self.yolo_model, input_size=640)

            self.plate_detector_model = OVModel(
                resolved_plate_det,
                device=self.device,
                num_streams=self.config.ov_num_streams,
                is_mock=self.is_mock,
            )
            self.plate_detector = OpenVinoPlateDetector(self.plate_detector_model, input_size=640)

            self.plate_rec_model = OVModel(
                resolved_plate_rec,
                device=self.device,
                num_streams=self.config.ov_num_streams,
                is_mock=self.is_mock,
            )
            self.plate_recognizer = OpenVinoPlateRecognizer(self.plate_rec_model, height=48, width=168)

            # Optional HyperLPR3 fallback
            if not self.is_mock:
                try:
                    import hyperlpr3 as lpr3
                    self.hyperlpr_catcher = lpr3.LicensePlateCatcher(detect_level=lpr3.DETECT_LEVEL_LOW)
                    logger.info("HyperLPR3 optional fallback loaded successfully")
                except Exception as exc:
                    logger.debug("HyperLPR3 fallback not loaded: %s", exc)
                    self.hyperlpr_catcher = None

            self._init_success = True
            logger.info("UnifiedAiEngine (OpenVINO) initialized successfully on device=%s", self.device)
        except Exception as exc:
            logger.error("failed to initialize OpenVINO AI models: %s", exc, exc_info=True)
            self._init_success = False

    def is_ready(self) -> bool:
        return self._init_success and (self.is_mock or (self.yolo_model is not None and self.yolo_model.is_ready))

    def detect_objects(self, frame: np.ndarray, conf: float | None = None) -> list[dict[str, Any]]:
        """Run YOLO object detection through OpenVINO."""
        if frame is None or frame.size == 0:
            return []

        detector = getattr(self, "yolo_detector", None)
        if detector is None:
            yolo_m = getattr(self, "yolo_model", None)
            if yolo_m is not None:
                if isinstance(yolo_m, OVModel):
                    self.yolo_detector = OpenVinoYoloDetector(yolo_m)
                    detector = self.yolo_detector
                elif callable(yolo_m):
                    effective_conf = conf if conf is not None else 0.25
                    results = yolo_m(frame, conf=effective_conf)
                    detected = []
                    if results:
                        boxes = getattr(results[0], "boxes", None)
                        if boxes is not None:
                            try:
                                for b in boxes:
                                    pass
                            except TypeError:
                                pass
                    return detected
            if detector is None:
                return []

        effective_conf = conf if conf is not None else 0.25
        return detector.detect(
            frame,
            conf_threshold=effective_conf,
            target_classes=TARGET_CLASSES,
            vehicle_conf_threshold=self.config.vehicle_conf_threshold,
            vehicle_min_size_px=self.config.vehicle_min_size_px,
        )

    def detect_and_recognize_plates(
        self,
        img_crop: np.ndarray,
        conf: float | None = None,
        crop_origin: tuple[int, int] = (0, 0),
        frame_height: int = 0,
    ) -> list[dict[str, Any]]:
        """Run plate detection + 4-point rectification + OCR through OpenVINO."""
        if getattr(self, "plate_detector", None) is None and getattr(self, "plate_detector_model", None) is not None:
            self.plate_detector = OpenVinoPlateDetector(self.plate_detector_model)
        if getattr(self, "plate_recognizer", None) is None and getattr(self, "plate_rec_model", None) is not None:
            self.plate_recognizer = OpenVinoPlateRecognizer(self.plate_rec_model)

        if img_crop is None or img_crop.size == 0 or getattr(self, "plate_detector", None) is None:
            return []

        cfg = self.config
        det_conf = conf if conf is not None else cfg.plate_det_conf_threshold
        ox, oy = crop_origin
        plates: list[dict[str, Any]] = []

        try:
            raw_plates = self.plate_detector.detect(
                img_crop,
                conf_threshold=det_conf,
                has_keypoints_mode=cfg.plate_det_has_keypoints,
            )
            for raw_item in raw_plates:
                parsed = parse_plate_detection_output(raw_item, has_keypoints_mode=cfg.plate_det_has_keypoints)
                box = parsed["bbox"]
                det_conf_val = parsed["confidence"]
                is_double = parsed["is_double_layer"]
                landmarks = parsed["landmarks"]

                bw = max(1, box[2] - box[0])
                bh = max(1, box[3] - box[1])

                # Global coordinates
                gx1, gy1 = box[0] + ox, box[1] + oy
                gx2, gy2 = box[2] + ox, box[3] + oy
                global_box = [gx1, gy1, gx2, gy2]

                # 1. OSD 顶部屏蔽检查: 车牌中心点在 OSD 遮罩区直接丢弃
                if frame_height > 0 and is_in_osd_mask(global_box, frame_height, cfg.osd_mask_top_ratio):
                    logger.debug("plate rejected: center inside top OSD mask %s", global_box)
                    continue

                # 2. 几何约束校验 (is_valid_plate_geometry)
                if not is_valid_plate_geometry(
                    box,
                    is_double_layer=is_double,
                    min_width=cfg.plate_min_width_px,
                    min_height=cfg.plate_min_height_px,
                    single_ar_range=(cfg.plate_single_ar_min, cfg.plate_single_ar_max),
                    double_ar_range=(cfg.plate_double_ar_min, cfg.plate_double_ar_max),
                ):
                    logger.debug("plate rejected: geometry mismatch box=%s is_double=%s", box, is_double)
                    continue

                # 3. 4 角点透视拉正或 4% 安全外扩传统水平框回退
                warped, is_warped = extract_plate_patch(
                    img_crop,
                    parsed,
                    safe_pad_ratio=0.04,
                    target_single=(168, 48),
                    target_double=(168, 96),
                )

                # 4. 光照质量与逆光暗光评估
                mean_luma, std_luma, lap_var, needs_enhance = assess_plate_illumination(
                    warped,
                    low_luma_thresh=cfg.plate_low_luma_threshold,
                    low_contrast_thresh=cfg.plate_low_contrast_threshold,
                )

                # 5. OpenVINO OCR (原图路径)
                plate_number, raw_color, ocr_avg_conf, ocr_min_conf = self.plate_recognizer.recognize(warped)
                used_enhanced = False

                # 6. 双路自适应增强择优
                if needs_enhance and cfg.enable_adaptive_enhance:
                    enhanced_crop = enhance_backlit_plate(
                        warped,
                        clip_limit=cfg.plate_clahe_clip_limit,
                        gamma=cfg.plate_enhance_gamma,
                    )
                    p_enh, r_col_enh, avg_conf_enh, min_conf_enh = self.plate_recognizer.recognize(enhanced_crop)

                    clean_orig = clean_plate_text(plate_number)
                    clean_enh = clean_plate_text(p_enh)
                    v_orig = is_valid_chinese_plate(clean_orig)
                    v_enh = is_valid_chinese_plate(clean_enh)

                    pick_enhanced = False
                    if v_enh and not v_orig:
                        pick_enhanced = True
                    elif v_enh and v_orig:
                        if avg_conf_enh > ocr_avg_conf:
                            pick_enhanced = True
                    elif not v_orig and not v_enh:
                        if avg_conf_enh > ocr_avg_conf:
                            pick_enhanced = True

                    if pick_enhanced:
                        plate_number = p_enh
                        ocr_avg_conf = avg_conf_enh
                        ocr_min_conf = min_conf_enh
                        used_enhanced = True

                # 7. OCR 置信度双门槛校验
                if ocr_avg_conf < cfg.plate_ocr_conf_threshold or ocr_min_conf < cfg.plate_ocr_min_char_conf:
                    logger.debug(
                        "plate rejected: OCR conf (avg=%.3f, min=%.3f) below thresholds (%.2f, %.2f) (text=%s)",
                        ocr_avg_conf, ocr_min_conf, cfg.plate_ocr_conf_threshold, cfg.plate_ocr_min_char_conf, plate_number,
                    )
                    continue

                plate_text = clean_plate_text(plate_number)

                # 8. 正则校验 (is_valid_chinese_plate)
                if not is_valid_chinese_plate(plate_text):
                    logger.debug("plate rejected: regex failed for '%s'", plate_text)
                    continue

                # 9. 颜色与属性分类 (以原图颜色为主，结合号牌位数做绿牌/警牌强制校正)
                color, v_type, viol_type = classify_plate_attributes(plate_text, raw_color)

                global_landmarks = (
                    [[int(lx + ox), int(ly + oy)] for lx, ly in landmarks]
                    if landmarks is not None
                    else None
                )

                plates.append(
                    {
                        "plate": plate_text,
                        "plate_color": color,
                        "raw_color": raw_color,
                        "vehicle_type": v_type,
                        "violation_type": viol_type,
                        "confidence": det_conf_val,
                        "ocr_confidence": ocr_avg_conf,
                        "ocr_min_char_conf": ocr_min_conf,
                        "bbox": global_box,
                        "landmarks": global_landmarks,
                        "laplacian_var": round(lap_var, 2),
                        "plate_area": int(bw * bh),
                        "warped": is_warped,
                        "enhanced": used_enhanced,
                        "mean_luma": round(mean_luma, 2),
                        "std_luma": round(std_luma, 2),
                    }
                )
        except Exception as exc:
            logger.warning("OpenVINO plate detection error: %s", exc)

        return plates

    def _detect_plates_in_vehicle(
        self,
        frame: np.ndarray,
        vehicle_bbox: list[int],
    ) -> list[dict[str, Any]]:
        """Run plate detection scoped to a single vehicle bounding box (with 6% padding)."""
        h, w = frame.shape[:2]
        padded = _pad_bbox(vehicle_bbox, self.config.vehicle_bbox_pad_ratio, w, h)
        px1, py1, px2, py2 = padded
        crop = frame[py1:py2, px1:px2]
        if crop.size == 0:
            return []

        return self.detect_and_recognize_plates(
            crop,
            crop_origin=(px1, py1),
            frame_height=h,
        )

    def _try_hyperlpr_in_vehicle(
        self,
        frame: np.ndarray,
        vehicle_bbox: list[int],
    ) -> list[dict[str, Any]]:
        """HyperLPR3 fallback, strictly scoped to vehicle bbox."""
        if self.hyperlpr_catcher is None:
            return []

        h, w = frame.shape[:2]
        padded = _pad_bbox(vehicle_bbox, self.config.vehicle_bbox_pad_ratio, w, h)
        px1, py1, px2, py2 = padded
        crop = frame[py1:py2, px1:px2]
        if crop.size == 0:
            return []

        plates: list[dict[str, Any]] = []
        try:
            raw_results = self.hyperlpr_catcher(crop) or []
            for item in raw_results:
                if isinstance(item, dict):
                    raw_code = item.get("code") or item.get("plate")
                    c_score = float(item.get("confidence", 0.0))
                    color_hint = item.get("type") or item.get("color")
                elif isinstance(item, (list, tuple)) and len(item) >= 2:
                    raw_code = item[0]
                    c_score = float(item[1])
                    color_hint = item[2] if len(item) > 2 else ""
                else:
                    continue

                p_text = clean_plate_text(raw_code)
                if not is_valid_chinese_plate(p_text):
                    continue

                if c_score < self.config.plate_ocr_conf_threshold:
                    continue

                if is_in_osd_mask([px1, py1, px2, py2], h, self.config.osd_mask_top_ratio):
                    continue

                color, v_type, viol_type = classify_plate_attributes(p_text, str(color_hint))
                plates.append(
                    {
                        "plate": p_text,
                        "plate_color": color,
                        "raw_color": str(color_hint),
                        "vehicle_type": v_type,
                        "violation_type": viol_type,
                        "confidence": c_score,
                        "ocr_confidence": c_score,
                        "ocr_min_char_conf": c_score,
                        "bbox": [px1, py1, px2, py2],
                        "landmarks": [],
                    }
                )
        except Exception as exc:
            logger.debug("hyperlpr3 fallback error: %s", exc)

        return plates

    def evaluate_space(
        self,
        frame: np.ndarray,
        space_id: str,
        roi_json: str | None = None,
    ) -> SpacePerception:
        """Evaluate space occupancy, vehicle/obstacle type, and license plate."""
        if frame is None:
            return SpacePerception(
                space_id=space_id,
                has_vehicle=False,
                plate=None,
                plate_color="EMPTY",
                vehicle_type="UNKNOWN",
                violation_type=None,
                confidence=0.0,
                category_label="无视频帧",
            )

        h, w = frame.shape[:2]
        cfg = self.config

        # 1. 解析车位多边形
        spaces_polygons = parse_spaces_from_roi_json(roi_json, space_id, w, h)
        target_space_polygon = spaces_polygons.get(space_id)
        if target_space_polygon is None and spaces_polygons:
            target_space_polygon = next(iter(spaces_polygons.values()))

        # 2. 执行 YOLO 全类目检测
        detected_objects = self.detect_objects(frame, conf=0.25)

        # 3. 1 拖 2 接地带归属算法分配机动车到目标车位
        space_cars: list[dict[str, Any]] = []
        space_heavies: list[dict[str, Any]] = []
        space_non_motors: list[dict[str, Any]] = []
        space_obstacles: list[dict[str, Any]] = []

        for obj in detected_objects:
            cat_type = obj["category_type"]
            bbox = obj["bbox"]

            if cat_type in {"CAR", "HEAVY_VEHICLE"}:
                assigned_space = assign_vehicle_to_space(
                    bbox,
                    spaces_polygons,
                    ground_band_ratio=cfg.ground_band_ratio,
                    min_ioa=cfg.ground_min_ioa,
                )
                if assigned_space == space_id:
                    if cat_type == "CAR":
                        space_cars.append(obj)
                    else:
                        space_heavies.append(obj)
            else:
                center = obj["center"]
                if target_space_polygon is not None and cv2.pointPolygonTest(target_space_polygon, (float(center[0]), float(center[1])), False) >= 0:
                    if cat_type == "NON_MOTOR":
                        space_non_motors.append(obj)
                    elif cat_type == "OBSTACLE":
                        space_obstacles.append(obj)

        all_vehicle_objs = space_cars + space_heavies

        # 4. 车辆门控硬约束：无车绝不检牌！
        space_plates: list[dict[str, Any]] = []
        plate_to_vehicle: dict[int, dict[str, Any]] = {}

        if all_vehicle_objs:
            for veh in all_vehicle_objs:
                veh_plates = self._detect_plates_in_vehicle(frame, veh["bbox"])
                if not veh_plates:
                    veh_plates = self._try_hyperlpr_in_vehicle(frame, veh["bbox"])

                for vp in veh_plates:
                    ioa = _compute_ioa(vp["bbox"], veh["bbox"])
                    if ioa < cfg.plate_ioa_threshold:
                        logger.debug(
                            "plate '%s' rejected: IoA=%.2f < %.2f vs vehicle %s",
                            vp["plate"], ioa, cfg.plate_ioa_threshold, veh["bbox"],
                        )
                        continue
                    idx = len(space_plates)
                    space_plates.append(vp)
                    plate_to_vehicle[idx] = veh

        # 5. 研判决策分发与画框生成
        if space_plates:
            best_idx = 0
            best_plate = space_plates[best_idx]
            matched_vehicle = plate_to_vehicle.get(best_idx)
            target_bbox = matched_vehicle["bbox"] if matched_vehicle else best_plate["bbox"]

            ev_frame = frame.copy()
            if cfg.debug_draw_boxes:
                self._draw_debug_annotations(ev_frame, spaces_polygons, all_vehicle_objs, space_plates)
            else:
                bx1, by1, bx2, by2 = target_bbox
                color_bgr = (0, 255, 0) if best_plate["vehicle_type"] == "NEW_ENERGY" else (255, 120, 0)
                cv2.rectangle(ev_frame, (bx1, by1), (bx2, by2), color_bgr, 2)
                px1, py1, px2, py2 = best_plate["bbox"]
                cv2.rectangle(ev_frame, (px1, py1), (px2, py2), (0, 255, 255), 2)

            return SpacePerception(
                space_id=space_id,
                has_vehicle=True,
                plate=best_plate["plate"],
                plate_color=best_plate["plate_color"],
                vehicle_type=best_plate["vehicle_type"],
                violation_type=best_plate["violation_type"],
                confidence=best_plate["confidence"],
                confirmed=True,
                evidence_frame=ev_frame,
                bbox=target_bbox,
                category_label=f"车辆入位 ({best_plate['plate']})",
            )

        if space_cars:
            target_car = space_cars[0]
            ev_frame = frame.copy()
            if cfg.debug_draw_boxes:
                self._draw_debug_annotations(ev_frame, spaces_polygons, all_vehicle_objs, [])
            else:
                bx1, by1, bx2, by2 = target_car["bbox"]
                cv2.rectangle(ev_frame, (bx1, by1), (bx2, by2), (0, 200, 255), 2)
            return SpacePerception(
                space_id=space_id,
                has_vehicle=True,
                plate=None,
                plate_color="UNKNOWN",
                vehicle_type="UNKNOWN",
                violation_type=None,
                confidence=target_car["confidence"],
                confirmed=False,
                evidence_frame=ev_frame,
                bbox=target_car["bbox"],
                category_label="车辆在位 (车牌未识别/遮挡)",
            )

        if space_heavies:
            target_heavy = space_heavies[0]
            ev_frame = frame.copy()
            bx1, by1, bx2, by2 = target_heavy["bbox"]
            cv2.rectangle(ev_frame, (bx1, by1), (bx2, by2), (0, 165, 255), 2)
            return SpacePerception(
                space_id=space_id,
                has_vehicle=True,
                plate=None,
                plate_color="YELLOW",
                vehicle_type="HEAVY_VEHICLE",
                violation_type="HEAVY_VEHICLE",
                confidence=target_heavy["confidence"],
                confirmed=True,
                evidence_frame=ev_frame,
                bbox=target_heavy["bbox"],
                category_label=f"大客货车违规占用 ({target_heavy['class_name']})",
            )

        if space_non_motors:
            target_nm = space_non_motors[0]
            ev_frame = frame.copy()
            bx1, by1, bx2, by2 = target_nm["bbox"]
            cv2.rectangle(ev_frame, (bx1, by1), (bx2, by2), (255, 0, 255), 2)
            return SpacePerception(
                space_id=space_id,
                has_vehicle=True,
                plate=None,
                plate_color="TWO_WHEELER",
                vehicle_type="NON_MOTOR",
                violation_type="NON_MOTOR_VEHICLE",
                confidence=target_nm["confidence"],
                confirmed=True,
                evidence_frame=ev_frame,
                bbox=target_nm["bbox"],
                category_label=f"两轮车非法占用 ({target_nm['class_name']})",
            )

        if space_obstacles:
            target_obs = space_obstacles[0]
            ev_frame = frame.copy()
            bx1, by1, bx2, by2 = target_obs["bbox"]
            cv2.rectangle(ev_frame, (bx1, by1), (bx2, by2), (0, 0, 255), 2)
            return SpacePerception(
                space_id=space_id,
                has_vehicle=True,
                plate=None,
                plate_color="OBSTACLE",
                vehicle_type="OBSTACLE",
                violation_type="OBSTACLE_OCCUPATION",
                confidence=target_obs["confidence"],
                confirmed=True,
                evidence_frame=ev_frame,
                bbox=target_obs["bbox"],
                category_label=f"桌椅杂物非法霸位 ({target_obs['class_name']})",
            )

        return SpacePerception(
            space_id=space_id,
            has_vehicle=False,
            plate=None,
            plate_color="EMPTY",
            vehicle_type="UNKNOWN",
            violation_type=None,
            confidence=1.0,
            category_label="空闲待机",
        )

    def _draw_debug_annotations(
        self,
        frame: np.ndarray,
        spaces_polygons: dict[str, np.ndarray],
        vehicles: list[dict[str, Any]],
        plates: list[dict[str, Any]],
    ) -> None:
        """Draw debug annotations: spaces (yellow), vehicle & ground band (blue), plates (green)."""
        for sp_id, poly in spaces_polygons.items():
            if poly is not None and len(poly) >= 3:
                cv2.polylines(frame, [poly], isClosed=True, color=(0, 255, 255), thickness=2)
                cx = int(np.mean(poly[:, 0]))
                cy = int(np.mean(poly[:, 1]))
                cv2.putText(frame, sp_id, (cx, cy), cv2.FONT_HERSHEY_SIMPLEX, 0.6, (0, 255, 255), 2)

        for v in vehicles:
            x1, y1, x2, y2 = v["bbox"]
            cv2.rectangle(frame, (x1, y1), (x2, y2), (255, 0, 0), 2)
            gbox = compute_ground_band(v["bbox"], self.config.ground_band_ratio)
            gx1, gy1, gx2, gy2 = gbox
            cv2.rectangle(frame, (gx1, gy1), (gx2, gy2), (255, 120, 0), 1)
            label = f"{v['class_name']} conf={v['confidence']:.2f}"
            cv2.putText(frame, label, (x1, max(y1 - 5, 12)), cv2.FONT_HERSHEY_SIMPLEX, 0.45, (255, 0, 0), 1)

        for p in plates:
            bx1, by1, bx2, by2 = p["bbox"]
            cv2.rectangle(frame, (bx1, by1), (bx2, by2), (0, 255, 0), 2)
            plate_label = f"{p['plate']} det={p['confidence']:.2f} ocr={p.get('ocr_confidence', 0):.2f}"
            cv2.putText(frame, plate_label, (bx1, max(by1 - 5, 12)), cv2.FONT_HERSHEY_SIMPLEX, 0.45, (0, 255, 0), 1)

    def _parse_space_polygon(self, roi_json: str | None, width: int, height: int) -> tuple[bool, np.ndarray]:
        """Backward-compatible helper to parse polygon points."""
        spaces = parse_spaces_from_roi_json(roi_json, "default", width, height)
        if spaces:
            poly = next(iter(spaces.values()))
            has_custom = not (poly[0, 0] == 0 and poly[0, 1] == 0 and poly[2, 0] == width and poly[2, 1] == height)
            return has_custom, poly
        return False, np.array([[0, 0], [width, 0], [width, height], [0, height]], dtype=np.int32)
