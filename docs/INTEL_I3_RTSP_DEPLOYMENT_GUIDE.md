# Intel i3 (16GB RAM) 裸机 RTSP 实景部署与车牌图片模拟测试完全指南

本指南面向**全新未安装系统的 Intel i3 + 16GB 内存（x86_64）边缘工控机/主机**，指导从零安装操作系统、部署全套云边与 RTSP 视觉识别服务、接入真实 RTSP 摄像头、用图片模拟蓝牌燃油车与绿牌新能源车验证车牌号与颜色识别及全库落盘，以及后续平滑切换至现场生产环境。

---

## 一、裸机系统安装与基础环境配置（Intel i3 + 16GB）

### 1.1 安装操作系统（Ubuntu Server 24.04 LTS）
1. 在另一台电脑下载 **Ubuntu 24.04 LTS Live Server (amd64)** ISO 镜像，使用 Rufus / Ventoy 写入 U 盘。
2. 将 Intel i3 设备接入显示器、键盘与网线，BIOS/UEFI 关闭 Secure Boot（可选），从 U 盘启动安装：
   - 分区方案：使用默认 LVM 整盘自动分区（建议 `/var/lib/docker` 与系统根分区至少留足 80GB+，用于缓存摄像头抓拍与证据图片）。
   - 勾选安装 **OpenSSH server**。
3. 装机重启后 SSH 登录设备，执行基础初始化与静态 IP / NTP 配置：

```bash
# 1. 更新系统与基础工具
sudo apt-get update && sudo apt-get install -y curl vim sqlite3 jq chrony usbutils net-tools

# 2. 开启高精度 NTP 对时（HMAC 防篡改签名要求云边时钟误差 < 5 分钟）
sudo systemctl enable --now chrony
chronyc tracking

# 3. 安装 Docker Engine & Docker Compose 插件（参考官方 APT 源）
curl -fsSL https://get.docker.com | sudo sh
sudo usermod -aG docker $USER
newgrp docker

docker --version
docker compose version
```

### 1.2 为 Intel i3 配置固定局域网 IP（保证摄像头与云端可达）
编辑 `/etc/netplan/00-installer-config.yaml`（按实际网口名如 `enp3s0` 修改）：
```yaml
network:
  version: 2
  ethernets:
    enp3s0:
      dhcp4: false
      addresses: [192.168.1.120/24]
      routes:
        - to: default
          via: 192.168.1.1
      nameservers:
        addresses: [223.5.5.5, 114.114.114.114]
```
应用生效并检查：`sudo netplan apply && ip addr show`

---

## 二、测试环境部署方案（二选一）

### 方案 A：单台 Intel i3 同机跑齐 Cloud + Edge + Vision + PostgreSQL（最适合现阶段联调测试）
Intel i3 + 16GB 内存完全充裕运行全栈（总内存占用约 2.5~4GB）：

```bash
git clone <您的仓库地址> /opt/chongdian
cd /opt/chongdian/deploy
cp .env.example .env
```

编辑 `deploy/.env`，**测试阶段关键变量建议如下**：
```env
BIND_IP=0.0.0.0
DB_PASS=ChongdianTestDb#2026
CLOUD_SECURITY_ENABLED=true
ADMIN_USERNAME=admin
ADMIN_PASSWORD=SetYourStrongAdminPassword!
OPERATOR_USERNAME=operator
OPERATOR_PASSWORD=SetYourStrongOperatorPassword!
# 局域网无 HTTPS 证书测试必须设为 false，否则浏览器无法保存登录会话
SESSION_COOKIE_SECURE=false

EDGE_API_KEY=ReplaceWithRandomCloudEdgeKey32Chars
EDGE_UPGRADE_SECRET=ReplaceWithRandomUpgradeKey32Chars
EDGE_NODE_ID=edge-i3-01
EDGE_CLOUD_URL=http://cloud-platform:8080
EDGE_LOCAL_API_KEY=

# 测试阶段：无物理继电器时使用 mock，摄像头视觉与数据库全量走真实生产链
EDGE_HARDWARE_MODE=mock
EDGE_STRICT_HARDWARE=false

# RTSP AI 视觉边车
EDGE_VISION_MODE=external
EDGE_AI_ENGINE=python-hyperlpr3
VISION_RTSP_TRANSPORT=tcp
VISION_INFER_FPS=1.0
VISION_MIN_CONFIDENCE=0.72
```

