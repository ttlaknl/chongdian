"""Comprehensive unit tests for false-positive suppression across the vision-service pipeline.

Tests cover:
  改造 1: 无车不检牌 (vehicle-first gating)
  改造 2: 车牌几何 + 双置信度门槛
  改造 3: 严格中国车牌正则 (民用/新能源/军牌/武警)
  改造 4: 多帧投票防抖
  改造 5: 配置化管理
"""

from __future__ import annotations

import json
import sys
import unittest
from pathlib import Path
from unittest.mock import MagicMock, patch

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

# Import from ai_engine (改造后版本)
from ai_engine import (  # noqa: E402
    is_valid_chinese_plate as ai_is_valid_plate,
    classify_plate_attributes,
    _compute_ioa,
    clean_plate_text,
    STANDARD_PLATE_RE as AI_STANDARD_RE,
    NEV_PLATE_RE as AI_NEV_RE,
    MILITARY_PLATE_RE as AI_MILITARY_RE,
    WJ_PLATE_RE as AI_WJ_RE,
)


# =============================================================================
# 改造 3: 车牌正则校验测试
# =============================================================================
class TestPlateRegexValidation(unittest.TestCase):
    """车牌正则必须拦截非法格式, 放行合法号牌."""

    # --- 必须拦截的非法字符串 ---
    def test_reject_qing_11H135(self):
        """青11H135: 第 2 位是数字, 不符合省份+字母规范."""
        self.assertFalse(is_valid_chinese_plate("青11H135"))
        self.assertFalse(ai_is_valid_plate("青11H135"))

    def test_reject_timestamp_string(self):
        """OSD 时间戳不应被识别为车牌."""
        self.assertFalse(is_valid_chinese_plate("2026/11/20"))
        self.assertFalse(ai_is_valid_plate("20261120"))

    def test_reject_osd_channel_name(self):
        self.assertFalse(is_valid_chinese_plate("测试1"))
        self.assertFalse(ai_is_valid_plate("测试1"))

    def test_reject_second_char_digit(self):
        """鄂112345: 第 2 位是数字 1."""
        self.assertFalse(is_valid_chinese_plate("鄂112345"))
        self.assertFalse(ai_is_valid_plate("鄂112345"))

    def test_reject_forbidden_letters_I_O(self):
        """京I12345 / 京AO2345: I 和 O 是禁用字母."""
        self.assertFalse(is_valid_chinese_plate("京I12345"))
        self.assertFalse(ai_is_valid_plate("京I12345"))

    def test_reject_too_short(self):
        self.assertFalse(is_valid_chinese_plate("京A123"))
        self.assertFalse(ai_is_valid_plate("京A1"))

    def test_reject_too_long(self):
        self.assertFalse(is_valid_chinese_plate("京A123456789"))
        self.assertFalse(ai_is_valid_plate("京A123456789"))

    def test_reject_empty_and_none(self):
        self.assertFalse(is_valid_chinese_plate(""))
        self.assertFalse(is_valid_chinese_plate(None))
        self.assertFalse(ai_is_valid_plate(""))
        self.assertFalse(ai_is_valid_plate(None))

    def test_reject_english_start(self):
        """AB12345: 非省份字符起始."""
        self.assertFalse(is_valid_chinese_plate("AB12345"))
        self.assertFalse(ai_is_valid_plate("AB12345"))

    # --- 必须放行的合法车牌 ---
    def test_allow_standard_blue_7char(self):
        """鄂A12345: 标准蓝牌."""
        self.assertTrue(is_valid_chinese_plate("鄂A12345"))
        self.assertTrue(ai_is_valid_plate("鄂A12345"))

    def test_allow_standard_with_separators(self):
        """京·A12345: 含分隔符的车牌."""
        self.assertTrue(is_valid_chinese_plate("京·A12345"))
        self.assertTrue(ai_is_valid_plate("京·A12345"))

    def test_allow_standard_with_jing_suffix(self):
        """粤B1234警: 末位为 '警'."""
        self.assertTrue(is_valid_chinese_plate("粤B1234警"))
        self.assertTrue(ai_is_valid_plate("粤B1234警"))

    def test_allow_standard_with_gua_suffix(self):
        """京A1234挂: 末位为 '挂'."""
        self.assertTrue(is_valid_chinese_plate("京A1234挂"))
        self.assertTrue(ai_is_valid_plate("京A1234挂"))

    def test_allow_nev_8char_small_DF_prefix(self):
        """京AD08000: 小型新能源 (D/F 在第 3 位)."""
        self.assertTrue(is_valid_chinese_plate("京AD08000"))
        self.assertTrue(ai_is_valid_plate("京AD08000"))

    def test_allow_nev_8char_large_DF_suffix(self):
        """沪A12345F: 大型新能源 (D/F 在末位)."""
        self.assertTrue(is_valid_chinese_plate("沪A12345F"))
        self.assertTrue(ai_is_valid_plate("沪A12345F"))

    def test_allow_nev_粤BD12345(self):
        self.assertTrue(is_valid_chinese_plate("粤BD12345"))
        self.assertTrue(ai_is_valid_plate("粤BD12345"))

    def test_allow_military_plate(self):
        """VA12345: 2012 式军牌."""
        self.assertTrue(ai_is_valid_plate("VA12345"))
        self.assertTrue(is_valid_chinese_plate("VA12345"))

    def test_allow_military_plate_various(self):
        for plate in ["KA88888", "HB12345", "GA00001", "SN99999"]:
            self.assertTrue(ai_is_valid_plate(plate), f"Should accept military plate: {plate}")
            self.assertTrue(is_valid_chinese_plate(plate), f"Should accept military plate: {plate}")

    def test_allow_wj_plate(self):
        """WJ12345: 武警牌."""
        self.assertTrue(ai_is_valid_plate("WJ12345"))
        self.assertTrue(is_valid_chinese_plate("WJ12345"))

    def test_allow_wj_plate_with_province(self):
        """WJ粤12345: 武警牌含省份."""
        self.assertTrue(ai_is_valid_plate("WJ粤12345"))
        self.assertTrue(is_valid_chinese_plate("WJ粤12345"))

    def test_reject_military_with_I_O(self):
        """Military plate with forbidden I/O letters in second position."""
        self.assertFalse(ai_is_valid_plate("VI12345"))
        self.assertFalse(ai_is_valid_plate("VO12345"))


