"""Pure unit tests for Chinese license plate parsing, classification, ROI cropping, and rolling voting."""

from __future__ import annotations

import json
import sys
import unittest
from pathlib import Path

VISION_DIR = Path(__file__).resolve().parents[1]
if str(VISION_DIR) not in sys.path:
    sys.path.insert(0, str(VISION_DIR))

from plate_recognizer import (  # noqa: E402
    PlateDetection,
    PlateVoteBuffer,
    classify_plate,
    crop_roi,
    is_valid_chinese_plate,
    normalize_plate,
)


class PlateRecognizerTests(unittest.TestCase):
    def test_valid_blue_fuel_plate_and_normalization(self) -> None:
        self.assertEqual(normalize_plate(" 京·a12345 "), "京A12345")
        self.assertTrue(is_valid_chinese_plate("京·A12345"))
        self.assertTrue(is_valid_chinese_plate("粤B88888"))
        color, vtype = classify_plate("粤B88888", 0)
        self.assertEqual((color, vtype), ("BLUE", "FUEL"))

    def test_valid_eight_char_nev_green_plate_overrides_ambiguous_raw_type(self) -> None:
        self.assertTrue(is_valid_chinese_plate("粤BD12345"))
        self.assertTrue(is_valid_chinese_plate("京AD00001"))
        self.assertTrue(is_valid_chinese_plate("沪A12345F"))
        color, vtype = classify_plate("粤BD12345", 0)
        self.assertEqual((color, vtype), ("GREEN", "NEW_ENERGY"))

    def test_invalid_plate_strings_rejected(self) -> None:
        self.assertFalse(is_valid_chinese_plate(""))
        self.assertFalse(is_valid_chinese_plate("TEST"))
        self.assertFalse(is_valid_chinese_plate("123"))
        self.assertFalse(is_valid_chinese_plate("AB12345"))
        self.assertFalse(is_valid_chinese_plate("京112345"))

    def test_crop_roi_normalized_pixels_and_clamping(self) -> None:
        frame = [[(r, c) for c in range(100)] for r in range(80)]

        cropped_norm = crop_roi(frame, json.dumps({"x": 0.25, "y": 0.25, "w": 0.5, "h": 0.5}))
        self.assertEqual(len(cropped_norm), 40)
        self.assertEqual(len(cropped_norm[0]), 50)
        self.assertEqual(cropped_norm[0][0], (20, 25))

        cropped_poly = crop_roi(frame, json.dumps({"points": [[-10, 10], [150, 50]]}))
        self.assertEqual(len(cropped_poly), 40)
        self.assertEqual(len(cropped_poly[0]), 100)
        self.assertEqual(cropped_poly[0][0], (10, 0))

        self.assertEqual(len(crop_roi(frame, "{invalid-json")), 80)

    def test_plate_vote_buffer_requires_consensus_tolerates_flicker_and_clears(self) -> None:
        voter = PlateVoteBuffer(window_size=5, min_hits=2, min_confidence=0.72, empty_clear_frames=3)

        # Low confidence detection ignored before any vehicle is confirmed
        v1 = voter.update(PlateDetection("粤B12345", "BLUE", "FUEL", 0.50))
        self.assertFalse(v1.has_vehicle)
        self.assertFalse(v1.confirmed)

        # First valid hit: waiting for consensus
        v2 = voter.update(PlateDetection("粤B12345", "BLUE", "FUEL", 0.85))
        self.assertTrue(v2.has_vehicle)
        self.assertIsNone(v2.plate)
        self.assertFalse(v2.confirmed)

        # Second valid hit: confirmed!
        v3 = voter.update(PlateDetection("粤b12345", "BLUE", "FUEL", 0.91))
        self.assertTrue(v3.has_vehicle)
        self.assertTrue(v3.confirmed)
        self.assertEqual(v3.plate, "粤B12345")
        self.assertEqual(v3.plate_color, "BLUE")
        self.assertEqual(v3.vehicle_type, "FUEL")

        # 1-2 empty/occluded frames preserve confirmed state
        v4 = voter.update(None)
        self.assertTrue(v4.has_vehicle)
        self.assertEqual(v4.plate, "粤B12345")
        v5 = voter.update(None)
        self.assertTrue(v5.has_vehicle)
        self.assertEqual(v5.plate, "粤B12345")

        # Third consecutive empty frame clears confirmed state cleanly
        v6 = voter.update(None)
        self.assertFalse(v6.has_vehicle)
        self.assertTrue(v6.confirmed)
        self.assertIsNone(v6.plate)
        self.assertEqual(v6.vehicle_type, "UNKNOWN")


if __name__ == "__main__":
    unittest.main()
