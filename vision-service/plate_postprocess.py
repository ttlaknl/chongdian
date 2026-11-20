"""License plate normalization, regex validation, and geometric filtering module."""

from __future__ import annotations

import re
from typing import Sequence

import cv2
import numpy as np

PROVINCE_CHARS = "京津沪渝冀豫云辽黑湘皖鲁新苏浙赣鄂桂甘晋蒙陕吉闽贵粤青藏川宁琼"

# 1. 7 位普通民用蓝/黄/黑/白及挂/学/警/港澳/使领车牌
STANDARD_PLATE_RE = re.compile(
    rf"^[{PROVINCE_CHARS}][A-HJ-NP-Z][A-HJ-NP-Z0-9]{{4}}[A-HJ-NP-Z0-9挂学警港澳领使]$"
)

# 2. 8 位新能源绿牌 (含小型 D/F 靠前与大型 D/F 靠后)
NEV_PLATE_RE = re.compile(
    rf"^[{PROVINCE_CHARS}][A-HJ-NP-Z](?:[DF][A-HJ-NP-Z0-9]\d{{4}}|\d{{5}}[DF])$"
)

# 3. 2012 式新军牌
MILITARY_PLATE_RE = re.compile(r"^[VKHBSGCEJLNQZ][A-HJ-NP-Z]\d{5}$")

# 4. 武警牌 (WJ + 可选省份 + 5 位字母/数字)
WJ_PLATE_RE = re.compile(
    rf"^WJ[{PROVINCE_CHARS}]?[0-9A-HJ-NP-Z]{{5}}$"
)


def normalize_plate_text(text: str | None) -> str:
    """Normalize raw OCR plate text by removing whitespace, dots, and converting to uppercase."""
    if not text:
        return ""
    return re.sub(r"[\s·\-•._]+", "", str(text).strip()).upper()


def is_valid_chinese_plate(text: str | None) -> bool:
    """Validate if the string matches a legitimate Chinese license plate format.

    Rejects invalid formats like '青11H135', '2026/11/20', '测试1', '鄂112345', '京I12345'.
    Allows 7-char civilian, 8-char NEV, military, and armed police plates.
    """
    plate = normalize_plate_text(text)
    if len(plate) < 7 or len(plate) > 8:
        return False

    return bool(
        STANDARD_PLATE_RE.match(plate)
        or NEV_PLATE_RE.match(plate)
        or MILITARY_PLATE_RE.match(plate)
        or WJ_PLATE_RE.match(plate)
    )


def is_valid_plate_geometry(
    bbox: Sequence[int | float],
    is_double_layer: bool = False,
    min_width: int = 40,
    min_height: int = 14,
    single_ar_range: tuple[float, float] = (1.8, 5.5),
    double_ar_range: tuple[float, float] = (1.1, 3.5),
) -> bool:
    """Validate if plate bounding box meets geometric constraints.

    - width >= min_width (default 40px)
    - height >= min_height (default 14px)
    - aspect ratio (width / height):
        * single-layer: [1.8, 5.5]
        * double-layer: [1.1, 3.5]
    """
    if len(bbox) < 4:
        return False

    x1, y1, x2, y2 = bbox[:4]
    w = float(x2 - x1)
    h = float(y2 - y1)

    if w < min_width or h < min_height:
        return False

    ar = w / max(1.0, h)
    if is_double_layer:
        return double_ar_range[0] <= ar <= double_ar_range[1]
    else:
        return single_ar_range[0] <= ar <= single_ar_range[1]


def assess_plate_illumination(
    plate_img: np.ndarray,
    low_luma_thresh: float = 75.0,
    low_contrast_thresh: float = 28.0,
    high_luma_ratio_thresh: float = 0.15,
) -> tuple[float, float, float, bool]:
    """Assess lighting quality and contrast of a cropped plate image.

    Args:
        plate_img: BGR image crop of the plate.
        low_luma_thresh: Threshold below which plate is considered dark/underexposed (default 75).
        low_contrast_thresh: Threshold below which plate has poor dynamic range (default 28).
        high_luma_ratio_thresh: Proportion of specular highlights (>=240) triggering overexposure (default 0.15).

    Returns:
        (mean_luma, std_luma, laplacian_var, needs_enhance)
    """
    if plate_img is None or plate_img.size == 0:
        return 0.0, 0.0, 0.0, False

    if plate_img.ndim == 3 and plate_img.shape[2] == 3:
        gray = cv2.cvtColor(plate_img, cv2.COLOR_BGR2GRAY)
    else:
        gray = plate_img

    mean_luma = float(np.mean(gray))
    std_luma = float(np.std(gray))
    laplacian_var = float(cv2.Laplacian(gray, cv2.CV_64F).var())

    high_luma_ratio = float(np.mean(gray >= 240))

    needs_enhance = (
        mean_luma < low_luma_thresh
        or std_luma < low_contrast_thresh
        or high_luma_ratio >= high_luma_ratio_thresh
    )

    return mean_luma, std_luma, laplacian_var, needs_enhance


def enhance_backlit_plate(
    plate_img: np.ndarray,
    clip_limit: float = 2.5,
    tile_grid_size: tuple[int, int] = (4, 4),
    gamma: float = 0.75,
) -> np.ndarray:
    """Adaptive CLAHE + Gamma enhancement in LAB color space for backlit/low-light plates.

    Operates purely on the L (luminance) channel while preserving A and B (chroma) channels,
    preventing vehicle plate color distortion (blue/green/yellow).

    Args:
        plate_img: BGR image crop of the plate.
        clip_limit: CLAHE contrast clip limit (default 2.5).
        tile_grid_size: CLAHE grid size (default 4x4 for plate proportions).
        gamma: Gamma power value for dark images (default 0.75).

    Returns:
        Enhanced BGR image with identical dimensions and data type.
    """
    if plate_img is None or plate_img.size == 0:
        return plate_img

    if plate_img.ndim != 3 or plate_img.shape[2] != 3:
        return plate_img

    # 1. Convert to LAB color space
    lab = cv2.cvtColor(plate_img, cv2.COLOR_BGR2LAB)
    l_chan, a_chan, b_chan = cv2.split(lab)

    # 2. CLAHE on L channel
    clahe = cv2.createCLAHE(clipLimit=clip_limit, tileGridSize=tile_grid_size)
    l_clahe = clahe.apply(l_chan)

    # 3. Lightweight gamma correction if mean luminance is dark
    mean_l = float(np.mean(l_clahe))
    if mean_l < 128.0 and gamma > 0.0 and gamma != 1.0:
        inv_gamma = gamma
        table = np.array([((i / 255.0) ** inv_gamma) * 255 for i in range(256)]).astype(np.uint8)
        l_enhanced = cv2.LUT(l_clahe, table)
    else:
        l_enhanced = l_clahe

    # 4. Merge back with untouched chroma channels A and B
    enhanced_lab = cv2.merge([l_enhanced, a_chan, b_chan])
    enhanced_bgr = cv2.cvtColor(enhanced_lab, cv2.COLOR_LAB2BGR)
    return enhanced_bgr