# =============================================================================
# 改造 1: 无车不检牌 (vehicle-gating) 集成测试
# =============================================================================
class TestVehicleFirstGating(unittest.TestCase):
    """When no vehicle is detected, plate detection must NOT run."""

    def test_no_vehicle_no_plate_output(self):
        """Mock YOLO detecting zero vehicles → evaluate_space should return empty with no plate."""
        import numpy as np
        from ai_engine import UnifiedAiEngine, SpacePerception
        from config import VisionConfig

        cfg = VisionConfig()
        engine = UnifiedAiEngine.__new__(UnifiedAiEngine)
        engine.device = "cpu"
        engine.yolo_model = MagicMock()
        engine.plate_detector = MagicMock()
        engine.plate_rec_model = MagicMock()
        engine.hyperlpr_catcher = None
        engine._init_success = True
        engine.config = cfg

        # YOLO returns empty boxes
        mock_result = MagicMock()
        mock_result.boxes = MagicMock()
        mock_result.boxes.__len__ = MagicMock(return_value=0)
        engine.yolo_model.return_value = [mock_result]

        frame = np.zeros((480, 640, 3), dtype=np.uint8)
        result = engine.evaluate_space(frame, "A-01")

        self.assertFalse(result.has_vehicle)
        self.assertIsNone(result.plate)
        self.assertEqual(result.category_label, "空闲待机")

        # Plate detector should NOT have been called
        engine.plate_detector.assert_not_called()


