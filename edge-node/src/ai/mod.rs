pub mod detector;
pub mod model_repo;
pub mod plate_ocr;
pub mod rknn;

pub use detector::{
    DetectionResult, RealVisualInferenceEngine, RecordedDatasetReplayEngine,
    RecordedDatasetScenario,
};
pub use model_repo::{ModelMetadata, ModelRepository};
pub use plate_ocr::{
    box_iou, crop_and_align_plate, ctc_greedy_decode, format_plate_number,
    non_maximum_suppression, PlateDetection, PLATE_CHARS,
};
pub use rknn::{RknnConfig, RknnCoreMask, RknnInferenceEngine};
