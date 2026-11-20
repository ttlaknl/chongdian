"""Offline Model Export Utility: PyTorch -> ONNX -> OpenVINO FP16 IR.

This script is an offline developer tool (NOT used in production runtime).
Usage:
    python vision-service/scripts/export_models.py --weights-dir vision-service/weights --output-dir vision-service/weights
"""

from __future__ import annotations

import argparse
import os
import sys
from pathlib import Path


def export_yolo_detector(pt_path: str, output_dir: str, image_size: int = 640) -> str:
    """Export Ultralytics YOLO object detector to ONNX."""
    from ultralytics import YOLO

    print(f"[1/3] Exporting YOLO detector: {pt_path}")
    model = YOLO(pt_path)
    onnx_path = model.export(
        format="onnx",
        imgsz=image_size,
        dynamic=False,
        opset=12,
        simplify=True,
    )
    print(f"  -> Exported ONNX: {onnx_path}")
    return str(onnx_path)


def export_plate_detector(pt_path: str, output_dir: str, image_size: int = 640) -> str:
    """Export YOLO-Pose license plate detector to ONNX."""
    from ultralytics import YOLO

    print(f"[2/3] Exporting plate detector: {pt_path}")
    model = YOLO(pt_path)
    onnx_path = model.export(
        format="onnx",
        imgsz=image_size,
        dynamic=False,
        opset=12,
        simplify=True,
    )
    print(f"  -> Exported ONNX: {onnx_path}")
    return str(onnx_path)


def export_plate_rec_color(
    pth_path: str,
    output_onnx_path: str,
    height: int = 48,
    width: int = 168,
) -> str:
    """Export CRNN/LPRNet plate OCR + color model to ONNX with named outputs."""
    import torch
    # Ensure plate_recognition module is importable
    vision_dir = Path(__file__).resolve().parents[1]
    if str(vision_dir) not in sys.path:
        sys.path.insert(0, str(vision_dir))

    from plate_recognition.plateNet import myNet_ocr_color
    from plate_recognition.plate_rec import plateName

    print(f"[3/3] Exporting plate OCR + color model: {pth_path}")
    check_point = torch.load(pth_path, map_location="cpu")
    model_state = check_point.get("state_dict", check_point)
    cfg = check_point.get("cfg", [32, 32, 64, 64, "M", 128, 128, "M", 196, 196, "M", 256, 256])

    model = myNet_ocr_color(
        num_classes=len(plateName),
        export=True,
        cfg=cfg,
        color_num=5,
    )
    model.load_state_dict(model_state, strict=False)
    model.eval()

    dummy_input = torch.randn(1, 3, height, width, dtype=torch.float32)

    torch.onnx.export(
        model,
        dummy_input,
        output_onnx_path,
        export_params=True,
        opset_version=12,
        do_constant_folding=True,
        input_names=["input"],
        output_names=["char_logits", "color_logits"],
        dynamic_axes=None,  # Fixed size 1x3x48x168 for deterministic NPU/GPU execution
    )
    print(f"  -> Exported OCR ONNX: {output_onnx_path}")
    return output_onnx_path


def convert_onnx_to_openvino_ir(onnx_path: str, output_dir: str) -> tuple[str, str]:
    """Convert an ONNX model to OpenVINO FP16 IR (.xml and .bin)."""
    try:
        import openvino as ov
    except ImportError:
        print("  [Notice] OpenVINO not installed; skipping IR conversion.")
        return "", ""

    print(f"  -> Converting to OpenVINO FP16 IR: {onnx_path}")
    core = ov.Core()
    model = core.read_model(onnx_path)
    base_name = Path(onnx_path).stem
    xml_path = os.path.join(output_dir, f"{base_name}.xml")
    bin_path = os.path.join(output_dir, f"{base_name}.bin")

    ov.save_model(model, xml_path, compress_to_fp16=True)
    print(f"  -> Saved OpenVINO FP16 IR: {xml_path}")
    return xml_path, bin_path


def main() -> None:
    parser = argparse.ArgumentParser(description="Export PyTorch weights to ONNX and OpenVINO IR")
    parser.add_argument(
        "--weights-dir",
        default="vision-service/weights",
        help="Directory containing .pt and .pth source weights",
    )
    parser.add_argument(
        "--output-dir",
        default="vision-service/weights",
        help="Output directory for .onnx and .xml/.bin files",
    )
    parser.add_argument(
        "--skip-ir",
        action="store_true",
        help="Skip OpenVINO IR conversion (keep ONNX only)",
    )
    args = parser.parse_args()

    os.makedirs(args.output_dir, exist_ok=True)
    weights_dir = args.weights_dir
    output_dir = args.output_dir

    # 1. 车辆检测模型
    yolo_pt = os.path.join(weights_dir, "yolov8n.pt")
    if os.path.exists(yolo_pt):
        onnx_file = export_yolo_detector(yolo_pt, output_dir)
        if not args.skip_ir:
            convert_onnx_to_openvino_ir(onnx_file, output_dir)
    else:
        print(f"Warning: {yolo_pt} not found, skipping object detector export.")

    # 2. 车牌姿态定位模型
    plate_pt = os.path.join(weights_dir, "yolo26s-plate-detect.pt")
    if os.path.exists(plate_pt):
        onnx_file = export_plate_detector(plate_pt, output_dir)
        if not args.skip_ir:
            convert_onnx_to_openvino_ir(onnx_file, output_dir)
    else:
        print(f"Warning: {plate_pt} not found, skipping plate detector export.")

    # 3. 车牌 OCR + 颜色模型
    ocr_pth = os.path.join(weights_dir, "plate_rec_color.pth")
    ocr_onnx = os.path.join(output_dir, "plate_rec_color.onnx")
    if os.path.exists(ocr_pth):
        onnx_file = export_plate_rec_color(ocr_pth, ocr_onnx)
        if not args.skip_ir:
            convert_onnx_to_openvino_ir(onnx_file, output_dir)
    else:
        print(f"Warning: {ocr_pth} not found, skipping plate OCR export.")

    print("\n[Done] All model export routines completed successfully.")


if __name__ == "__main__":
    main()