# =============================================================================
# 改造 2: 几何 & IoA 过滤测试
# =============================================================================
class TestGeometryAndIoAFiltering(unittest.TestCase):
    """Test plate bounding box geometry and IoA calculation."""

    def test_ioa_fully_inside(self):
        plate_box = [100, 100, 200, 130]
        vehicle_box = [50, 50, 300, 300]
        ioa = _compute_ioa(plate_box, vehicle_box)
        self.assertAlmostEqual(ioa, 1.0)

    def test_ioa_partially_outside(self):
        plate_box = [0, 0, 100, 30]
        vehicle_box = [50, 0, 300, 300]
        ioa = _compute_ioa(plate_box, vehicle_box)
        # Plate area = 100*30 = 3000, intersection = 50*30 = 1500
        self.assertAlmostEqual(ioa, 0.5)

    def test_ioa_no_overlap(self):
        plate_box = [0, 0, 50, 30]
        vehicle_box = [100, 100, 300, 300]
        ioa = _compute_ioa(plate_box, vehicle_box)
        self.assertAlmostEqual(ioa, 0.0)

    def test_ioa_below_threshold_rejects_plate(self):
        """IoA < 0.7 should reject the plate."""
        from config import VisionConfig
        cfg = VisionConfig()
        plate_box = [0, 0, 100, 30]
        vehicle_box = [60, 0, 300, 300]
        ioa = _compute_ioa(plate_box, vehicle_box)
        self.assertLess(ioa, cfg.plate_ioa_threshold)


# =============================================================================
# 改造 3: classify_plate_attributes 测试
# =============================================================================
class TestClassifyPlateAttributes(unittest.TestCase):
    def test_military_classification(self):
        color, vtype, viol = classify_plate_attributes("VA12345", "白色")
        self.assertEqual(color, "WHITE")
        self.assertEqual(vtype, "SPECIAL_VEHICLE")
        self.assertIsNone(viol)

    def test_nev_green_classification(self):
        color, vtype, viol = classify_plate_attributes("京AD08000", "绿色")
        self.assertEqual(color, "GREEN")
        self.assertEqual(vtype, "NEW_ENERGY")
        self.assertIsNone(viol)

    def test_blue_fuel_classification(self):
        color, vtype, viol = classify_plate_attributes("鄂A12345", "蓝色")
        self.assertEqual(color, "BLUE")
        self.assertEqual(vtype, "FUEL")
        self.assertEqual(viol, "FUEL_VEHICLE")


# =============================================================================
# 改造 4: 多帧投票防抖测试
# =============================================================================
class TestPlateVoteDebouncer(unittest.TestCase):
    """Multi-frame voting must suppress single-frame false positives."""

    def test_single_hit_not_confirmed(self):
        """单帧检出不应直接确认."""
        from rtsp_worker import PlateVoteDebouncer
        voter = PlateVoteDebouncer(window_size=3, min_hits=2, clear_frames=4)
        result = voter.update("鄂A12345")
        # First hit: not yet confirmed (no previous confirmed either)
        self.assertIsNone(result)

    def test_two_hits_confirmed(self):
        """连续 2 帧相同车牌 → 确认."""
        from rtsp_worker import PlateVoteDebouncer
        voter = PlateVoteDebouncer(window_size=3, min_hits=2, clear_frames=4)
        voter.update("鄂A12345")
        result = voter.update("鄂A12345")
        self.assertEqual(result, "鄂A12345")

    def test_different_plates_no_consensus(self):
        """不同车牌交替 → 无共识."""
        from rtsp_worker import PlateVoteDebouncer
        voter = PlateVoteDebouncer(window_size=3, min_hits=2, clear_frames=4)
        voter.update("鄂A12345")
        voter.update("京B99999")
        result = voter.update("粤C11111")
        self.assertIsNone(result)

    def test_empty_frames_clear_cache(self):
        """连续空帧后清除确认状态."""
        from rtsp_worker import PlateVoteDebouncer
        voter = PlateVoteDebouncer(window_size=3, min_hits=2, clear_frames=3)
        voter.update("鄂A12345")
        voter.update("鄂A12345")
        confirmed = voter.update("鄂A12345")
        self.assertEqual(confirmed, "鄂A12345")

        # 3 consecutive empty frames → clear
        voter.update(None)
        voter.update(None)
        result = voter.update(None)
        self.assertIsNone(result)

    def test_invalid_plate_treated_as_empty(self):
        """非法车牌 (如 OSD 文字) 应被视为空帧."""
        from rtsp_worker import PlateVoteDebouncer
        voter = PlateVoteDebouncer(window_size=3, min_hits=2, clear_frames=4)
        voter.update("青11H135")  # invalid → treated as empty
        voter.update("青11H135")  # invalid → treated as empty
        result = voter.update("青11H135")
        self.assertIsNone(result)

    def test_flicker_tolerance(self):
        """确认后 1-2 帧空/噪声不影响保持状态."""
        from rtsp_worker import PlateVoteDebouncer
        voter = PlateVoteDebouncer(window_size=3, min_hits=2, clear_frames=4)
        voter.update("鄂A12345")
        voter.update("鄂A12345")
        confirmed = voter.update("鄂A12345")
        self.assertEqual(confirmed, "鄂A12345")

        # 2 empty frames: should retain
        r1 = voter.update(None)
        self.assertEqual(r1, "鄂A12345")
        r2 = voter.update(None)
        self.assertEqual(r2, "鄂A12345")


