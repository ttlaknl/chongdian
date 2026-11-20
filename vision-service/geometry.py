"""Geometry and spatial containment module for parking spaces and vehicles.

Features:
- Coordinate conversion between normalized ([0.0, 1.0]) and pixel space
- 1-to-2 Ground Band (轮胎接地带) assignment algorithm preventing adjacent space bleed
- OSD mask checking for camera timestamp / channel overlays
"""

from __future__ import annotations

import json
from typing import Any, Sequence

import cv2
import numpy as np


def to_pixel_coords(
    points: Sequence[Sequence[float]] | Sequence[float],
    width: int,
    height: int,
) -> np.ndarray:
    """Convert points or bbox from normalized ([0.0, 1.0]) or raw pixel coordinates to int32 pixel coords.

    If all coordinate values are <= 1.0 (and float), they are treated as normalized.
    Otherwise, they are clamped to image bounds.
    """
    pts = np.array(points, dtype=np.float32)
    if pts.size == 0:
        return np.empty((0, 2), dtype=np.int32)

    # Check if normalized: all coordinates in [0.0, 1.0]
    if np.all(pts <= 1.0) and np.any(pts > 0.0):
        if pts.ndim == 1 and pts.shape[0] == 4:
            # bbox [x1, y1, x2, y2]
            pts[0] *= width
            pts[1] *= height
            pts[2] *= width
            pts[3] *= height
        elif pts.ndim == 2 and pts.shape[1] >= 2:
            pts[:, 0] *= width
            pts[:, 1] *= height

    return np.round(pts).astype(np.int32)


def to_normalized_coords(
    points: Sequence[Sequence[float]] | Sequence[float],
    width: int,
    height: int,
) -> np.ndarray:
    """Convert pixel coordinates to normalized [0.0, 1.0] range."""
    pts = np.array(points, dtype=np.float32)
    if pts.size == 0 or width <= 0 or height <= 0:
        return pts

    if np.any(pts > 1.0):
        if pts.ndim == 1 and pts.shape[0] == 4:
            pts[0] /= width
            pts[1] /= height
            pts[2] /= width
            pts[3] /= height
        elif pts.ndim == 2 and pts.shape[1] >= 2:
            pts[:, 0] /= width
            pts[:, 1] /= height

    return np.clip(pts, 0.0, 1.0)


def is_in_osd_mask(
    bbox: Sequence[int | float],
    frame_height: int,
    top_ratio: float = 0.08,
) -> bool:
    """Check if the center of a bounding box falls within the top OSD exclusion zone."""
    if frame_height <= 0 or top_ratio <= 0.0:
        return False
    if len(bbox) < 4:
        return False

    y1 = float(bbox[1])
    y2 = float(bbox[3])
    center_y = (y1 + y2) / 2.0
    return center_y < (frame_height * top_ratio)


def compute_ground_band(
    vehicle_bbox: Sequence[int],
    ground_band_ratio: float = 0.22,
) -> list[int]:
    """Extract bottom ground band (轮胎接地带) [gx1, gy1, gx2, gy2] from vehicle bbox."""
    x1, y1, x2, y2 = [int(v) for v in vehicle_bbox[:4]]
    bh = max(1, y2 - y1)
    band_height = max(1, int(bh * ground_band_ratio))
    gy1 = max(y1, y2 - band_height)
    return [x1, gy1, x2, y2]


def get_bottom_anchors(
    vehicle_bbox: Sequence[int],
) -> list[tuple[float, float]]:
    """Return 3 bottom anchor points at 25%, 50% (center), and 75% width, 96% height."""
    x1, y1, x2, y2 = [float(v) for v in vehicle_bbox[:4]]
    bw = max(1.0, x2 - x1)
    bh = max(1.0, y2 - y1)
    y_anchor = y1 + 0.96 * bh
    return [
        (x1 + 0.25 * bw, y_anchor),
        (x1 + 0.50 * bw, y_anchor),
        (x1 + 0.75 * bw, y_anchor),
    ]


def compute_polygon_box_intersection_area(
    polygon: np.ndarray,
    box: Sequence[int],
) -> float:
    """Compute exact pixel intersection area between an arbitrary polygon and a bounding box."""
    bx1, by1, bx2, by2 = [int(v) for v in box[:4]]
    bw = max(0, bx2 - bx1)
    bh = max(0, by2 - by1)
    if bw == 0 or bh == 0:
        return 0.0

    poly = np.asarray(polygon, dtype=np.int32)
    if len(poly) < 3:
        return 0.0

    mask = np.zeros((bh, bw), dtype=np.uint8)
    shifted_poly = poly.copy()
    shifted_poly[:, 0] -= bx1
    shifted_poly[:, 1] -= by1

    cv2.fillPoly(mask, [shifted_poly], 1)
    return float(np.sum(mask))


