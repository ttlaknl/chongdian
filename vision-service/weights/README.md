# 模型权重与 OpenVINO / ONNX 导出说明

本目录存放 `vision-service` 边缘侧推理所需的模型权重。

生产环境运行已全面**去除 PyTorch/Ultralytics 依赖**，采用 **OpenVINO (IR / ONNX)** 统一运行时，支持 Intel i3 CPU 及 Intel 核显 (iGPU) 加速。

---

## 1. 预期文件清单

| 文件名 | 模型类型 | 输入尺寸 | 目标设备 | 说明 |
|---|---|---|---|---|
| `yolov8n.onnx` / `yolov8n.xml` + `.bin` | 车辆与障碍物检测 | `1x3x640x640` | `AUTO:GPU,CPU` | 检出 `car`, `bus`, `truck`, `bicycle`, `motorcycle`, `chair`, `bench` 等 |
| `yolo26s-plate-detect.onnx` / `.xml` + `.bin` | 车牌 4 点透视检测 | `1x3x640x640` | `AUTO:GPU,CPU` | 检出车牌目标框及 4 个角点坐标 |
| `plate_rec_color.onnx` / `.xml` + `.bin` | 车牌 OCR + 颜色分类 | `1x3x48x168` | `AUTO:GPU,CPU` | 输出 `char_logits` (字符序列) 与 `color_logits` (5 种颜色分类) |

---

## 2. 离线导出命令（开发环境执行）

在具备 PyTorch 和 Ultralytics 的开发环境中执行：

```bash
# 执行一键导出脚本（将 .pt / .pth 转换为 ONNX 并自动生成 OpenVINO FP16 IR）
python vision-service/scripts/export_models.py --weights-dir vision-service/weights --output-dir vision-service/weights
```

生产容器构建与运行时无需安装 PyTorch，仅依赖轻量级 `openvino` 运行时库。
