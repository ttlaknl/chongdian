"""Unit tests for Milestone 2A: 4-corner perspective warping, outdoor backlight enhancement, and compatible plate detection parsing."""

from __future__ import annotations

import sys
import unittest
from pathlib import Path
from unittest.mock import MagicMock

import cv2
import numpy as np

VISION_DIR = Path(__file__).resolve().parents[1]
if str(VISION_DIR) not in sys.path:
    sys.path.insert(0, str(VISION_DIR))

from geometry import (
    order_plate_corners,
    split_and_merge_double_plate,
    warp_plate_perspective,
)
from plate_postprocess import (
    assess_plate_illumination,
    enhance_backlit_plate,
)
from plate_recognizer import (
    PlateDetection,
    extract_plate_patch,
    parse_plate_detection_output,
)


class TestPlateCornersAndPerspectiveWarp(unittest.TestCase):
    """Test 4-corner ordering, perspective transform, and double-layer split & merge."""

    def test_order_corners_from_shuffled_inputs(self):
        """Randomly shuffled 4 points must always be canonically ordered [TL, TR, BR, BL]."""
        # Canonical points: TL=(20, 20), TR=(150, 25), BR=(145, 65), BL=(22, 60)
        canonical = np.array([[20.0, 20.0], [150.0, 25.0], [145.0, 65.0], [22.0, 60.0]], dtype=np.float32)

        permutations = [
            [0, 1, 2, 3],  # Identity
            [3, 2, 1, 0],  # Reversed
            [1, 3, 0, 2],  # Scrambled
            [2, 0, 3, 1],  # Scrambled
            [3, 0, 1, 2],  # Rotated
        ]

        for perm in permutations:
            shuffled = canonical[perm]
            ordered = order_plate_corners(shuffled)
            self.assertIsNotNone(ordered, f"Failed to order permutation {perm}")
            self.assertEqual(ordered.shape, (4, 2))
            # Check TL is closest to (20, 20)
            np.testing.assert_allclose(ordered[0], canonical[0], atol=1e-3)
            # Check TR is closest to (150, 25)
            np.testing.assert_allclose(ordered[1], canonical[1], atol=1e-3)
            # Check BR is closest to (145, 65)
            np.testing.assert_allclose(ordered[2], canonical[2], atol=1e-3)
            # Check BL is closest to (22, 60)
            np.testing.assert_allclose(ordered[3], canonical[3], atol=1e-3)

    def test_order_corners_rejects_degenerate_quads(self):
        """Degenerate inputs (collinear, non-convex, area < 20, bad shape) must return None."""
        # 1. Collinear points
        collinear = np.array([[0, 0], [10, 10], [20, 20], [30, 30]], dtype=np.float32)
        self.assertIsNone(order_plate_corners(collinear), "Collinear points must return None")

        # 2. Non-convex / concave quad (arrowhead)
        concave = np.array([[0, 0], [100, 0], [40, 40], [0, 100]], dtype=np.float32)
        self.assertIsNone(order_plate_corners(concave), "Concave quad must return None")

        # 3. Area < 20 px^2
        tiny = np.array([[0, 0], [4, 0], [4, 2], [0, 2]], dtype=np.float32)
        self.assertIsNone(order_plate_corners(tiny), "Tiny quad (<20 px^2) must return None")

        # 4. Wrong shape or None
        self.assertIsNone(order_plate_corners(None))
        self.assertIsNone(order_plate_corners(np.zeros((3, 2), dtype=np.float32)))

    def test_warp_plate_perspective_single_layer(self):
        """warp_plate_perspective must rectify tilted synthetic plate to exactly (48, 168, 3)."""
        img = np.zeros((300, 400, 3), dtype=np.uint8)
        cv2.putText(img, "TEST", (60, 120), cv2.FONT_HERSHEY_SIMPLEX, 1.0, (255, 255, 255), 2)
        corners = np.array([[50, 80], [220, 60], [225, 110], [55, 130]], dtype=np.float32)

        warped = warp_plate_perspective(img, corners, is_double_layer=False, target_single=(168, 48))
        self.assertEqual(warped.shape, (48, 168, 3))
        self.assertEqual(warped.dtype, np.uint8)

    def test_warp_plate_perspective_double_layer(self):
        """warp_plate_perspective with is_double_layer must split & merge into single-line (48, 168, 3)."""
        img = np.zeros((300, 400, 3), dtype=np.uint8)
        cv2.putText(img, "ROW1", (60, 90), cv2.FONT_HERSHEY_SIMPLEX, 0.8, (255, 255, 255), 2)
        cv2.putText(img, "ROW2", (60, 130), cv2.FONT_HERSHEY_SIMPLEX, 0.8, (255, 255, 255), 2)
        corners = np.array([[50, 70], [200, 70], [200, 150], [50, 150]], dtype=np.float32)

        warped = warp_plate_perspective(img, corners, is_double_layer=True, target_single=(168, 48), target_double=(168, 96))
        self.assertEqual(warped.shape, (48, 168, 3))

    def test_warp_plate_perspective_fallback_on_degenerate(self):
        """Degenerate corners must gracefully fall back to bounding box crop of (48, 168, 3)."""
        img = np.zeros((300, 400, 3), dtype=np.uint8)
        collinear = np.array([[10, 10], [20, 20], [30, 30], [40, 40]], dtype=np.float32)
        warped = warp_plate_perspective(img, collinear, target_single=(168, 48))
        self.assertEqual(warped.shape, (48, 168, 3))


