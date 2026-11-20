"""Unit tests for geometry, 1-to-2 ground band containment, plate postprocessing, and gating."""

from __future__ import annotations

import sys
import unittest
from pathlib import Path
from unittest.mock import MagicMock

import numpy as np

VISION_DIR = Path(__file__).resolve().parents[1]
if str(VISION_DIR) not in sys.path:
    sys.path.insert(0, str(VISION_DIR))

from geometry import (
    assign_vehicle_to_space,
    compute_ground_band,
    compute_polygon_box_intersection_area,
    get_bottom_anchors,
    is_in_osd_mask,
    parse_spaces_from_roi_json,
    to_normalized_coords,
    to_pixel_coords,
)
from plate_postprocess import (
    is_valid_chinese_plate,
    is_valid_plate_geometry,
    normalize_plate_text,
)


class TestPlateRegexAndPostprocess(unittest.TestCase):
    """Test strict Chinese license plate validation and geometry checks."""

    def test_must_reject_invalid_plates(self):
        """必须拒绝的非法车牌格式."""
        invalids = [
            "青11H135",       # 第 2 位是数字 1
            "2026/11/20",     # 时间戳
            "20261120",       # 纯日期数字
            "测试1",          # 通道 OSD 文本
            "鄂112345",       # 第 2 位是数字 1
            "京I12345",       # 包含禁用字母 I
            "京O12345",       # 包含禁用字母 O
            "AB12345",        # 纯英文字头
            "京A12",          # 过短
            "京A123456789",   # 过长
            "",               # 空字符串
            None,             # 空对象
        ]
        for p in invalids:
            self.assertFalse(
                is_valid_chinese_plate(p),
                f"Plate '{p}' should be REJECTED but was accepted",
            )

    def test_must_allow_valid_plates(self):
        """必须放行的合法车牌格式."""
        valids = [
            "鄂A12345",       # 普通民用 7 位蓝牌
            "鄂AD12345",      # 新能源 8 位绿牌 (小型: D 靠前)
            "鄂A12345D",      # 新能源 8 位绿牌 (大型: D 靠后)
            "鄂A1234警",      # 警车号牌 (末位 警)
            "京A1234挂",      # 挂车号牌 (末位 挂)
            "VA12345",        # 2012 式新军牌 (战区字母 V + 字母 A + 5位数字)
            "WJ鄂12345",      # 武警号牌 (WJ + 省份简称 + 5位数字)
            "WJ12345",        # 武警号牌 (WJ + 5位数字)
        ]
        for p in valids:
            self.assertTrue(
                is_valid_chinese_plate(p),
                f"Plate '{p}' should be ACCEPTED but was rejected",
            )

    def test_plate_text_normalization(self):
        self.assertEqual(normalize_plate_text(" 鄂·a12345 "), "鄂A12345")
        self.assertEqual(normalize_plate_text("粤B-88888"), "粤B88888")
        self.assertEqual(normalize_plate_text(""), "")
        self.assertEqual(normalize_plate_text(None), "")

    def test_plate_geometry_validation(self):
        # 合法单层车牌: 140x40 -> AR = 3.5 (在 1.8~5.5 内)
        self.assertTrue(is_valid_plate_geometry([100, 100, 240, 140], is_double_layer=False))

        # 尺寸过小: 宽 30px < 40px
        self.assertFalse(is_valid_plate_geometry([100, 100, 130, 120], is_double_layer=False))

        # 宽高比过大 (AR = 6.0 > 5.5)
        self.assertFalse(is_valid_plate_geometry([100, 100, 340, 140], is_double_layer=False))

        # 宽高比过小单层 (AR = 1.2 < 1.8)
        self.assertFalse(is_valid_plate_geometry([100, 100, 160, 150], is_double_layer=False))

        # 合法双层车牌 (AR = 2.0 在 1.1~3.5 内)
        self.assertTrue(is_valid_plate_geometry([100, 100, 200, 150], is_double_layer=True))


