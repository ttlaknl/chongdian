"""Offline image evaluation tool for testing smartphone photos or single-frame camera snapshots.

Usage:
    python vision-service/scripts/test_photo.py --image path/to/phone_photo.jpg
    python vision-service/scripts/test_photo.py --image path/to/phone_photo.jpg --output result.jpg
"""

from __future__ import annotations

import argparse
import os
import sys
from pathlib import Path

import cv2
import numpy as np

# Ensure vision-service package is importable
VISION_DIR = Path(__file__).resolve().parents[1]
if str(VISION_DIR) not in sys.path:
    sys.path.insert(0, str(VISION_DIR))

from ai_engine import UnifiedAiEngine
from config import VisionConfig
from geometry import compute_ground_band, get_bottom_anchors, parse_spaces_from_roi_json


def evaluate_photo(
    image_path: str,
    output_path: str | None = None,
    space_id: str = "A-01",
    roi_json: str | None = None,
    weights_dir: str | None = None,
) -> dict:
    if not os.path.exists(image_path):
        print(f"[Error] Image not found: {image_path}")
        return {}

    img = cv2.imread(image_path)
    if img is None:
        print(f"[Error] Failed to decode image: {image_path}")
        return {}

    h, w = img.shape[:2]
    print(f"\n=======================================================")
    print(f" Testing Photo: {image_path} ({w}x{h})")
    print(f"=======================================================")

    wdir = weights_dir or str(VISION_DIR / "weights")
    yolo_w = os.path.join(wdir, "yolov8n.onnx")
    plate_det_w = os.path.join(wdir, "yolo26s-plate-detect.onnx")
    plate_rec_w = os.path.join(wdir, "plate_rec_color.onnx")

    cfg = VisionConfig(debug_draw_boxes=True)
    engine = UnifiedAiEngine(
        yolo_weights=yolo_w,
        plate_detect_weights=plate_det_w,
        plate_rec_weights=plate_rec_w,
        config=cfg,
    )

    # 1. Evaluate Space
    perc = engine.evaluate_space(img, space_id=space_id, roi_json=roi_json)

    print("\n--- 识别与决策结果 ---")
    print(f"车位 ID (spaceId)     : {perc.space_id}")
    print(f"车辆在位 (hasVehicle) : {perc.has_vehicle}")
    print(f"车牌号码 (plate)      : {perc.plate or '无车牌 / 未检出'}")
    print(f"车牌颜色 (plateColor) : {perc.plate_color}")
    print(f"车辆类型 (vehicleType): {perc.vehicle_type}")
    print(f"违规类型 (violation)  : {perc.violation_type or '正常合规'}")
    print(f"业务标签 (label)      : {perc.category_label}")
    print(f"综合置信度 (confidence): {perc.confidence:.2f}")

    # 2. Save annotated visualization
    if perc.evidence_frame is not None:
        out_file = output_path or f"eval_{Path(image_path).stem}.jpg"
        cv2.imwrite(out_file, perc.evidence_frame)
        print(f"\n[OK] 可视化标注图片已保存至: {out_file}")

    return {
        "spaceId": perc.space_id,
        "hasVehicle": perc.has_vehicle,
        "plate": perc.plate,
        "plateColor": perc.plate_color,
        "vehicleType": perc.vehicle_type,
        "violationType": perc.violation_type,
        "categoryLabel": perc.category_label,
        "confidence": perc.confidence,
    }


def main():
    parser = argparse.ArgumentParser(description="Evaluate smartphone photos or static frames")
    parser.add_argument("--image", required=True, help="Path to input image file")
    parser.add_argument("--output", default=None, help="Path to save annotated output image")
    parser.add_argument("--space-id", default="A-01", help="Target parking space ID (default: A-01)")
    parser.add_argument("--roi-json", default=None, help="Optional ROI JSON definition")
    parser.add_argument("--weights-dir", default=None, help="Directory containing ONNX models")
    args = parser.parse_args()

    evaluate_photo(
        image_path=args.image,
        output_path=args.output,
        space_id=args.space_id,
        roi_json=args.roi_json,
        weights_dir=args.weights_dir,
    )


if __name__ == "__main__":
    main()
