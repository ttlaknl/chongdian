//! 边缘端模型仓库管理 (Model Repository)
//!
//! 负责车位检测与车牌 OCR 深度学习模型文件的扫描、元数据校验与热加载。

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelMetadata {
    pub name: String,
    pub filename: String,
    pub path: PathBuf,
    pub exists: bool,
    pub version: String,
    pub input_size: (u32, u32),
    pub format: String, // "RKNN" | "ONNX"
}

pub struct ModelRepository {
    pub base_dir: PathBuf,
    pub plate_detector: ModelMetadata,
    pub plate_recognizer: ModelMetadata,
}

impl Default for ModelRepository {
    fn default() -> Self {
        Self::from_dir(Path::new("models"))
    }
}

impl ModelRepository {
    pub fn from_env() -> Self {
        let dir = std::env::var("EDGE_MODEL_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("models"));
        Self::from_dir(&dir)
    }

    pub fn from_dir(dir: &Path) -> Self {
        let detector_path = dir.join("yolov8n_plate_rk3588.rknn");
        let recognizer_path = dir.join("lprnet_rk3588.rknn");

        let plate_detector = ModelMetadata {
            name: "YOLOv8n-Plate-Detector".to_string(),
            filename: "yolov8n_plate_rk3588.rknn".to_string(),
            exists: detector_path.exists(),
            path: detector_path,
            version: "v2.1.0".to_string(),
            input_size: (640, 640),
            format: "RKNN".to_string(),
        };

        let plate_recognizer = ModelMetadata {
            name: "LPRNet-Plate-Recognizer".to_string(),
            filename: "lprnet_rk3588.rknn".to_string(),
            exists: recognizer_path.exists(),
            path: recognizer_path,
            version: "v2.1.0".to_string(),
            input_size: (94, 24),
            format: "RKNN".to_string(),
        };

        Self {
            base_dir: dir.to_path_buf(),
            plate_detector,
            plate_recognizer,
        }
    }

    /// 重新扫描磁盘上的模型文件存在状态
    pub fn rescan(&mut self) {
        self.plate_detector.exists = self.plate_detector.path.exists();
        self.plate_recognizer.exists = self.plate_recognizer.path.exists();
    }

    /// 是否所有必须的端侧真实推理模型均已就绪
    pub fn is_ready(&self) -> bool {
        self.plate_detector.exists && self.plate_recognizer.exists
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_model_repo_defaults() {
        let repo = ModelRepository::default();
        assert_eq!(repo.plate_detector.filename, "yolov8n_plate_rk3588.rknn");
        assert_eq!(repo.plate_recognizer.filename, "lprnet_rk3588.rknn");
        assert_eq!(repo.plate_detector.input_size, (640, 640));
        assert_eq!(repo.plate_recognizer.input_size, (94, 24));
    }
}
