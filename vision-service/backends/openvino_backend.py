"""OpenVINO pure-inference backend with zero PyTorch/Ultralytics runtime dependencies.

Provides:
- OVModel: OpenVINO / ONNX runtime inference wrapper with auto GPU/CPU fallback and throughput hints
- Pure NumPy + OpenCV Letterbox preprocessing and cv2.dnn.NMSBoxes postprocessing
- Mock fallback mode when INFERENCE_BACKEND=mock for lightweight CI testing
"""

from __future__ import annotations

import logging
import os
from typing import Any, Sequence

import cv2
import numpy as np

logger = logging.getLogger("vision.backend.openvino")

# Character dictionary for Chinese license plate OCR CTC decoding
PLATE_CHARS = r"#京沪津渝冀晋蒙辽吉黑苏浙皖闽赣鲁豫鄂湘粤桂琼川贵云藏陕甘青宁新学警港澳挂使领民航危0123456789ABCDEFGHJKLMNPQRSTUVWXYZ险品"
PLATE_COLORS = ["黑色", "蓝色", "绿色", "白色", "黄色"]
NORM_MEAN = 0.588
NORM_STD = 0.193


def letterbox(
    image: np.ndarray,
    new_shape: tuple[int, int] = (640, 640),
    color: tuple[int, int, int] = (114, 114, 114),
    auto: bool = False,
    scaleFill: bool = False,
    scaleup: bool = True,
    stride: int = 32,
) -> tuple[np.ndarray, float, tuple[float, float]]:
    """Resize and pad image while meeting stride-multiple constraints."""
    shape = image.shape[:2]  # current shape [height, width]
    if isinstance(new_shape, int):
        new_shape = (new_shape, new_shape)

    # Scale ratio (new / old)
    r = min(new_shape[0] / shape[0], new_shape[1] / shape[1])
    if not scaleup:  # only scale down, do not scale up
        r = min(r, 1.0)

    # Compute padding
    new_unpad = (int(round(shape[1] * r)), int(round(shape[0] * r)))
    dw, dh = new_shape[1] - new_unpad[0], new_shape[0] - new_unpad[1]
    if auto:  # minimum rectangle
        dw, dh = np.mod(dw, stride), np.mod(dh, stride)
    elif scaleFill:  # stretch
        dw, dh = 0.0, 0.0
        new_unpad = (new_shape[1], new_shape[0])
        r = new_shape[1] / shape[1]

    dw /= 2  # divide padding into 2 sides
    dh /= 2

    if shape[::-1] != new_unpad:
        image = cv2.resize(image, new_unpad, interpolation=cv2.INTER_LINEAR)

    top, bottom = int(round(dh - 0.1)), int(round(dh + 0.1))
    left, right = int(round(dw - 0.1)), int(round(dw + 0.1))
    image = cv2.copyMakeBorder(
        image, top, bottom, left, right, cv2.BORDER_CONSTANT, value=color
    )
    return image, r, (dw, dh)


def decode_ctc(preds: Sequence[int]) -> tuple[list[int], list[int]]:
    """Standard CTC greedy decoding removing consecutive duplicates and blank (index 0)."""
    prev = 0
    new_preds: list[int] = []
    indices: list[int] = []
    for i, idx in enumerate(preds):
        if idx != 0 and idx != prev:
            new_preds.append(idx)
            indices.append(i)
        prev = idx
    return new_preds, indices