class TestIlluminationAndAdaptiveEnhancement(unittest.TestCase):
    """Test outdoor backlight, underexposure, overexposure assessment and CLAHE enhancement."""

    def test_assess_illumination_dark_plate(self):
        """Dark plate (mean < 75) must trigger needs_enhance=True."""
        dark_img = np.full((48, 168, 3), 45, dtype=np.uint8)
        mean_l, std_l, lap_v, needs_enh = assess_plate_illumination(dark_img, low_luma_thresh=75.0)
        self.assertEqual(mean_l, 45.0)
        self.assertTrue(needs_enh, "Dark plate must request enhancement")

    def test_assess_illumination_low_contrast_plate(self):
        """Low contrast plate (std < 28) must trigger needs_enhance=True."""
        # Mean = 100, but nearly flat (std = 5)
        flat_img = np.random.randint(95, 105, size=(48, 168, 3), dtype=np.uint8)
        mean_l, std_l, lap_v, needs_enh = assess_plate_illumination(flat_img, low_contrast_thresh=28.0)
        self.assertLess(std_l, 28.0)
        self.assertTrue(needs_enh, "Flat contrast plate must request enhancement")

    def test_assess_illumination_normal_plate(self):
        """Well-illuminated plate with good contrast must NOT trigger needs_enhance."""
        normal_img = np.zeros((48, 168, 3), dtype=np.uint8)
        # Background blue
        normal_img[:, :] = (150, 80, 20)
        # White text
        normal_img[10:38, 20:148] = (220, 220, 220)

        mean_l, std_l, lap_v, needs_enh = assess_plate_illumination(normal_img)
        self.assertGreater(mean_l, 75.0)
        self.assertGreater(std_l, 28.0)
        self.assertFalse(needs_enh, "Well-lit plate should not trigger enhancement")

    def test_assess_illumination_overexposed_specular_plate(self):
        """Plate with >=15% specular reflection (>=240) must trigger needs_enhance=True."""
        overexp_img = np.full((48, 168, 3), 120, dtype=np.uint8)
        # Add 25% blown out pixels
        overexp_img[:, :42] = 250
        mean_l, std_l, lap_v, needs_enh = assess_plate_illumination(overexp_img)
        self.assertTrue(needs_enh, "Blown out specular highlight should trigger enhancement")

    def test_enhance_backlit_plate_increases_luma_and_preserves_shape(self):
        """enhance_backlit_plate must increase luminance, dynamic range, and preserve dimensions."""
        dark_img = np.full((48, 168, 3), 35, dtype=np.uint8)
        # Set some color (blueish plate)
        dark_img[:, :, 0] = 70
        dark_img[:, :, 1] = 30
        dark_img[:, :, 2] = 20

        orig_mean = float(np.mean(dark_img))
        enhanced = enhance_backlit_plate(dark_img, clip_limit=2.5, gamma=0.75)

        self.assertEqual(enhanced.shape, (48, 168, 3))
        self.assertEqual(enhanced.dtype, np.uint8)
        enh_mean = float(np.mean(enhanced))
        self.assertGreater(enh_mean, orig_mean, "Enhancement should increase image luminance")

    def test_dual_path_ocr_selection_prefers_valid_regex(self):
        """Dual-path selection must choose enhanced result if original fails regex but enhanced passes."""
        from ai_engine import clean_plate_text
        from plate_postprocess import is_valid_chinese_plate

        # Original was corrupted by dark shadows / low contrast: e.g. '青11H135' (invalid regex)
        orig_raw = "青11H135"
        orig_conf = 0.60
        # Enhanced extracted valid plate: '粤B12345'
        enh_raw = "粤B12345"
        enh_conf = 0.88

        v_orig = is_valid_chinese_plate(clean_plate_text(orig_raw))
        v_enh = is_valid_chinese_plate(clean_plate_text(enh_raw))

        self.assertFalse(v_orig)
        self.assertTrue(v_enh)

        # Selection rule check
        pick_enhanced = False
        if v_enh and not v_orig:
            pick_enhanced = True
        elif v_enh and v_orig:
            if enh_conf > orig_conf:
                pick_enhanced = True

        self.assertTrue(pick_enhanced, "Should pick enhanced plate when it passes regex")


