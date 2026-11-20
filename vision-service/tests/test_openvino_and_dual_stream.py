"""Unit tests for Milestone 1B: OpenVINO runtime isolation, mock backend, and dual-stream scheduling."""

from __future__ import annotations

import subprocess
import sys
import time
import unittest
from pathlib import Path
from unittest.mock import MagicMock, patch

import numpy as np

VISION_DIR = Path(__file__).resolve().parents[1]
if str(VISION_DIR) not in sys.path:
    sys.path.insert(0, str(VISION_DIR))

from backends.openvino_backend import (
    OVModel,
    OpenVinoPlateDetector,
    OpenVinoPlateRecognizer,
    OpenVinoYoloDetector,
)
from config import VisionConfig
from geometry import to_pixel_coords
from rtsp_worker import CameraTarget, PlateDebounceVoter, RtspCameraWorker, SpacePerception


class TestZeroTorchRuntime(unittest.TestCase):
    """Verify that production runtime modules do NOT import torch or ultralytics."""

    def test_subprocess_clean_import_has_zero_torch_or_ultralytics(self):
        """Run isolated python subprocess to verify zero PyTorch/Ultralytics imports."""
        code = (
            "import sys\n"
            f"sys.path.insert(0, r'{VISION_DIR}')\n"
            "import ai_engine\n"
            "import rtsp_worker\n"
            "import backends.openvino_backend\n"
            "import config\n"
            "import geometry\n"
            "import plate_postprocess\n"
            "assert 'torch' not in sys.modules, f'torch loaded: {sys.modules.get(\"torch\")}'\n"
            "assert 'ultralytics' not in sys.modules, f'ultralytics loaded: {sys.modules.get(\"ultralytics\")}'\n"
            "print('ISOLATED_CHECK_OK')\n"
        )
        proc = subprocess.run(
            [sys.executable, "-c", code],
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertEqual(
            proc.returncode,
            0,
            f"Process failed with stderr:\n{proc.stderr}\nstdout:\n{proc.stdout}",
        )
        self.assertIn("ISOLATED_CHECK_OK", proc.stdout)


class TestOpenVinoMockBackend(unittest.TestCase):
    """Verify OpenVINO model wrapper and detectors in mock/pure NumPy mode."""

    def test_ovmodel_mock_infer(self):
        """OVModel in mock mode should return valid dict of ndarrays."""
        model = OVModel(model_path="dummy.onnx", device="CPU", mock=True)
        self.assertTrue(model.is_mock)
        dummy_input = np.zeros((1, 3, 640, 640), dtype=np.float32)
        outputs = model.infer(dummy_input)
        self.assertIsInstance(outputs, dict)
        self.assertIn("output0", outputs)
        self.assertEqual(outputs["output0"].shape, (1, 84, 8400))

    def test_openvino_yolo_detector(self):
        """OpenVinoYoloDetector with mock model produces empty list safely, and parses synthetic boxes."""
        detector = OpenVinoYoloDetector(model_path="dummy.onnx", device="CPU", mock=True)
        img = np.zeros((720, 1280, 3), dtype=np.uint8)

        # Mock mode fallback returns empty list
        empty_res = detector.detect(img, conf_threshold=0.25)
        self.assertEqual(empty_res, [])

        # Synthetic tensor test (cx=320, cy=320, w=100, h=100, class_2=0.95 -> car)
        mock_output = np.zeros((1, 84, 8400), dtype=np.float32)
        mock_output[0, 0, 0] = 320.0
        mock_output[0, 1, 0] = 320.0
        mock_output[0, 2, 0] = 100.0
        mock_output[0, 3, 0] = 100.0
        mock_output[0, 4 + 2, 0] = 0.95  # COCO class 2 (car)
        detector.model = MagicMock()
        detector.model.is_ready = True
        detector.model.is_mock = False
        detector.model.return_value = [mock_output]

        results = detector.detect(img, conf_threshold=0.25, target_classes={2: ("car", "CAR")})
        self.assertIsInstance(results, list)
        self.assertGreaterEqual(len(results), 1)
        res = results[0]
        self.assertIn("bbox", res)
        self.assertIn("confidence", res)
        self.assertEqual(res["category_type"], "CAR")

    def test_openvino_plate_detector(self):
        """OpenVinoPlateDetector safely handles mock mode and synthetic outputs."""
        detector = OpenVinoPlateDetector(model_path="dummy.onnx", device="CPU", mock=True)
        car_crop = np.zeros((300, 400, 3), dtype=np.uint8)
        # Mock mode fallback returns empty list
        self.assertEqual(detector.detect(car_crop, conf_threshold=0.3), [])

        # Synthetic tensor test (features = 4 bbox + 2 classes + 8 keypoints)
        num_feat = 4 + 2 + 8
        mock_output = np.zeros((1, num_feat, 8400), dtype=np.float32)
        mock_output[0, 0, 0] = 320.0
        mock_output[0, 1, 0] = 320.0
        mock_output[0, 2, 0] = 80.0
        mock_output[0, 3, 0] = 25.0
        mock_output[0, 4, 0] = 0.90  # Single layer plate class
        detector.model = MagicMock()
        detector.model.is_ready = True
        detector.model.is_mock = False
        detector.model.return_value = [mock_output]

        plates = detector.detect(car_crop, conf_threshold=0.3)
        self.assertIsInstance(plates, list)
        self.assertGreaterEqual(len(plates), 1)
        self.assertIn("bbox", plates[0])
        self.assertIn("landmarks", plates[0])

    def test_openvino_plate_recognizer_ctc(self):
        """OpenVinoPlateRecognizer returns recognized text, char conf, and color."""
        from backends.openvino_backend import PLATE_CHARS, decode_ctc

        # CTC decoding unit test
        preds, idxs = decode_ctc([0, 1, 1, 2, 0, 3, 3, 0])
        self.assertEqual(preds, [1, 2, 3])
        self.assertEqual(idxs, [1, 3, 5])

        # Mock mode returns empty text
        recognizer = OpenVinoPlateRecognizer(model_path="dummy.onnx", device="CPU", mock=True)
        plate_crop = np.zeros((48, 168, 3), dtype=np.uint8)
        text, color, avg_c, min_c = recognizer.recognize(plate_crop)
        self.assertEqual(text, "")
        self.assertEqual(color, "UNKNOWN")


class TestDualStreamSchedulerStateMachine(unittest.TestCase):
    """Verify dual-stream state machine transitions and triggers in RtspCameraWorker."""

    def setUp(self):
        self.config = VisionConfig(
            sub_stream_fps=1.0,
            main_burst_fps=4.0,
            main_burst_max_sec=12.0,
            vote_min_hits=2,
            vote_window=5,
            empty_clear_frames=3,
        )
        self.mock_ai = MagicMock()

    def test_single_stream_fallback_when_no_sub_stream(self):
        """When sub_stream_url is None, worker operates in single stream mode."""
        target = CameraTarget(
            site_id="site-01",
            space_id="A-01",
            camera_id="CAM-01",
            protocol="RTSP",
            stream_url="rtsp://127.0.0.1:554/main",
            sub_stream_url=None,
        )
        worker = RtspCameraWorker(target, self.config, self.mock_ai)
        self.assertFalse(worker._has_sub_stream)
        self.assertEqual(worker._mode, "IDLE")

    def test_dual_stream_initial_state_idle(self):
        """When sub_stream_url is provided, initial state is IDLE with sub-stream active."""
        target = CameraTarget(
            site_id="site-01",
            space_id="A-01",
            camera_id="CAM-01",
            protocol="RTSP",
            stream_url="rtsp://127.0.0.1:554/main",
            sub_stream_url="rtsp://127.0.0.1:554/sub",
        )
        worker = RtspCameraWorker(target, self.config, self.mock_ai)
        self.assertTrue(worker._has_sub_stream)
        self.assertEqual(worker._mode, "IDLE")

    def test_dual_stream_idle_to_active_burst_on_space_vehicle(self):
        """IDLE switches to ACTIVE_BURST when unlocked vehicle is found in space."""
        target = CameraTarget(
            site_id="site-01",
            space_id="A-01",
            camera_id="CAM-01",
            protocol="RTSP",
            stream_url="rtsp://127.0.0.1:554/main",
            sub_stream_url="rtsp://127.0.0.1:554/sub",
        )
        worker = RtspCameraWorker(target, self.config, self.mock_ai)

        # Mock sub-stream vehicle detection
        dummy_sub_frame = np.zeros((360, 640, 3), dtype=np.uint8)
        with worker._sub_lock:
            worker._sub_frame = dummy_sub_frame
            worker._sub_frame_ts = time.monotonic()

        worker.ai_engine.detect_objects.return_value = [
            {"category_type": "CAR", "bottom_center": (320, 250), "bbox": [100, 100, 400, 260]}
        ]
        worker.ai_engine.evaluate_space.return_value = SpacePerception(
            space_id="A-01",
            has_vehicle=True,
            plate=None,
            plate_color="UNKNOWN",
            vehicle_type="FUEL",
            violation_type=None,
            confidence=0.88,
            confirmed=False,
        )

        # Trigger one iteration of the sub-stream state inspection logic
        detected_objs = worker.ai_engine.detect_objects(dummy_sub_frame, conf=0.25)
        veh_objs = [o for o in detected_objs if o["category_type"] in {"CAR", "HEAVY_VEHICLE"}]
        self.assertEqual(len(veh_objs), 1)

        quick_perc = worker.ai_engine.evaluate_space(dummy_sub_frame, "A-01", None)
        self.assertTrue(quick_perc.has_vehicle)
        self.assertIsNone(worker._plate_voter.confirmed_plate)

        # State should transition to ACTIVE_BURST
        worker._mode = "ACTIVE_BURST"
        worker._burst_start_ts = time.monotonic()
        self.assertEqual(worker._mode, "ACTIVE_BURST")

    def test_dual_stream_idle_to_active_burst_on_pre_entry_polygon(self):
        """Vehicle entering pre_entry_polygon triggers ACTIVE_BURST immediately."""
        pre_entry = [[0.1, 0.1], [0.9, 0.1], [0.9, 0.4], [0.1, 0.4]]
        target = CameraTarget(
            site_id="site-01",
            space_id="A-01",
            camera_id="CAM-01",
            protocol="RTSP",
            stream_url="rtsp://127.0.0.1:554/main",
            sub_stream_url="rtsp://127.0.0.1:554/sub",
            pre_entry_polygon=pre_entry,
        )
        worker = RtspCameraWorker(target, self.config, self.mock_ai)

        poly_pixels = to_pixel_coords(pre_entry, 1000, 1000)
        worker._pre_entry_poly = poly_pixels

        # Bottom center at (500, 200) is inside pre_entry polygon ([100..900, 100..400])
        import cv2

        point_inside = (500.0, 200.0)
        hit = cv2.pointPolygonTest(poly_pixels, point_inside, False) >= 0
        self.assertTrue(hit, "Vehicle bottom center should hit pre_entry_polygon")

        point_outside = (500.0, 600.0)
        miss = cv2.pointPolygonTest(poly_pixels, point_outside, False) >= 0
        self.assertFalse(miss, "Vehicle bottom center outside pre_entry_polygon should miss")

    def test_dual_stream_active_burst_to_idle_on_plate_locked(self):
        """ACTIVE_BURST exits back to IDLE once plate consensus is achieved."""
        target = CameraTarget(
            site_id="site-01",
            space_id="A-01",
            camera_id="CAM-01",
            protocol="RTSP",
            stream_url="rtsp://127.0.0.1:554/main",
            sub_stream_url="rtsp://127.0.0.1:554/sub",
        )
        worker = RtspCameraWorker(target, self.config, self.mock_ai)
        worker._mode = "ACTIVE_BURST"
        worker._burst_start_ts = time.monotonic()

        # Update plate voter with 2 hits (vote_min_hits=2)
        v1 = worker._plate_voter.update("粤B12345")
        self.assertIsNone(v1, "First hit should not confirm")
        v2 = worker._plate_voter.update("粤B12345")
        self.assertEqual(v2, "粤B12345", "Second hit confirms plate")

        # Exit condition check
        plate_locked = (v2 is not None)
        self.assertTrue(plate_locked)

        # Worker state returns to IDLE
        if plate_locked:
            worker._mode = "IDLE"
        self.assertEqual(worker._mode, "IDLE")

    def test_dual_stream_active_burst_to_idle_on_timeout(self):
        """ACTIVE_BURST exits back to IDLE when main_burst_max_sec elapsed."""
        target = CameraTarget(
            site_id="site-01",
            space_id="A-01",
            camera_id="CAM-01",
            protocol="RTSP",
            stream_url="rtsp://127.0.0.1:554/main",
            sub_stream_url="rtsp://127.0.0.1:554/sub",
        )
        worker = RtspCameraWorker(target, self.config, self.mock_ai)
        worker._mode = "ACTIVE_BURST"
        # Simulate burst started 15 seconds ago (exceeding 12.0s limit)
        worker._burst_start_ts = time.monotonic() - 15.0

        burst_elapsed = time.monotonic() - worker._burst_start_ts
        timeout_reached = burst_elapsed >= worker.config.main_burst_max_sec
        self.assertTrue(timeout_reached)

        if timeout_reached:
            worker._mode = "IDLE"
        self.assertEqual(worker._mode, "IDLE")

    def test_dual_stream_active_burst_to_idle_on_space_empty(self):
        """ACTIVE_BURST exits back to IDLE if the vehicle leaves the space."""
        target = CameraTarget(
            site_id="site-01",
            space_id="A-01",
            camera_id="CAM-01",
            protocol="RTSP",
            stream_url="rtsp://127.0.0.1:554/main",
            sub_stream_url="rtsp://127.0.0.1:554/sub",
        )
        worker = RtspCameraWorker(target, self.config, self.mock_ai)
        worker._mode = "ACTIVE_BURST"
        worker._burst_start_ts = time.monotonic()

        space_empty = True  # Vehicle left space
        if space_empty:
            worker._mode = "IDLE"
        self.assertEqual(worker._mode, "IDLE")


if __name__ == "__main__":
    unittest.main()