class OVModel:
    """OpenVINO Model wrapper loading .xml / .bin or .onnx with automatic GPU->CPU fallback."""

    def __init__(
        self,
        model_path: str,
        device: str | None = None,
        num_streams: str | int | None = None,
        is_mock: bool = False,
        mock: bool = False,
    ) -> None:
        self.model_path = model_path
        self.is_mock = is_mock or mock or os.getenv("INFERENCE_BACKEND", "").lower() == "mock"
        self._compiled_model: Any = None
        self._input_layer: Any = None
        self._output_layers: list[Any] = []
        self._core: Any = None

        if self.is_mock or not os.path.exists(model_path):
            if not self.is_mock and not os.path.exists(model_path):
                logger.warning(
                    "model file %s not found on disk, running in mock/fallback mode",
                    model_path,
                )
            self.is_mock = True
            return

        self._init_openvino(device, num_streams)

    def _init_openvino(self, device: str | None, num_streams: str | int | None) -> None:
        try:
            import openvino as ov

            self._core = ov.Core()
            model = self._core.read_model(self.model_path)

            target_device = device or os.getenv("OV_DEVICE", "AUTO:GPU,CPU").strip()
            config: dict[str, str] = {"PERFORMANCE_HINT": "THROUGHPUT"}
            stream_cfg = num_streams or os.getenv("OV_NUM_STREAMS")
            if stream_cfg and str(stream_cfg).upper() != "AUTO":
                config["NUM_STREAMS"] = str(stream_cfg)

            try:
                logger.info(
                    "compiling OpenVINO model %s on target device=%s (hints=%s)",
                    self.model_path,
                    target_device,
                    config,
                )
                self._compiled_model = self._core.compile_model(model, target_device, config)
            except Exception as exc:
                logger.warning(
                    "failed to compile OpenVINO model on %s (%s), gracefully falling back to CPU",
                    target_device,
                    exc,
                )
                self._compiled_model = self._core.compile_model(model, "CPU", {"PERFORMANCE_HINT": "THROUGHPUT"})

            self._input_layer = self._compiled_model.input(0)
            self._output_layers = list(self._compiled_model.outputs)
            logger.info("OpenVINO model %s loaded successfully", self.model_path)
        except Exception as exc:
            logger.error(
                "failed to initialize OpenVINO runtime for %s: %s, falling back to mock mode",
                self.model_path,
                exc,
                exc_info=True,
            )
            self.is_mock = True

    def infer(self, input_tensor: np.ndarray | dict[str, np.ndarray]) -> dict[str, np.ndarray]:
        """Inference returning dict of output name -> ndarray."""
        if self.is_mock or self._compiled_model is None:
            if isinstance(input_tensor, dict):
                first_arr = next(iter(input_tensor.values()))
            else:
                first_arr = input_tensor
            if first_arr is not None and first_arr.ndim >= 2 and first_arr.shape[-2:] == (48, 168):
                return {
                    "char_logits": np.zeros((1, 21, len(PLATE_CHARS)), dtype=np.float32),
                    "color_logits": np.zeros((1, len(PLATE_COLORS)), dtype=np.float32),
                }
            return {"output0": np.zeros((1, 84, 8400), dtype=np.float32)}

        infer_request = self._compiled_model.create_infer_request()
        if isinstance(input_tensor, dict):
            results = infer_request.infer(input_tensor)
        else:
            results = infer_request.infer([input_tensor])

        return {out.get_any_name(): results[out] for out in self._output_layers}

    def __call__(self, input_tensor: np.ndarray) -> list[np.ndarray]:
        res_dict = self.infer(input_tensor)
        return list(res_dict.values())

    @property
    def is_ready(self) -> bool:
        return self._compiled_model is not None or self.is_mock


class OpenVinoYoloDetector:
    """NumPy + OpenCV implementation of YOLOv8 object detection with OpenVINO."""

    def __init__(
        self,
        model: OVModel | None = None,
        input_size: int = 640,
        model_path: str | None = None,
        device: str | None = None,
        is_mock: bool = False,
        mock: bool = False,
    ) -> None:
        if model is None:
            model = OVModel(model_path or "dummy.onnx", device=device, is_mock=is_mock or mock)
        self.model = model
        self.input_size = input_size

    def detect(
        self,
        frame: np.ndarray,
        conf_threshold: float = 0.25,
        target_classes: dict[int, tuple[str, str]] | None = None,
        vehicle_conf_threshold: float = 0.45,
        vehicle_min_size_px: int = 60,
    ) -> list[dict[str, Any]]:
        if frame is None or frame.size == 0 or not self.model.is_ready:
            return []

        if self.model.is_mock:
            return []

        h0, w0 = frame.shape[:2]
        img_padded, ratio, (dw, dh) = letterbox(frame, (self.input_size, self.input_size), auto=False)

        # HWC BGR -> CHW RGB -> float32 [0.0, 1.0]
        blob = img_padded[:, :, ::-1].transpose((2, 0, 1))
        blob = np.ascontiguousarray(blob, dtype=np.float32) / 255.0
        blob = np.expand_dims(blob, axis=0)

        outputs = self.model(blob)
        if not outputs:
            return []

        # YOLOv8 output shape is typically [1, 84, 8400]
        output = outputs[0]
        if output.ndim == 3 and output.shape[1] < output.shape[2]:
            output = output.transpose((0, 2, 1))
        predictions = output[0]  # [8400, 84]

        boxes = predictions[:, :4]  # cx, cy, w, h in 640x640 letterbox coordinates
        scores = predictions[:, 4:]  # class probabilities

        class_ids = np.argmax(scores, axis=1)
        confidences = np.max(scores, axis=1)

        mask = confidences >= conf_threshold
        boxes = boxes[mask]
        confidences = confidences[mask]
        class_ids = class_ids[mask]

        if len(boxes) == 0:
            return []

        # Convert [cx, cy, w, h] to [x1, y1, w, h] for cv2.dnn.NMSBoxes
        nms_boxes = []
        for box in boxes:
            cx, cy, w, h = box
            x1 = cx - w / 2.0
            y1 = cy - h / 2.0
            nms_boxes.append([int(x1), int(y1), int(w), int(h)])

        indices = cv2.dnn.NMSBoxes(
            nms_boxes,
            [float(c) for c in confidences],
            score_threshold=conf_threshold,
            nms_threshold=0.45,
        )

        if len(indices) == 0:
            return []

        if isinstance(indices, np.ndarray):
            indices = indices.flatten().tolist()

        detected: list[dict[str, Any]] = []
        for idx in indices:
            cls_id = int(class_ids[idx])
            score = float(confidences[idx])
            cx, cy, w, h = boxes[idx]

            # Scale back to original image
            x1 = max(0, int(round((cx - w / 2.0 - dw) / ratio)))
            y1 = max(0, int(round((cy - h / 2.0 - dh) / ratio)))
            x2 = min(w0, int(round((cx + w / 2.0 - dw) / ratio)))
            y2 = min(h0, int(round((cy + h / 2.0 - dh) / ratio)))

            box_w = x2 - x1
            box_h = y2 - y1

            if target_classes is not None and cls_id not in target_classes:
                continue

            name, cat_type = (
                target_classes[cls_id] if target_classes and cls_id in target_classes else (str(cls_id), "OTHER")
            )

            # Gating check for vehicles
            if cat_type in {"CAR", "HEAVY_VEHICLE"}:
                if score < vehicle_conf_threshold:
                    continue
                if box_w < vehicle_min_size_px or box_h < vehicle_min_size_px:
                    continue

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