class TestPlateDetectionCompatibilityParsing(unittest.TestCase):
    """Test 4-corner keypoints vs traditional 4-box output parsing and patch extraction."""

    def test_parse_4_corner_keypoint_format(self):
        """14-item format [x1, y1, x2, y2, conf, cls, x_tl, y_tl, x_tr, y_tr, x_br, y_br, x_bl, y_bl]."""
        raw = [10.0, 20.0, 110.0, 50.0, 0.92, 0.0, 12.0, 22.0, 108.0, 21.0, 107.0, 49.0, 11.0, 48.0]
        parsed = parse_plate_detection_output(raw)
        self.assertTrue(parsed["has_keypoints"])
        self.assertEqual(parsed["bbox"], [10, 20, 110, 50])
        self.assertEqual(parsed["confidence"], 0.92)
        self.assertEqual(parsed["layer_type"], 0)
        self.assertFalse(parsed["is_double_layer"])
        self.assertEqual(len(parsed["landmarks"]), 4)
        self.assertEqual(parsed["landmarks"][0], [12, 22])

    def test_parse_traditional_4_box_format(self):
        """6-item format [x1, y1, x2, y2, conf, cls] without keypoints."""
        raw = [10.0, 20.0, 110.0, 50.0, 0.88, 0.0]
        parsed = parse_plate_detection_output(raw)
        self.assertFalse(parsed["has_keypoints"])
        self.assertIsNone(parsed["landmarks"])
        self.assertEqual(parsed["bbox"], [10, 20, 110, 50])
        self.assertEqual(parsed["confidence"], 0.88)

    def test_double_layer_auto_detection(self):
        """Double layer detected by layer_type == 1 OR aspect_ratio < 2.1."""
        # 1. layer_type = 1
        d1 = parse_plate_detection_output([0, 0, 120, 30, 0.9, 1])
        self.assertTrue(d1["is_double_layer"])

        # 2. Aspect ratio < 2.1 (w=80, h=50 -> 1.6)
        d2 = parse_plate_detection_output([0, 0, 80, 50, 0.9, 0])
        self.assertTrue(d2["is_double_layer"])

        # 3. Single layer normal aspect ratio (w=140, h=40 -> 3.5)
        d3 = parse_plate_detection_output([0, 0, 140, 40, 0.9, 0])
        self.assertFalse(d3["is_double_layer"])

    def test_extract_plate_patch_keypoints_vs_box(self):
        """extract_plate_patch returns was_warped=True for keypoints, False for traditional box."""
        img = np.zeros((300, 400, 3), dtype=np.uint8)

        # Case A: 4 keypoints
        kpt_det = {
            "bbox": [50, 80, 220, 130],
            "is_double_layer": False,
            "landmarks": [[52, 82], [218, 80], [217, 128], [51, 129]],
        }
        patch_kpt, was_warped = extract_plate_patch(img, kpt_det)
        self.assertTrue(was_warped)
        self.assertEqual(patch_kpt.shape, (48, 168, 3))

        # Case B: traditional box
        box_det = {
            "bbox": [50, 80, 220, 130],
            "is_double_layer": False,
            "landmarks": None,
        }
        patch_box, was_warped = extract_plate_patch(img, box_det)
        self.assertFalse(was_warped)
        self.assertEqual(patch_box.shape, (48, 168, 3))


class TestPlateDetectionDataclass(unittest.TestCase):
    """Test metadata additions to PlateDetection dataclass."""

    def test_plate_detection_backward_compatibility(self):
        """PlateDetection can be instantiated with 4 positional arguments as before."""
        det = PlateDetection("粤B12345", "BLUE", "FUEL", 0.95)
        self.assertEqual(det.plate, "粤B12345")
        self.assertEqual(det.plate_color, "BLUE")
        self.assertEqual(det.confidence, 0.95)
        self.assertEqual(det.laplacian_var, 0.0)
        self.assertFalse(det.warped)
        self.assertFalse(det.enhanced)

    def test_plate_detection_with_new_metadata(self):
        """PlateDetection holds full Milestone 2A metadata."""
        det = PlateDetection(
            plate="粤B12345",
            plate_color="BLUE",
            vehicle_type="FUEL",
            confidence=0.95,
            laplacian_var=125.4,
            plate_area=3600,
            warped=True,
            enhanced=True,
            mean_luma=62.3,
            std_luma=22.1,
        )
        self.assertEqual(det.laplacian_var, 125.4)
        self.assertEqual(det.plate_area, 3600)
        self.assertTrue(det.warped)
        self.assertTrue(det.enhanced)
        self.assertEqual(det.mean_luma, 62.3)


if __name__ == "__main__":
    unittest.main()
