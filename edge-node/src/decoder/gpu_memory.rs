//! GPU/DMA-BUF 显存与像素格式转换工具 (NV12 -> RGB24)

/// 将 NV12 (YUV420SP) 格式数据快速转换为 RGB24 格式
///
/// NV12 格式：
/// - Y 平面：连续的 `width * height` 字节
/// - UV 平面：UV 交错存储，宽度为 `width`，高度为 `height / 2`
pub fn nv12_to_rgb24(
    y_plane: &[u8],
    uv_plane: &[u8],
    width: u32,
    height: u32,
    y_stride: u32,
    uv_stride: u32,
) -> Vec<u8> {
    let w = width as usize;
    let h = height as usize;
    let y_str = y_stride as usize;
    let uv_str = uv_stride as usize;

    let mut rgb = vec![0u8; w * h * 3];

    for row in 0..h {
        let y_row_start = row * y_str;
        let uv_row_start = (row / 2) * uv_str;
        let rgb_row_start = row * w * 3;

        for col in 0..w {
            let y_idx = y_row_start + col;
            let y_val = if y_idx < y_plane.len() {
                y_plane[y_idx] as f32
            } else {
                0.0
            };

            let uv_col = (col / 2) * 2;
            let u_idx = uv_row_start + uv_col;
            let v_idx = u_idx + 1;

            let u_val = if u_idx < uv_plane.len() {
                uv_plane[u_idx] as f32 - 128.0
            } else {
                0.0
            };
            let v_val = if v_idx < uv_plane.len() {
                uv_plane[v_idx] as f32 - 128.0
            } else {
                0.0
            };

            // ITU-R BT.601 标清/高清标准 YUV 转 RGB 矩阵
            let r = y_val + 1.402 * v_val;
            let g = y_val - 0.344136 * u_val - 0.714136 * v_val;
            let b = y_val + 1.772 * u_val;

            let out_idx = rgb_row_start + col * 3;
            if out_idx + 2 < rgb.len() {
                rgb[out_idx] = r.clamp(0.0, 255.0) as u8;
                rgb[out_idx + 1] = g.clamp(0.0, 255.0) as u8;
                rgb[out_idx + 2] = b.clamp(0.0, 255.0) as u8;
            }
        }
    }

    rgb
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_nv12_to_rgb24_basic() {
        let width = 4;
        let height = 4;
        let y_plane = vec![128u8; 16];
        let uv_plane = vec![128u8; 8];

        let rgb = nv12_to_rgb24(&y_plane, &uv_plane, width, height, width, width);
        assert_eq!(rgb.len(), (width * height * 3) as usize);
        // 灰色 (Y=128, U=128, V=128) 应该转换为约 (128, 128, 128)
        assert_eq!(rgb[0], 128);
        assert_eq!(rgb[1], 128);
        assert_eq!(rgb[2], 128);
    }
}