class OpenVinoPlateDetector:
    """NumPy + OpenCV implementation of YOLO-Pose 4-keypoint plate detector with OpenVINO."""

    def __init__(
        self,
        model: OVModel | None = None,
        input_size: int = 640,
        model_path: str | None = None,
        device: str | None = None,
        is_mock: bool = False,
        mock: bool = False,
    ) -> None:
        if model is None:
            model = OVModel(model_path or "dummy.onnx", device=device, is_mock=is_mock or mock)
        self.model = model
        self.input_size = input_size

    def detect(
        self,
        img_crop: np.ndarray,
        conf_threshold: float = 0.55,
        has_keypoints_mode: str = "auto",
    ) -> list[dict[str, Any]]:
        if img_crop is None or img_crop.size == 0 or not self.model.is_ready:
            return []

        if self.model.is_mock:
            return []

        h0, w0 = img_crop.shape[:2]
        img_padded, ratio, (dw, dh) = letterbox(img_crop, (self.input_size, self.input_size), auto=False)

        blob = img_padded[:, :, ::-1].transpose((2, 0, 1))
        blob = np.ascontiguousarray(blob, dtype=np.float32) / 255.0
        blob = np.expand_dims(blob, axis=0)

        outputs = self.model(blob)
        if not outputs:
            return []

        output = outputs[0]
        if output.ndim == 3 and output.shape[1] < output.shape[2]:
            output = output.transpose((0, 2, 1))
        predictions = output[0]  # [8400, features]

        # In YOLO-Pose:
        # features = 4 (bbox: cx, cy, w, h) + num_classes (2: single/double) + 4 keypoints * (2 or 3)
        num_classes = 2
        total_feats = predictions.shape[1]
        boxes = predictions[:, :4]

        if total_feats >= 4 + num_classes:
            scores = predictions[:, 4 : 4 + num_classes]
            kpts_raw = predictions[:, 4 + num_classes :]
        else:
            # Single-class fallback
            scores = predictions[:, 4:5]
            kpts_raw = predictions[:, 5:]

        class_ids = np.argmax(scores, axis=1)
        confidences = np.max(scores, axis=1)

        mask = confidences >= conf_threshold
        boxes = boxes[mask]
        confidences = confidences[mask]
        class_ids = class_ids[mask]
        kpts_raw = kpts_raw[mask]

        if len(boxes) == 0:
            return []

        nms_boxes = []
        for box in boxes:
            cx, cy, w, h = box
            nms_boxes.append([int(cx - w / 2.0), int(cy - h / 2.0), int(w), int(h)])

        indices = cv2.dnn.NMSBoxes(
            nms_boxes,
            [float(c) for c in confidences],
            score_threshold=conf_threshold,
            nms_threshold=0.45,
        )

        if len(indices) == 0:
            return []

        if isinstance(indices, np.ndarray):
            indices = indices.flatten().tolist()

        kpt_mode = str(has_keypoints_mode).strip().lower()
        can_have_kpt = (kpt_mode != "false") and (kpts_raw.shape[1] >= 8)

        results: list[dict[str, Any]] = []
        for idx in indices:
            cx, cy, w, h = boxes[idx]
            x1 = max(0, int(round((cx - w / 2.0 - dw) / ratio)))
            y1 = max(0, int(round((cy - h / 2.0 - dh) / ratio)))
            x2 = min(w0, int(round((cx + w / 2.0 - dw) / ratio)))
            y2 = min(h0, int(round((cy + h / 2.0 - dh) / ratio)))

            bw = max(1, x2 - x1)
            bh = max(1, y2 - y1)
            aspect_ratio = bw / float(bh)
            is_double = (int(class_ids[idx]) == 1) or (aspect_ratio < 2.1)

            # Keypoints: 4 corner landmarks if available
            landmarks: list[list[int]] | None = None
            if can_have_kpt:
                raw_kpt = kpts_raw[idx]
                if len(raw_kpt) >= 8:
                    landmarks = []
                    step = 3 if len(raw_kpt) >= 12 else 2
                    for k in range(4):
                        kx = float(raw_kpt[k * step])
                        ky = float(raw_kpt[k * step + 1])
                        orig_kx = max(0, min(w0 - 1, int(round((kx - dw) / ratio))))
                        orig_ky = max(0, min(h0 - 1, int(round((ky - dh) / ratio))))
                        landmarks.append([orig_kx, orig_ky])

            results.append(
                {
                    "bbox": [x1, y1, x2, y2],
                    "confidence": float(confidences[idx]),
                    "layer_type": int(class_ids[idx]),  # 0: single, 1: double
                    "is_double_layer": is_double,
                    "landmarks": landmarks,
                    "has_keypoints": landmarks is not None,
                }
            )

        return results