class TestGeometryAndGroundBand(unittest.TestCase):
    """Test coordinate conversion, OSD mask, and 1-to-2 ground band space assignment."""

    def test_coordinate_conversion(self):
        # 归一化转像素
        norm_pts = [[0.1, 0.2], [0.5, 0.6]]
        pix = to_pixel_coords(norm_pts, 1000, 500)
        np.testing.assert_array_equal(pix, [[100, 100], [500, 300]])

        # 历史像素坐标保持不变
        raw_pix = [[100, 200], [300, 400]]
        pix2 = to_pixel_coords(raw_pix, 1000, 500)
        np.testing.assert_array_equal(pix2, raw_pix)

        # 像素转归一化
        norm2 = to_normalized_coords([[500, 250]], 1000, 500)
        np.testing.assert_almost_equal(norm2[0], [0.5, 0.5])

    def test_osd_mask_check(self):
        # 画面高 1000, top_ratio=0.08 -> cutoff=80
        # 候选框位于顶部 OSD 区域: center_y = 40 < 80 -> True
        self.assertTrue(is_in_osd_mask([10, 10, 200, 70], frame_height=1000, top_ratio=0.08))

        # 正常车牌框: center_y = 500 > 80 -> False
        self.assertFalse(is_in_osd_mask([100, 480, 240, 520], frame_height=1000, top_ratio=0.08))

    def test_one_to_two_ground_band_prevents_adjacent_space_bleed(self):
        """1 拖 2 核心测试: 左车车顶侵入右车位多边形，但轮胎接地带在左车位内，准确归属左车位而非右车位."""
        # 定义相邻的两个车位多边形:
        # 左车位 A-01: x in [0, 500], y in [100, 800]
        # 右车位 A-02: x in [500, 1000], y in [100, 800]
        spaces = {
            "A-01": np.array([[0, 100], [500, 100], [500, 800], [0, 800]], dtype=np.int32),
            "A-02": np.array([[500, 100], [1000, 100], [1000, 800], [500, 800]], dtype=np.int32),
        }

        # 构造一辆停在 A-01 的车辆:
        # 车辆整体框: x1=80, y1=150, x2=580, y2=750 (宽度 500px, 高度 600px)
        # 注意: x2=580 说明右后视镜或车顶斜向侵入了 A-02 多边形 80px!
        vehicle_bbox = [80, 150, 580, 750]

        # 轮胎接地带: 底部 22% 区域 (高度 600*0.22 = 132px, y: 618~750)
        # 接地带中: x 在 [80, 500] 属于 A-01 (420px), x 在 [500, 580] 属于 A-02 (80px)
        # IoA 与 A-01 = 420 / 500 = 0.84 (>= 0.35)
        # IoA 与 A-02 = 80 / 500 = 0.16 (< 0.35)
        # 底边中心锚点: x = 80 + 0.5*500 = 330, 落在 A-01 内 (不在 A-02 内)
        assigned = assign_vehicle_to_space(
            vehicle_bbox,
            spaces,
            ground_band_ratio=0.22,
            min_ioa=0.35,
        )

        # 必须严格归属左车位 A-01，绝不串位到 A-02!
        self.assertEqual(assigned, "A-01")

    def test_vehicle_outside_all_spaces_returns_none(self):
        """车辆完全在车道通道上，接地带不满足任何车位."""
        spaces = {
            "A-01": np.array([[0, 100], [400, 100], [400, 600], [0, 600]], dtype=np.int32),
            "A-02": np.array([[500, 100], [900, 100], [900, 600], [500, 600]], dtype=np.int32),
        }
        # 车辆停在中间通道 x in [420, 480]
        vehicle_bbox = [420, 200, 480, 500]
        assigned = assign_vehicle_to_space(vehicle_bbox, spaces, ground_band_ratio=0.22, min_ioa=0.35)
        self.assertIsNone(assigned)


class TestNoVehicleGating(unittest.TestCase):
    """Test vehicle-first gating: zero vehicles -> zero plate OCR calls."""

    def test_mock_no_vehicle_skips_plate_ocr(self):
        from ai_engine import UnifiedAiEngine
        from config import VisionConfig

        engine = UnifiedAiEngine.__new__(UnifiedAiEngine)
        engine.device = "cpu"
        engine.yolo_model = MagicMock()
        engine.plate_detector = MagicMock()
        engine.plate_rec_model = MagicMock()
        engine.hyperlpr_catcher = MagicMock()
        engine._init_success = True
        engine.config = VisionConfig()

        # Mock YOLO 检出 0 目标
        mock_result = MagicMock()
        mock_result.boxes = []
        engine.yolo_model.return_value = [mock_result]

        frame = np.zeros((720, 1280, 3), dtype=np.uint8)
        res = engine.evaluate_space(frame, "A-01")

        # 结果应判定为无车待机
        self.assertFalse(res.has_vehicle)
        self.assertIsNone(res.plate)
        self.assertEqual(res.category_label, "空闲待机")

        # 车牌检测与 OCR 绝不得被调用！
        engine.plate_detector.assert_not_called()
        engine.hyperlpr_catcher.assert_not_called()


if __name__ == "__main__":
    unittest.main()