def assign_vehicle_to_space(
    vehicle_bbox: Sequence[int],
    spaces_polygons: dict[str, np.ndarray],
    ground_band_ratio: float = 0.22,
    min_ioa: float = 0.35,
) -> str | None:
    """1 拖 2 轮胎接地带归属算法: Assign a vehicle to the best matching parking space.

    Steps:
    1. Extract vehicle bottom ground band (ground_band_ratio, default 22%).
    2. Compute Intersection-over-Area (IoA = inter_area / ground_box_area) with each space polygon.
    3. Evaluate 3 bottom anchor points (25%, 50%, 75% width, 96% height).
    4. Only valid if IoA >= min_ioa AND (center anchor inside OR >= 2 anchors inside).
    5. If multiple spaces match, assign to space with strictly largest IoA.
    """
    if not spaces_polygons:
        return None

    gbox = compute_ground_band(vehicle_bbox, ground_band_ratio)
    gx1, gy1, gx2, gy2 = gbox
    g_area = max(1.0, float((gx2 - gx1) * (gy2 - gy1)))

    anchors = get_bottom_anchors(vehicle_bbox)
    pt_left, pt_center, pt_right = anchors

    best_space_id: str | None = None
    best_ioa: float = -1.0

    for space_id, poly in spaces_polygons.items():
        if poly is None or len(poly) < 3:
            continue

        poly_arr = np.asarray(poly, dtype=np.int32)

        # 1. IoA calculation
        inter_area = compute_polygon_box_intersection_area(poly_arr, gbox)
        ioa = inter_area / g_area

        if ioa < min_ioa:
            continue

        # 2. Bottom 3 anchors test
        hit_center = cv2.pointPolygonTest(poly_arr, pt_center, False) >= 0
        hit_left = cv2.pointPolygonTest(poly_arr, pt_left, False) >= 0
        hit_right = cv2.pointPolygonTest(poly_arr, pt_right, False) >= 0
        hit_count = int(hit_left) + int(hit_center) + int(hit_right)

        if not (hit_center or hit_count >= 2):
            continue

        if ioa > best_ioa:
            best_ioa = ioa
            best_space_id = space_id

    return best_space_id


def parse_spaces_from_roi_json(
    roi_json: str | dict[str, Any] | None,
    default_space_id: str,
    width: int,
    height: int,
) -> dict[str, np.ndarray]:
    """Parse single or multi-space (1 拖 2) polygons from roi_json configuration.

    Supports:
    - Multi-space dict: {"spaces": {"A-01": {"polygon": [...]}, "A-02": {"polygon": [...]}}}
    - Single-space dict: {"polygon": [[x, y], ...]} or {"points": [[x, y], ...]}
    - Rect format: {"x": 0.1, "y": 0.1, "w": 0.4, "h": 0.8}
    - None / empty: returns {default_space_id: full_frame_rect}
    """
    if not roi_json:
        full_rect = np.array([[0, 0], [width, 0], [width, height], [0, height]], dtype=np.int32)
        return {default_space_id: full_rect}

    try:
        data = json.loads(roi_json) if isinstance(roi_json, str) else roi_json
    except Exception:
        full_rect = np.array([[0, 0], [width, 0], [width, height], [0, height]], dtype=np.int32)
        return {default_space_id: full_rect}

    if not isinstance(data, dict):
        full_rect = np.array([[0, 0], [width, 0], [width, height], [0, height]], dtype=np.int32)
        return {default_space_id: full_rect}

    # Case 1: Multi-space configuration (1 拖 2)
    if "spaces" in data and isinstance(data["spaces"], dict):
        result: dict[str, np.ndarray] = {}
        for sp_id, sp_cfg in data["spaces"].items():
            parsed_poly = _parse_single_polygon(sp_cfg, width, height)
            if parsed_poly is not None:
                result[sp_id] = parsed_poly
        if result:
            return result

    # Case 2: Single space
    single = _parse_single_polygon(data, width, height)
    if single is not None:
        return {default_space_id: single}

    full_rect = np.array([[0, 0], [width, 0], [width, height], [0, height]], dtype=np.int32)
    return {default_space_id: full_rect}


def _parse_single_polygon(data: Any, width: int, height: int) -> np.ndarray | None:
    """Helper to parse a single polygon or box dict into int32 ndarray."""
    if not isinstance(data, dict):
        return None

    poly = data.get("polygon") or data.get("points")
    if isinstance(poly, list) and len(poly) >= 3:
        pts = to_pixel_coords(poly, width, height)
        return pts

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

    return None