class OpenVinoPlateRecognizer:
    """NumPy implementation of CRNN Plate OCR and Color Classification with OpenVINO."""

    def __init__(
        self,
        model: OVModel | None = None,
        height: int = 48,
        width: int = 168,
        model_path: str | None = None,
        device: str | None = None,
        is_mock: bool = False,
        mock: bool = False,
    ) -> None:
        if model is None:
            model = OVModel(model_path or "dummy.onnx", device=device, is_mock=is_mock or mock)
        self.model = model
        self.height = height
        self.width = width

    def recognize(self, warped_img: np.ndarray) -> tuple[str, str, float, float]:
        """Recognize plate text and color from 4-point rectified plate image.

        Returns:
            (plate_text, color_name, ocr_avg_conf, ocr_min_conf)
        """
        if warped_img is None or warped_img.size == 0 or not self.model.is_ready:
            return "", "UNKNOWN", 0.0, 0.0

        if self.model.is_mock:
            return "", "UNKNOWN", 0.0, 0.0

        # Preprocessing: resize to 168x48, normalize with mean/std, CHW float32
        resized = cv2.resize(warped_img, (self.width, self.height))
        norm = (resized.astype(np.float32) / 255.0 - NORM_MEAN) / NORM_STD
        blob = norm.transpose((2, 0, 1))
        blob = np.expand_dims(blob, axis=0)  # (1, 3, 48, 168)

        outputs = self.model(blob)
        if not outputs or len(outputs) < 1:
            return "", "UNKNOWN", 0.0, 0.0

        char_logits = outputs[0]  # shape: [1, w, num_classes] or [w, 1, num_classes]
        if char_logits.ndim == 3 and char_logits.shape[0] != 1 and char_logits.shape[1] == 1:
            char_logits = char_logits.transpose((1, 0, 2))

        # Softmax on characters
        exp_chars = np.exp(char_logits - np.max(char_logits, axis=-1, keepdims=True))
        char_probs = exp_chars / np.sum(exp_chars, axis=-1, keepdims=True)
        char_preds = np.argmax(char_probs[0], axis=-1)
        max_char_probs = np.max(char_probs[0], axis=-1)

        new_preds, new_indices = decode_ctc(char_preds.tolist())
        selected_probs = max_char_probs[new_indices] if len(new_indices) > 0 else np.array([])

        plate_text = "".join(PLATE_CHARS[i] for i in new_preds if i < len(PLATE_CHARS))

        # Color classification
        color_name = "未知"
        if len(outputs) >= 2:
            color_logits = outputs[1]
            exp_color = np.exp(color_logits - np.max(color_logits, axis=-1, keepdims=True))
            color_probs = exp_color / np.sum(exp_color, axis=-1, keepdims=True)
            color_idx = int(np.argmax(color_probs.flatten()))
            if 0 <= color_idx < len(PLATE_COLORS):
                color_name = PLATE_COLORS[color_idx]

        if len(selected_probs) > 0:
            ocr_avg = float(np.mean(selected_probs))
            ocr_min = float(np.min(selected_probs))
        else:
            ocr_avg = 0.0
            ocr_min = 0.0

        return plate_text, color_name, ocr_avg, ocr_min