构建并启动全栈服务：
```bash
cd /opt/chongdian/deploy
docker compose up -d --build
```

验证服务健康：
```bash
curl -s http://127.0.0.1:8080/api/v1/health
curl -s http://127.0.0.1:8081/health
curl -s http://127.0.0.1:8090/health
```

### 方案 B：Intel i3 仅部署边缘端（对接独立云端服务器）
如果云端与数据库部署在独立服务器（如 `192.168.1.50`），在 Intel i3 上将 `deploy/.env` 中 `EDGE_CLOUD_URL=http://192.168.1.50:8080`，并运行：
```bash
cd /opt/chongdian/deploy
docker compose -f docker-compose.edge-i3.yml up -d --build
```

---

## 三、真实 RTSP 摄像头接入配置

### 3.1 推荐摄像头子码流参数
为让 Intel i3 稳定支持多路拉流识别且网络零卡顿，请在摄像头 Web 管理端配置 **子码流 / 辅码流**：
- 编码：`H.264`（Base/Main Profile）
- 分辨率：`1280x720`（测试单路近距车牌图片可设 `1920x1080`）
- 帧率：`15 FPS`，码率：`1024~2048 kbps`，固定码率 CBR

常见品牌 RTSP 地址格式：
- **海康威视 (Hikvision)**：`rtsp://admin:<密码>@<摄像头IP>:554/Streaming/Channels/102`（子码流 `102`，主码流 `101`）
- **大华 (Dahua)**：`rtsp://admin:<密码>@<摄像头IP>:554/cam/realmonitor?channel=1&subtype=1`（辅码流 `subtype=1`）
- **TP-Link / 宇视**：`rtsp://admin:<密码>@<摄像头IP>:554/stream2`

先在 i3 上用 `ffprobe` / `curl` 验证流可达：
```bash
docker exec -it chongdian-vision python -c "
import os, cv2
os.environ['OPENCV_FFMPEG_CAPTURE_OPTIONS'] = 'rtsp_transport;tcp|stimeout;5000000'
cap = cv2.VideoCapture('rtsp://admin:密码@192.168.1.x:554/Streaming/Channels/102', cv2.CAP_FFMPEG)
print('opened=', cap.isOpened(), 'read=', cap.read()[0])
"
```

### 3.2 在管理后台绑定节点、摄像头与车位
访问云端管理后台：`http://<i3主机IP>:8080/admin.html`（使用你在 `.env` 中设置的管理员账号登录）：
1. **步骤 1（停车场）**：选择默认场站 `default` 或新建测试场站。
2. **步骤 2（边缘工控机）**：新增节点 `edge-i3-01`（务必与 `.env` 中 `EDGE_NODE_ID` 一致），`authKey` 填写 `.env` 中 `EDGE_API_KEY` 的精确值。等待约 10 秒后，页面自动显示节点 `ONLINE` 及真实 CPU/内存遥测。
3. **步骤 3（摄像头接入）**：新增摄像头，如 `CAM-01` -> 归属节点 `edge-i3-01` -> 监控车位 `A-01` -> 协议选 `RTSP` -> 填入真实 RTSP URL。如需裁剪检测范围可填归一化 ROI：
   `{"x":0.1,"y":0.1,"w":0.8,"h":0.8}`
4. **步骤 5（车位与声光联动）**：新增车位 `A-01`，关联 `CAM-01`，指定电桩编号与继电器通道 `0`，保存。