def order_plate_corners(pts: np.ndarray | Sequence[Sequence[float]]) -> np.ndarray | None:
    """Order 4 arbitrary 2D corner coordinates into [top_left, top_right, bottom_right, bottom_left].

    Validates that the quadrilateral is convex, non-degenerate, and has sufficient area (>= 20px^2).
    Returns:
        np.ndarray of shape (4, 2) with dtype float32 ordered [TL, TR, BR, BL],
        or None if points are degenerate (non-convex, collinear, or area < 20).
    """
    if pts is None:
        return None

    pts_arr = np.asarray(pts, dtype=np.float32)
    if pts_arr.shape != (4, 2) or not np.all(np.isfinite(pts_arr)):
        return None

    # Sort points clockwise around centroid to get perimeter order
    cx = float(np.mean(pts_arr[:, 0]))
    cy = float(np.mean(pts_arr[:, 1]))
    angles = np.arctan2(pts_arr[:, 1] - cy, pts_arr[:, 0] - cx)
    sort_idx = np.argsort(angles)
    pts_cw = pts_arr[sort_idx]

    # Check minimum perimeter contour area
    area = cv2.contourArea(pts_cw)
    if area < 20.0:
        return None

    # Verify convexity: all adjacent cross products must have the same non-zero sign
    edges = np.roll(pts_cw, -1, axis=0) - pts_cw
    crosses = edges[:, 0] * np.roll(edges[:, 1], -1) - edges[:, 1] * np.roll(edges[:, 0], -1)

    all_pos = np.all(crosses > 1e-4)
    all_neg = np.all(crosses < -1e-4)
    if not (all_pos or all_neg):
        return None

    # If counter-clockwise, reverse to make strictly clockwise
    if all_neg:
        pts_cw = pts_cw[::-1]

    # Top-left has minimum sum (x + y)
    s = pts_cw.sum(axis=1)
    tl_idx = int(np.argmin(s))
    ordered = np.roll(pts_cw, -tl_idx, axis=0)

    # Sanity check: TR must be to the right of TL, BL must be below TL
    if ordered[1, 0] < ordered[0, 0] or ordered[3, 1] < ordered[0, 1]:
        # Fallback to sum/diff ordering
        rect = np.zeros((4, 2), dtype=np.float32)
        s = pts_arr.sum(axis=1)
        rect[0] = pts_arr[np.argmin(s)]  # TL
        rect[2] = pts_arr[np.argmax(s)]  # BR
        diff = pts_arr[:, 1] - pts_arr[:, 0]  # y - x
        rect[1] = pts_arr[np.argmin(diff)]  # TR
        rect[3] = pts_arr[np.argmax(diff)]  # BL
        ordered = rect

    return ordered


def split_and_merge_double_plate(img: np.ndarray) -> np.ndarray:
    """Split double-layer plate horizontally and merge side-by-side."""
    if img is None or img.size == 0:
        return img
    h, w = img.shape[:2]
    img_upper = img[0 : int(5 / 12 * h), :]
    img_lower = img[int(1 / 3 * h) :, :]
    img_upper = cv2.resize(img_upper, (img_lower.shape[1], img_lower.shape[0]))
    return np.hstack((img_upper, img_lower))


def warp_plate_perspective(
    image: np.ndarray,
    corners: np.ndarray | Sequence[Sequence[float]],
    is_double_layer: bool = False,
    target_single: tuple[int, int] = (168, 48),
    target_double: tuple[int, int] = (168, 96),
) -> np.ndarray:
    """Warp plate region to flat rectangle using 4 perspective corners.

    Args:
        image: Source image (BGR ndarray).
        corners: 4 2D corner coordinates of the plate.
        is_double_layer: Whether the plate has two text rows (double yellow / tractor).
        target_single: (width, height) for single-layer output (default 168x48).
        target_double: (width, height) intermediate rectification for double-layer (default 168x96).

    Returns:
        Rectified BGR plate image with size matching target_single (168, 48).
    """
    if image is None or image.size == 0:
        return image

    ordered = order_plate_corners(corners)
    target_w, target_h = target_double if is_double_layer else target_single

    if ordered is not None:
        src_pts = ordered.astype(np.float32)
        dst_pts = np.float32(
            [
                [0, 0],
                [target_w - 1, 0],
                [target_w - 1, target_h - 1],
                [0, target_h - 1],
            ]
        )
        try:
            m = cv2.getPerspectiveTransform(src_pts, dst_pts)
            warped = cv2.warpPerspective(
                image,
                m,
                (target_w, target_h),
                flags=cv2.INTER_CUBIC,
                borderMode=cv2.BORDER_REPLICATE,
            )
            if is_double_layer:
                merged = split_and_merge_double_plate(warped)
                return cv2.resize(merged, target_single, interpolation=cv2.INTER_LINEAR)
            return warped
        except Exception:
            pass

    # Fallback if corners are invalid or warp failed: crop bounding box
    pts_arr = np.asarray(corners, dtype=np.float32)
    ih, iw = image.shape[:2]
    if pts_arr.size >= 8:
        x1 = max(0, min(iw - 1, int(np.floor(np.min(pts_arr[:, 0])))))
        y1 = max(0, min(ih - 1, int(np.floor(np.min(pts_arr[:, 1])))))
        x2 = max(x1 + 1, min(iw, int(np.ceil(np.max(pts_arr[:, 0])))))
        y2 = max(y1 + 1, min(ih, int(np.ceil(np.max(pts_arr[:, 1])))))
        crop = image[y1:y2, x1:x2]
    else:
        crop = image

    if is_double_layer:
        crop = split_and_merge_double_plate(crop)
    return cv2.resize(crop, target_single, interpolation=cv2.INTER_LINEAR)

