# 边缘端深度学习模型库 (Edge AI Models)

本目录存放基于瑞芯微 RK3588 NPU (6 TOPS 三核算力) 加速的深度学习视觉检测与字符识别模型。

---

## 1. 模型资产清单

| 模型角色 | 默认文件名 | 架构选型 | 输入张量规格 | 量化精度 | 说明 |
|---|---|---|---|---|---|
| **车牌检测与角点定位** | `yolov8n_plate_rk3588.rknn` | YOLOv8n-plate | `[1, 640, 640, 3]` (RGB/NHWC) | INT8 | 输出车牌 BBox 及 4 角点坐标 (用于大仰角俯视透视校正) |
| **车牌字符识别 OCR** | `lprnet_rk3588.rknn` | LPRNet (纯卷积无RNN) | `[1, 24, 94, 3]` (RGB/NHWC) | INT8 | 输出 18 时间步 × 68 类字符 Logits，经 CTC Greedy 解码为真实车牌号 |

---

## 2. 模型转换与部署流程 (基于 RKNN-Toolkit2)

### 2.1 转换 YOLOv8n 车牌检测模型
```python
from rknn.api import RKNN

rknn = RKNN(verbose=True)
rknn.config(
    mean_values=[[0, 0, 0]],
    std_values=[[255, 255, 255]],
    target_platform='rk3588'
)

# 加载 ONNX 模型
rknn.load_onnx(model='yolov8n_plate.onnx')

# 构建 INT8 量化模型（使用校准数据集）
rknn.build(do_quantization=True, dataset='dataset.txt')

# 导出为 RK3588 专用二进制
rknn.export_rknn('yolov8n_plate_rk3588.rknn')
```

### 2.2 转换 LPRNet 车牌识别模型
```python
from rknn.api import RKNN

rknn = RKNN(verbose=True)
rknn.config(
    mean_values=[[127.5, 127.5, 127.5]],
    std_values=[[127.5, 127.5, 127.5]],
    target_platform='rk3588'
)

rknn.load_onnx(model='lprnet.onnx')
rknn.build(do_quantization=True, dataset='dataset_lpr.txt')
rknn.export_rknn('lprnet_rk3588.rknn')
```

---

## 3. 环境变量配置

边缘工控机启动时可通过以下环境变量覆盖默认配置：

- `EDGE_MODEL_DIR`: 模型文件根目录（默认为 `models` 或 `/opt/chongdian/models`）
- `EDGE_RKNN_CORE_MASK`: NPU 核心分配掩码（`1`=Core0, `2`=Core1, `4`=Core2, `3`=Core0+1, `7`=三核 6 TOPS 全并发，默认 `7`）
- `EDGE_RKNN_CONF_THRESHOLD`: 车牌检测置信度阈值（默认 `0.60`）

---

## 4. 生产部署与 OTA

由于 `.rknn` 二进制权重文件通常在 2MB ~ 5MB 之间，生产部署时应通过 OTA 下发或 Docker volume 挂载到容器中：

```bash
docker run -d \
  -v /opt/chongdian/models:/opt/chongdian/models:ro \
  --device /dev/rknpu0:/dev/rknpu0 \
  --device /dev/mpp_service:/dev/mpp_service \
  chongdian-edge:latest
```
