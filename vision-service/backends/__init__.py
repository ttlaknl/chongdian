"""Inference backends package."""
from backends.openvino_backend import (
    OVModel,
    OpenVinoYoloDetector,
    OpenVinoPlateDetector,
    OpenVinoPlateRecognizer,
)

__all__ = [
    "OVModel",
    "OpenVinoYoloDetector",
    "OpenVinoPlateDetector",
    "OpenVinoPlateRecognizer",
]