# =============================================================================
# Legacy tests from original file (kept for backward compat)
# =============================================================================
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

        v1 = voter.update(PlateDetection("粤B12345", "BLUE", "FUEL", 0.50))
        self.assertFalse(v1.has_vehicle)
        self.assertFalse(v1.confirmed)

        v2 = voter.update(PlateDetection("粤B12345", "BLUE", "FUEL", 0.85))
        self.assertTrue(v2.has_vehicle)
        self.assertIsNone(v2.plate)
        self.assertFalse(v2.confirmed)

        v3 = voter.update(PlateDetection("粤b12345", "BLUE", "FUEL", 0.91))
        self.assertTrue(v3.has_vehicle)
        self.assertTrue(v3.confirmed)
        self.assertEqual(v3.plate, "粤B12345")
        self.assertEqual(v3.plate_color, "BLUE")
        self.assertEqual(v3.vehicle_type, "FUEL")

        v4 = voter.update(None)
        self.assertTrue(v4.has_vehicle)
        self.assertEqual(v4.plate, "粤B12345")
        v5 = voter.update(None)
        self.assertTrue(v5.has_vehicle)
        self.assertEqual(v5.plate, "粤B12345")

        v6 = voter.update(None)
        self.assertFalse(v6.has_vehicle)
        self.assertTrue(v6.confirmed)
        self.assertIsNone(v6.plate)
        self.assertEqual(v6.vehicle_type, "UNKNOWN")


# =============================================================================
# 改造 5: 配置化管理测试
# =============================================================================
class TestConfigDefaults(unittest.TestCase):
    """Verify config defaults are production-ready."""

    def test_default_thresholds(self):
        from config import VisionConfig
        cfg = VisionConfig()
        self.assertAlmostEqual(cfg.vehicle_conf_threshold, 0.45)
        self.assertEqual(cfg.vehicle_min_box_px, 60)
        self.assertAlmostEqual(cfg.plate_det_conf_threshold, 0.55)
        self.assertAlmostEqual(cfg.plate_ocr_conf_threshold, 0.75)
        self.assertEqual(cfg.plate_min_width_px, 40)
        self.assertEqual(cfg.plate_min_height_px, 14)
        self.assertAlmostEqual(cfg.osd_mask_top_ratio, 0.08)
        self.assertEqual(cfg.plate_vote_window, 3)
        self.assertEqual(cfg.plate_vote_min_hits, 2)
        self.assertFalse(cfg.debug_draw_boxes)

    def test_env_override(self):
        import os
        from config import VisionConfig

        os.environ["VEHICLE_CONF_THRESHOLD"] = "0.60"
        os.environ["DEBUG_DRAW_BOXES"] = "true"
        try:
            cfg = VisionConfig.from_env()
            self.assertAlmostEqual(cfg.vehicle_conf_threshold, 0.60)
            self.assertTrue(cfg.debug_draw_boxes)
        finally:
            del os.environ["VEHICLE_CONF_THRESHOLD"]
            del os.environ["DEBUG_DRAW_BOXES"]


if __name__ == "__main__":
    unittest.main()
