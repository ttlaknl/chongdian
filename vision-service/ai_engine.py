"""YOLO26 + Plate Recognition Unified AI Perception Engine.

Features:
- Ultralytics YOLO Object Detection: Native zero-training detection for:
  * Vehicles: car, bus, truck
  * Two-wheelers: bicycle, motorcycle (Non-motor violation)
  * Obstacles: chair, bench, dining table, couch, suitcase (Obstacle violation)
- yolo26-plate: YOLO Pose (4-keypoint) + CRNN/LPRNet character & color recognition:
  * Blue, Green (NEV), Yellow, White (Police/Armed Police WJ/Military), Double-row
- Parking Space Polygon Containment:
  * cv2.pointPolygonTest for robust spatial binding between vehicles, obstacles, and parking slots
"""

from __future__ import annotations

import json
import logging
import os
import re
from dataclasses import dataclass, field
from typing import Any

import cv2
import numpy as np

logger = logging.getLogger("vision.ai_engine")

PROVINCE_CHARS = "京津沪渝冀豫云辽黑湘皖鲁新苏浙赣鄂桂甘晋蒙陕吉闽贵粤青藏川宁琼"
STANDARD_PLATE_RE = re.compile(
    rf"^[{PROVINCE_CHARS}][A-Z][A-HJ-NP-Z0-9]{{4}}[A-HJ-NP-Z0-9挂学警港澳]$"
)
NEV_PLATE_RE = re.compile(
    rf"^[{PROVINCE_CHARS}][A-Z](?:[DF][A-HJ-NP-Z0-9]{{5}}|[A-HJ-NP-Z0-9]{{5}}[DF])$"
)
MILITARY_PLATE_RE = re.compile(r"^[VKHBSLJNGCZ][A-Z][0-9]{5}$")
WJ_PLATE_RE = re.compile(r"^WJ[0-9A-Z]{5,6}$")

# COCO 80 class IDs
COCO_CAR = 2
COCO_MOTORCYCLE = 3
COCO_BUS = 5
COCO_TRUCK = 7
COCO_BICYCLE = 1
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


def four_point_transform(image: np.ndarray, pts: np.ndarray) -> np.ndarray:
    """Warp quadrilateral plate region to flat rectangle using 4 corner keypoints."""
    rect = pts.astype(np.float32)
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
    if not raw:
        return ""
    return re.sub(r"[\s·\-•._]+", "", raw.strip()).upper()


