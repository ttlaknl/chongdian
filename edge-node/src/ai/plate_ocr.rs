//! 车牌检测与 OCR 字符识别算法工具集 (CTC 解码、NMS 与透视对齐变换)

/// 中国机动车号牌标准字符集 (68 类，对应 LPRNet 字典)
pub const PLATE_CHARS: &[&str] = &[
    "京", "沪", "津", "渝", "冀", "晋", "蒙", "辽", "吉", "黑", "苏", "浙", "皖", "闽", "赣", "鲁",
    "豫", "鄂", "湘", "粤", "桂", "琼", "川", "贵", "云", "藏", "陕", "甘", "青", "宁", "新",
    "0", "1", "2", "3", "4", "5", "6", "7", "8", "9",
    "A", "B", "C", "D", "E", "F", "G", "H", "J", "K", "L", "M", "N", "P", "Q", "R", "S", "T",
    "U", "V", "W", "X", "Y", "Z",
    "港", "澳", "学", "警", "领", "挂", "使", "_",
];

pub const BLANK_INDEX: usize = 67; // '_' 字符索引

/// 车牌检测目标结果
#[derive(Debug, Clone, PartialEq)]
pub struct PlateDetection {
    pub bbox: [f32; 4],           // [x, y, w, h] 归一化 (0.0 .. 1.0)
    pub confidence: f32,
    pub landmarks: [[f32; 2]; 4], // 左上、右上、右下、左下四顶点坐标 (0.0 .. 1.0)
}

/// CTC Greedy 解码算法
///
/// 沿着时域序列（Timesteps）取置信度最高的类别（ArgMax），
/// 合并连续重复字符并过滤 Blank 占位符。
pub fn ctc_greedy_decode(logits: &[f32], timesteps: usize, num_classes: usize) -> String {
    if logits.len() < timesteps * num_classes || num_classes == 0 {
        return String::new();
    }

    let mut raw_indices = Vec::with_capacity(timesteps);

    for t in 0..timesteps {
        let step_offset = t * num_classes;
        let step_slice = &logits[step_offset..step_offset + num_classes];

        let mut max_val = f32::NEG_INFINITY;
        let mut max_idx = 0usize;

        for (idx, &val) in step_slice.iter().enumerate() {
            if val > max_val {
                max_val = val;
                max_idx = idx;
            }
        }

        raw_indices.push(max_idx);
    }

    // 去除连续重复项与 Blank 占位符
    let mut plate_chars = Vec::new();
    let mut prev_idx = usize::MAX;

    for &idx in &raw_indices {
        if idx != prev_idx {
            if idx < PLATE_CHARS.len() && idx != BLANK_INDEX {
                plate_chars.push(PLATE_CHARS[idx]);
            }
            prev_idx = idx;
        }
    }

    // 格式化输出：例如 粤BD12345 -> 粤B·D12345
    let raw_plate = plate_chars.concat();
    format_plate_number(&raw_plate)
}

/// 规范化车牌号显示格式（如插入点间隔符）
pub fn format_plate_number(plate: &str) -> String {
    let chars: Vec<char> = plate.chars().collect();
    if chars.len() >= 7 {
        let province = chars[0];
        let letter = chars[1];
        let rest: String = chars[2..].iter().collect();
        format!("{}{}·{}", province, letter, rest)
    } else {
        plate.to_string()
    }
}

/// 计算两个 Bounding Box 的交并比 (IoU)
pub fn box_iou(box1: &[f32; 4], box2: &[f32; 4]) -> f32 {
    let x1_min = box1[0] - box1[2] / 2.0;
    let y1_min = box1[1] - box1[3] / 2.0;
    let x1_max = box1[0] + box1[2] / 2.0;
    let y1_max = box1[1] + box1[3] / 2.0;

    let x2_min = box2[0] - box2[2] / 2.0;
    let y2_min = box2[1] - box2[3] / 2.0;
    let x2_max = box2[0] + box2[2] / 2.0;
    let y2_max = box2[1] + box2[3] / 2.0;

    let inter_x_min = x1_min.max(x2_min);
    let inter_y_min = y1_min.max(y2_min);
    let inter_x_max = x1_max.min(x2_max);
    let inter_y_max = y1_max.min(y2_max);

    let inter_w = (inter_x_max - inter_x_min).max(0.0);
    let inter_h = (inter_y_max - inter_y_min).max(0.0);
    let inter_area = inter_w * inter_h;

    let area1 = box1[2] * box1[3];
    let area2 = box2[2] * box2[3];
    let union_area = area1 + area2 - inter_area;

    if union_area <= 1e-6 {
        0.0
    } else {
        inter_area / union_area
    }
}