边缘机将在 **15 秒内** 从云端自动拉取配置与摄像头 RTSP 地址；`vision-service` 将在 **10 秒内** 开始持续拉流推帧与车牌识别！

---

## 四、使用图片模拟蓝牌燃油车与新能源绿牌车全链路测试

### 4.1 车牌图片模拟规范（提高真实摄像头 OCR 准确率）
1. 找几张**真实清晰、无强反光**的中国汽车车牌照片或高清图片（可 A4 彩色打印或用高亮度平板/大屏幕显示）：
   - **蓝牌燃油车（7位）**：如 `粤B12345`、`京A88888`（HyperLPR3 识别类型为 `BLUE` -> `FUEL`）
   - **绿牌新能源车（8位，第3位或末位含 `D`/`F`）**：如 `粤BD12345`、`沪A12345F`（HyperLPR3 + 8位正则双重校验为 `GREEN` -> `NEW_ENERGY`）
2. 拍摄摆放建议：
   - 车牌在视频画面中横向宽度建议 **140~400 像素**，倾斜角度 `< 15°`。
   - 若用平板/显示器翻拍，请降低摄像头顺光反光并关闭屏幕高刷频闪（亮度调至 60%~75%）。
3. 进入边缘本地后台 `http://<i3主机IP>:8081/admin` -> 「视频与通道」检查真实 RTSP 画面已出图，然后调整相机/图片位置使其落入 ROI 框内。

### 4.2 逐项验收标准与落库验证命令

#### 场景 1：模拟蓝牌燃油车占位（预计触发违规 + 真实取证图片 + 报警）
1. 将**蓝牌燃油车图片**置于摄像头视野中保持 ≥ 5 秒（超过入场确认防抖时间 `3s`）。
2. **预期表现**：
   - `vision-service` 日志输出识别到真实 7 位蓝牌（如 `粤B12345`，confidence ≥ 0.72）。
   - 大屏 `http://<i3主机IP>:8080/index.html`：车位 `A-01` 变红告警，显示车牌与 `[RTSP_VISION_AI]` 来源，**无 `SIM` 假数据标签**。
   - 右侧预警流水与云端后台「违规台账」生成一条真实带**带水印现场抓拍缩略图**的 `FUEL_OCCUPIED` 事件（点击缩略图可放大看真实 JPEG）。
   - 云端后台「AI 视觉感知 -> 实时识别流水 / 16 车位矩阵」显示车牌号、蓝色徽标、置信度及实时抓拍图。

#### 场景 2：车辆驶离 -> 更换绿牌新能源车图片（预计自动消警闭环 + 合规停放）
1. 移开蓝牌图片，保持空车位 ≥ 6 秒（超过离场防抖 `5s`），事件自动变为 `CLEARED`（已离场闭环）。
2. 再放置 **8 位新能源绿牌图片**（如 `粤BD12345`）：
   - 系统识别车牌颜色 `GREEN`、车辆类型 `NEW_ENERGY`，展示绿色车牌徽标。
   - 此时若对应充电桩状态为 `CHARGING`，车位卡片呈绿色正常充电状态，不报警；若电桩持续 `IDLE` 超过 `unchargedGraceSeconds`，将触发未充电超时占位预警。

#### 4.3 检查 Edge SQLite 与 Cloud PostgreSQL 真实落库数据

检查边缘端 SQLite (`edge.db`)：
```bash
docker exec -it chongdian-edge sqlite3 /data/edge/edge.db "
SELECT space_id, plate, plate_color, vehicle_type, round(confidence,3), judgment_status, recognized_at
FROM recognition_records ORDER BY recognized_at DESC LIMIT 10;
"
docker exec -it chongdian-edge sqlite3 /data/edge/edge.db "
SELECT event_id, space_id, plate, violation_type, evidence_url, status, synced
FROM violation_events ORDER BY occurred_at DESC LIMIT 10;
"
```