def classify_plate_attributes(plate: str, color_hint: str) -> tuple[str, str, str | None]:
    """Classify plate into (plate_color, vehicle_type, violation_type)."""
    p = clean_plate_text(plate)
    color = normalize_plate_color(color_hint)

    # 1. 军车与武警判定
    if color == "WHITE" or color == "UNKNOWN":
        if MILITARY_PLATE_RE.match(p) or WJ_PLATE_RE.match(p) or "警" in p:
            return "WHITE", "SPECIAL_VEHICLE", None

    # 2. 新能源绿牌判定
    if color == "GREEN" or NEV_PLATE_RE.match(p):
        return "GREEN", "NEW_ENERGY", None

    # 3. 燃油车/黄牌判定
    if color == "BLUE" or STANDARD_PLATE_RE.match(p):
        return "BLUE", "FUEL", "FUEL_VEHICLE"

    if color == "YELLOW":
        return "YELLOW", "FUEL", "FUEL_VEHICLE"

    # 默认兜底
    return color, "UNKNOWN", None


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
    """Manages Ultralytics YOLO object detector and YOLO-Pose license plate recognizer."""

    def __init__(
        self,
        yolo_weights: str = "weights/yolov8n.pt",
        plate_detect_weights: str = "weights/yolo26s-plate-detect.pt",
        plate_rec_weights: str = "weights/plate_rec_color.pth",
        device: str = "cpu",
    ) -> None:
        self.device = device
        self.yolo_model: Any = None
        self.plate_detector: Any = None
        self.plate_rec_model: Any = None
        self._init_success = False

        self._load_models(yolo_weights, plate_detect_weights, plate_rec_weights)

    def _load_models(
        self,
        yolo_weights: str,
        plate_detect_weights: str,
        plate_rec_weights: str,
    ) -> None:
        try:
            import torch
            from ultralytics import YOLO
            from plate_recognition.plate_rec import init_model

            dev = torch.device(self.device)

            if os.path.exists(yolo_weights):
                logger.info("loading YOLO object detector from %s", yolo_weights)
                self.yolo_model = YOLO(yolo_weights)
                self.yolo_model.to(dev)
            else:
                logger.warning("YOLO weights %s not found, will download standard yolov8n", yolo_weights)
                self.yolo_model = YOLO("yolov8n.pt")
                self.yolo_model.to(dev)

            if os.path.exists(plate_detect_weights):
                logger.info("loading plate pose detector from %s", plate_detect_weights)
                self.plate_detector = YOLO(plate_detect_weights)
                self.plate_detector.to(dev)
            else:
                logger.warning("plate pose weights %s not found", plate_detect_weights)

            if os.path.exists(plate_rec_weights):
                logger.info("loading plate OCR model from %s", plate_rec_weights)
                self.plate_rec_model = init_model(dev, plate_rec_weights, is_color=True)
            else:
                logger.warning("plate OCR weights %s not found", plate_rec_weights)

            self._init_success = True
            logger.info("UnifiedAiEngine loaded successfully on device=%s", self.device)
        except Exception as exc:
            logger.error("failed to load AI models: %s", exc, exc_info=True)
            self._init_success = False

    def is_ready(self) -> bool:
        return self._init_success and self.yolo_model is not None

    def detect_objects(self, frame: np.ndarray, conf: float = 0.3) -> list[dict[str, Any]]:
        """Run YOLO object detection and filter for vehicles, non-motor, and obstacles."""
        if self.yolo_model is None or frame is None:
            return []

        results = self.yolo_model(frame, conf=conf, verbose=False)
        detected = []
        for res in results:
            boxes = res.boxes
            if len(boxes) == 0:
                continue
            for idx in range(len(boxes)):
                cls_id = int(boxes.cls[idx])
                if cls_id not in TARGET_CLASSES:
                    continue
                score = float(boxes.conf[idx])
                x1, y1, x2, y2 = [int(v) for v in boxes.xyxy[idx].cpu().numpy()]
                name, cat_type = TARGET_CLASSES[cls_id]
                detected.append(
                    {
                        "class_id": cls_id,
                        "class_name": name,
                        "category_type": cat_type,
                        "confidence": score,
                        "bbox": [x1, y1, x2, y2],
                        "bottom_center": [(x1 + x2) // 2, y2],
                        "center": [(x1 + x2) // 2, (y1 + y2) // 2],
                    }
                )
        return detected

    def detect_and_recognize_plates(
        self,
        img_crop: np.ndarray,
        conf: float = 0.25,
    ) -> list[dict[str, Any]]:
        """Run YOLO-Pose plate detection + 4-point perspective warp + CRNN OCR on image/crop."""
        if self.plate_detector is None or self.plate_rec_model is None or img_crop is None:
            return []

        import torch
        from plate_recognition.double_plate_split_merge import get_split_merge
        from plate_recognition.plate_rec import get_plate_result

        dev = torch.device(self.device)
        plates = []

        try:
            results = self.plate_detector(img_crop, conf=conf, verbose=False)
            for res in results:
                boxes = res.boxes
                keypoints = res.keypoints
                if len(boxes) == 0 or keypoints is None:
                    continue

                kpts_xy = keypoints.xy
                num_det = min(len(boxes), len(kpts_xy))
                for idx in range(num_det):
                    box = [int(v) for v in boxes.xyxy[idx].cpu().numpy()]
                    det_conf = float(boxes.conf[idx])
                    plate_layer_type = int(boxes.cls[idx])  # 0: single, 1: double
                    landmarks = kpts_xy[idx].cpu().numpy().astype(np.int64)

                    warped = four_point_transform(img_crop, landmarks)
                    if plate_layer_type == 1:
                        warped = get_split_merge(warped)

                    plate_number, _, raw_color, color_conf = get_plate_result(
                        warped, dev, self.plate_rec_model, is_color=True
                    )

                    plate_text = clean_plate_text(plate_number)
                    if not plate_text or len(plate_text) < 5:
                        continue

                    color, v_type, viol_type = classify_plate_attributes(plate_text, raw_color)

                    plates.append(
                        {
                            "plate": plate_text,
                            "plate_color": color,
                            "raw_color": raw_color,
                            "vehicle_type": v_type,
                            "violation_type": viol_type,
                            "confidence": det_conf,
                            "bbox": box,
                            "landmarks": landmarks.tolist(),
                        }
                    )
        except Exception as exc:
            logger.warning("plate detection error: %s", exc)

        return plates

    def evaluate_space(
        self,
        frame: np.ndarray,
        space_id: str,
        roi_json: str | None = None,
    ) -> SpacePerception:
        """Evaluate space occupancy, vehicle/obstacle type, and license plate."""
        h, w = frame.shape[:2]

        # 1. 解析车位多边形 (Parking Space Polygon)
        polygon = self._parse_space_polygon(roi_json, w, h)

        # 2. 全图检测所有目标
        detected_objects = self.detect_objects(frame)

        # 3. 空间拓扑匹配：筛选落在本车位多边形内的物体
        space_objects = []
        for obj in detected_objects:
            anchor = obj["bottom_center"] if obj["category_type"] in {"CAR", "HEAVY_VEHICLE"} else obj["center"]
            # cv2.pointPolygonTest: >= 0 表示点在多边形内部或边界上
            inside = cv2.pointPolygonTest(polygon, (float(anchor[0]), float(anchor[1])), False) >= 0
            if inside:
                space_objects.append(obj)

        if not space_objects:
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

        # 4. 目标优先级判定: CAR > HEAVY_VEHICLE > NON_MOTOR > OBSTACLE
        car_objs = [o for o in space_objects if o["category_type"] == "CAR"]
        heavy_objs = [o for o in space_objects if o["category_type"] == "HEAVY_VEHICLE"]
        non_motor_objs = [o for o in space_objects if o["category_type"] == "NON_MOTOR"]
        obstacle_objs = [o for o in space_objects if o["category_type"] == "OBSTACLE"]

        # CASE A: 检测到小轿车/车辆 -> 触发车位局部车牌识别
        if car_objs:
            target_car = car_objs[0]
            # 裁剪车位/车辆所在 ROI 区域进行车牌识别
            x1, y1, x2, y2 = target_car["bbox"]
            pad_x = int((x2 - x1) * 0.15)
            pad_y = int((y2 - y1) * 0.15)
            cx1 = max(0, x1 - pad_x)
            cy1 = max(0, y1 - pad_y)
            cx2 = min(w, x2 + pad_x)
            cy2 = min(h, y2 + pad_y)
            crop = frame[cy1:cy2, cx1:cx2]

            plates = self.detect_and_recognize_plates(crop)
            if plates:
                best_plate = plates[0]
                return SpacePerception(
                    space_id=space_id,
                    has_vehicle=True,
                    plate=best_plate["plate"],
                    plate_color=best_plate["plate_color"],
                    vehicle_type=best_plate["vehicle_type"],
                    violation_type=best_plate["violation_type"],
                    confidence=best_plate["confidence"],
                    confirmed=True,
                    evidence_frame=frame.copy(),
                    bbox=target_car["bbox"],
                    category_label=f"车辆入位 ({best_plate['plate']})",
                )
            else:
                # 物理在位，但车牌暂未识别
                return SpacePerception(
                    space_id=space_id,
                    has_vehicle=True,
                    plate=None,
                    plate_color="UNKNOWN",
                    vehicle_type="UNKNOWN",
                    violation_type=None,
                    confidence=target_car["confidence"],
                    confirmed=False,
                    evidence_frame=frame.copy(),
                    bbox=target_car["bbox"],
                    category_label="车辆在位 (车牌识别中)",
                )

        # CASE B: 大客货车违规占用
        if heavy_objs:
            target_heavy = heavy_objs[0]
            return SpacePerception(
                space_id=space_id,
                has_vehicle=True,
                plate=None,
                plate_color="YELLOW",
                vehicle_type="HEAVY_VEHICLE",
                violation_type="HEAVY_VEHICLE",
                confidence=target_heavy["confidence"],
                confirmed=True,
                evidence_frame=frame.copy(),
                bbox=target_heavy["bbox"],
                category_label=f"大客货车违规占用 ({target_heavy['class_name']})",
            )

        # CASE C: 自行车/两轮车占用
        if non_motor_objs:
            target_nm = non_motor_objs[0]
            return SpacePerception(
                space_id=space_id,
                has_vehicle=True,
                plate=None,
                plate_color="TWO_WHEELER",
                vehicle_type="NON_MOTOR",
                violation_type="NON_MOTOR_VEHICLE",
                confidence=target_nm["confidence"],
                confirmed=True,
                evidence_frame=frame.copy(),
                bbox=target_nm["bbox"],
                category_label=f"两轮车非法占用 ({target_nm['class_name']})",
            )

        # CASE D: 桌椅板凳/杂物非法霸位
        if obstacle_objs:
            target_obs = obstacle_objs[0]
            return SpacePerception(
                space_id=space_id,
                has_vehicle=True,
                plate=None,
                plate_color="OBSTACLE",
                vehicle_type="OBSTACLE",
                violation_type="OBSTACLE_OCCUPATION",
                confidence=target_obs["confidence"],
                confirmed=True,
                evidence_frame=frame.copy(),
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

    def _parse_space_polygon(self, roi_json: str | None, width: int, height: int) -> np.ndarray:
        """Parse roi_json polygon points, or fallback to sensible default space polygon."""
        if roi_json:
            try:
                data = json.loads(roi_json) if isinstance(roi_json, str) else roi_json
                if isinstance(data, dict):
                    # Format: {"polygon": [[x1,y1], [x2,y2], ...]}
                    poly = data.get("polygon") or data.get("points")
                    if isinstance(poly, list) and len(poly) >= 3:
                        pts = []
                        for pt in poly:
                            px = float(pt[0])
                            py = float(pt[1])
                            if px <= 1.0 and py <= 1.0:
                                px *= width
                                py *= height
                            pts.append([int(px), int(py)])
                        return np.array(pts, dtype=np.int32)
                    # Rect format: {"x": 100, "y": 100, "w": 400, "h": 500}
                    if "x" in data and "y" in data and "w" in data and "h" in data:
                        rx, ry, rw, rh = float(data["x"]), float(data["y"]), float(data["w"]), float(data["h"])
                        if rx <= 1.0 and ry <= 1.0 and rw <= 1.0 and rh <= 1.0:
                            rx *= width
                            ry *= height
                            rw *= width
                            rh *= height
                        return np.array(
                            [[rx, ry], [rx + rw, ry], [rx + rw, ry + rh], [rx, ry + rh]],
                            dtype=np.int32,
                        )
            except Exception as exc:
                logger.warning("failed to parse roi_json %s: %s", roi_json, exc)

        # 默认四边形：覆盖画面中央核心车位区域 (15% ~ 85%)
        x1 = int(width * 0.15)
        y1 = int(height * 0.15)
        x2 = int(width * 0.85)
        y2 = int(height * 0.95)
        return np.array([[x1, y1], [x2, y1], [x2, y2], [x1, y2]], dtype=np.int32)