/// 非极大值抑制 (NMS, Non-Maximum Suppression)
pub fn non_maximum_suppression(
    mut detections: Vec<PlateDetection>,
    iou_threshold: f32,
) -> Vec<PlateDetection> {
    detections.sort_by(|a, b| b.confidence.partial_cmp(&a.confidence).unwrap());

    let mut keep = Vec::new();
    let mut suppressed = vec![false; detections.len()];

    for i in 0..detections.len() {
        if suppressed[i] {
            continue;
        }
        keep.push(detections[i].clone());

        for j in (i + 1)..detections.len() {
            if suppressed[j] {
                continue;
            }
            if box_iou(&detections[i].bbox, &detections[j].bbox) > iou_threshold {
                suppressed[j] = true;
            }
        }
    }

    keep
}

/// 将四顶点倾斜车牌透视对齐并缩放裁剪为 LPRNet 输入规格 (94x24 RGB)
pub fn crop_and_align_plate(
    image: &[u8],
    width: u32,
    height: u32,
    landmarks: &[[f32; 2]; 4],
    target_w: u32,
    target_h: u32,
) -> Vec<u8> {
    let mut aligned = vec![0u8; (target_w * target_h * 3) as usize];

    // 双线性插值透视变换映射
    // landmarks: 0=左上, 1=右上, 2=右下, 3=左下 (归一化坐标)
    let p0 = [landmarks[0][0] * width as f32, landmarks[0][1] * height as f32];
    let p1 = [landmarks[1][0] * width as f32, landmarks[1][1] * height as f32];
    let p2 = [landmarks[2][0] * width as f32, landmarks[2][1] * height as f32];
    let p3 = [landmarks[3][0] * width as f32, landmarks[3][1] * height as f32];

    for ty in 0..target_h {
        let v = ty as f32 / target_h as f32;
        for tx in 0..target_w {
            let u = tx as f32 / target_w as f32;

            // 双线性插值点 (u, v)
            let src_x = (1.0 - u) * (1.0 - v) * p0[0]
                + u * (1.0 - v) * p1[0]
                + u * v * p2[0]
                + (1.0 - u) * v * p3[0];
            let src_y = (1.0 - u) * (1.0 - v) * p0[1]
                + u * (1.0 - v) * p1[1]
                + u * v * p2[1]
                + (1.0 - u) * v * p3[1];

            let sx = (src_x.round() as u32).min(width.saturating_sub(1));
            let sy = (src_y.round() as u32).min(height.saturating_sub(1));

            let src_idx = ((sy * width + sx) * 3) as usize;
            let dst_idx = ((ty * target_w + tx) * 3) as usize;

            if src_idx + 2 < image.len() && dst_idx + 2 < aligned.len() {
                aligned[dst_idx] = image[src_idx];
                aligned[dst_idx + 1] = image[src_idx + 1];
                aligned[dst_idx + 2] = image[src_idx + 2];
            }
        }
    }

    aligned
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ctc_greedy_decode_simple() {
        let timesteps = 9;
        let num_classes = 68;
        let mut logits = vec![0.0f32; timesteps * num_classes];

        // 模拟输出: 粤 B D 1 2 3 4 5
        let char_indices = [
            19, // 粤
            42, // B
            44, // D
            32, // 1
            33, // 2
            34, // 3
            35, // 4
            36, // 5
            BLANK_INDEX,
        ];

        for (t, &idx) in char_indices.iter().enumerate() {
            logits[t * num_classes + idx] = 10.0;
        }

        let plate = ctc_greedy_decode(&logits, timesteps, num_classes);
        assert_eq!(plate, "粤B·D12345");
    }

    #[test]
    fn test_box_iou_and_nms() {
        let b1 = [0.5, 0.5, 0.2, 0.2];
        let b2 = [0.5, 0.5, 0.2, 0.2];
        assert!((box_iou(&b1, &b2) - 1.0).abs() < 1e-4);

        let det1 = PlateDetection {
            bbox: [0.5, 0.5, 0.2, 0.2],
            confidence: 0.95,
            landmarks: [[0.4, 0.4], [0.6, 0.4], [0.6, 0.6], [0.4, 0.6]],
        };
        let det2 = PlateDetection {
            bbox: [0.51, 0.51, 0.2, 0.2],
            confidence: 0.80,
            landmarks: [[0.41, 0.41], [0.61, 0.41], [0.61, 0.61], [0.41, 0.61]],
        };

        let result = non_maximum_suppression(vec![det1.clone(), det2], 0.5);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].confidence, 0.95);
    }
}