检查云端 PostgreSQL (`chongdian`)：
```bash
docker exec -it chongdian-postgres psql -U chongdian -d chongdian -c "
SELECT space_id, plate, plate_color, vehicle_type, confidence, judgment_status, is_simulated, recognized_at
FROM chg_plate_recognition_records ORDER BY recognized_at DESC LIMIT 10;
"
docker exec -it chongdian-postgres psql -U chongdian -d chongdian -c "
SELECT event_id, site_id, space_id, plate, violation_type, evidence_url, status
FROM chg_violation_events ORDER BY occurred_at DESC LIMIT 10;
"
docker exec -it chongdian-postgres psql -U chongdian -d chongdian -c "
SELECT space_id, phase, plate, vehicle_type, alarming, stream_type, snapshot_url, confidence, observed_at
FROM chg_space_runtime_status ORDER BY space_id;
"
```

---

## 五、从模拟测试平滑切换到生产环境

本项目所有硬件、流媒体、鉴权与持久化开关全部收口至环境变量与云端后台规则中，**无需改动一行业务代码**：

| 配置项 | 模拟测试阶段值 | 正式生产环境值 | 说明 |
| :--- | :--- | :--- | :--- |
| `EDGE_HARDWARE_MODE` | `mock` | `modbus-tcp` 或 `modbus-rtu` | 接入现场网络继电器或 RS-485 声光报警器 |
| `EDGE_STRICT_HARDWARE` | `false` | `true` | 生产开启严格模式：总线断线不伪造成功响应 |
| `EDGE_MODBUS_TCP_ADDR` | 留空 | `192.168.x.x:502` | 网络型 Modbus TCP 继电器控制器 IP |
| `EDGE_MODBUS_RTU_PORT` | 留空 | `/dev/ttyUSB0` | 在 `docker-compose.edge-i3.yml` 取消 `devices` 注释 |
| `EDGE_VISION_MODE` | `external` | `external` | 保持由 Python RTSP HyperLPR3 边车持续推理 |
| `VISION_MIN_CONFIDENCE` | `0.72` | `0.75~0.80` | 正式车场可根据照明与抓拍清晰度微调阈值 |
| `SESSION_COOKIE_SECURE` | `false` | `true` | 生产接入 Nginx/Caddy HTTPS 反向代理后必须开启 |
| `EDGE_LOCAL_API_KEY` | 可留空 | 强随机密钥 | 锁定工控机本地 `8081` 管理端口，防止内网未授权访问 |
| `DB_PASS` / `ADMIN_PASSWORD` / `EDGE_API_KEY` / `EDGE_UPGRADE_SECRET` | 测试密码 | 重新生成 32 位强随机值 | 正式上线前全部轮换，不复用测试环境口令 |
| 充电桩协议与运营商对接 | 未接桩默认未知/测试配置 | 后台步骤 4/5 绑定真实品牌协议 + OCPP/HTTP 参数 | 打开即生效，自动随版本下发至边缘机 |

### 生产切换前自检清单
1. **停止测试环境残留数据（如需清空测试流水）**：在云端后台「违规台账」点击清空历史流水，并视需要重置测试车位映射。
2. **更新 `.env` 密钥与硬件模式**，如使用 RS-485 串口，在 `deploy/docker-compose.edge-i3.yml` 取消 `devices: - /dev/ttyUSB0:/dev/ttyUSB0` 注释。
3. **重启容器生效**：
   `cd /opt/chongdian/deploy && docker compose up -d`（或 `docker compose -f docker-compose.edge-i3.yml up -d`）。
4. **实车/实际摄像头巡检**：在 `admin.html` 与 `index.html` 核对边缘机遥测（CPU/内存/温度）、相机状态 `ONLINE`、继电器触发、实景抓拍、违规证据上传及数据库持久化全部正常。
