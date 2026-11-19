# 云边协同接口契约与数据模型规范（v1 生产正式版）

更新日期：2026-09-08  
规范范围：`cloud-platform` 云端微服务、`edge-node` 边缘工控机、`frontend` 监控大屏及第三方集成。

---

## 一、 通信基础与安全准入规范

1. **协议规范**：所有接口均为 HTTP/1.1 或 HTTP/2，全量 API 包含 `/api/v1` 前缀。
2. **时间格式**：全量时间字段使用 ISO-8601 UTC 格式，例如 `2026-09-08T10:00:00Z`。
3. **双重鉴权与请求防重放**：
   - **管理员鉴权**：通过 HttpOnly Cookie 会话（账号密码登录后建立），不支持 `X-Admin-Api-Key` 请求头；
   - **边缘节点鉴权**：请求头携带 `X-Api-Key: {EDGE_API_KEY}`；
   - **HMAC 防重放签名（核心上报接口强制启用）**：
     - 请求头携带 `X-Edge-Timestamp: {unixEpochSeconds}`、`X-Edge-Nonce: {uuid}`、`X-Content-SHA256: {sha256Hex}`、`X-Node-Id: {nodeId}` 与 `X-Edge-Signature: {hexDigest}`；
     - 待签字符串规范：`"{unixEpochSeconds}\n{nonce}\n{HTTP_METHOD}\n{requestPath}\n{sha256(body)}\n{nodeId}"`（6 个字段，以换行符分隔）；
     - 密钥为 `EDGE_API_KEY`，使用 HMAC-SHA256 算法计算，云端校验时钟漂移允许窗口为 ±60 秒；
     - nonce 单次有效（服务端缓存已用 nonce，防止重放），`nodeId` 必须与 URL 路径中的节点标识一致。

---

## 二、 云端管控平台接口 (`cloud-platform`)

### 1. 监控大屏与全局统计
- `GET /api/v1/health`  
  公开系统健康探针，返回 `{"status":"UP","database":"POSTGRES|FILE"}`。
- `GET /api/v1/dashboard/stats`  
  监控大屏核心聚合统计（免密公开）：
  ```json
  {
    "totalEvents": 128,
    "todayEvents": 14,
    "violationTypeCounts": { "FUEL_VEHICLE": 8, "NOT_CHARGING_OVERTIME": 6 },
    "spaceViolationCounts": { "A-01": 5, "B-02": 9 },
    "onlineNodes": 2,
    "totalNodes": 2
  }
  ```

### 2. 管理员认证与权限
- `POST /api/v1/auth/login`：提交 `{"username":"admin","password":"..."}` 获取 HttpOnly 会话。
- `POST /api/v1/auth/logout`：注销当前管理员会话。
- `GET /api/v1/auth/session`：查询当前操作员身份与权限角色（已登录返回角色，未登录返回 GUEST）。
- `POST /api/v1/auth/change-password`：预留扩展接口，当前版本密码通过环境变量管理。

### 3. 车位实时在线状态与 SSE 长连接
- `POST /api/v1/nodes/{nodeId}/spaces/status`：边缘节点每 3 秒批量上报车位实时感知：
  ```json
  [
    {
      "nodeId": "edge-01",
      "siteId": "site-1",
      "spaceId": "A-01",
      "phase": "OCCUPIED",
      "chargerStatus": "IDLE",
      "plate": "粤B88888",
      "vehicleType": "FUEL",
      "alarming": true,
      "activeEventId": "d6f7d1f6-2a4c-4c38-a4f8-6c1b0e60f6b4",
      "observedAt": "2026-09-08T10:00:00Z",
      "isSimulated": false,
      "streamType": "REAL_CAMERA_INFERENCE"
    }
  ]
  ```
- `GET /api/v1/spaces/status`：云端查询当前所有车位实时快照。
- `GET /api/v1/stream/spaces`：基于 SSE (Server-Sent Events) 的车位状态毫秒级实时流。
- `GET /api/v1/stream/events`：基于 SSE 的违规报警与消警事件实时流。

### 4. 违规事件生命周期与消警闭环
- `POST /api/v1/events`：边缘节点上报违规事件（`ALARMING` / `CLEARED`）：
  ```json
  {
    "eventId": "d6f7d1f6-2a4c-4c38-a4f8-6c1b0e60f6b4",
    "sessionId": "A-01:1788800000000",
    "spaceId": "A-01",
    "plate": "粤B88888",
    "vehicleType": "FUEL",
    "violationType": "FUEL_VEHICLE",
    "occurredAt": "2026-09-08T10:00:00Z",
    "evidenceUrl": "https://oss.example.com/evidences/A-01_20260908.jpg",
    "status": "ALARMING"
  }
  ```
- `GET /api/v1/events`：历史事件分页与条件检索（按车位、车牌、时间范围）。
- `GET /api/v1/events/export`：导出历史台账为标准 UTF-8 CSV 文件。
- `POST /api/v1/events/{eventId}/acknowledge`：现场安保或运营人员一键消警闭环，全网广播 `ACKNOWLEDGED`。

### 5. 规则配置热同步
- `GET /api/v1/configuration` & `PUT /api/v1/configuration`：
  发布递增版本配置，包含超时阈值、车位绑定、继电器物理通道与白名单。
- `GET /api/v1/rules` & `PUT /api/v1/rules`：全局研判规则查询与更新。
- `GET /api/v1/nodes/heartbeats`：查询各边缘节点心跳时间戳。
- `POST /api/v1/nodes/{nodeId}/heartbeat`：边缘节点心跳上报。
- `GET /api/v1/nodes/configuration-status`：查询各节点版本生效结果。

### 6. 车牌白名单与操作审计
- `GET /api/v1/whitelist`、`POST /api/v1/whitelist`、`DELETE /api/v1/whitelist/{plate}`。
- `GET /api/v1/audit`：返回合规审计日志流水（登录、改密、调配、消警等操作记录）。

### 9. 多停车场管理
- `GET /api/v1/sites`：列出所有停车场。
- `POST /api/v1/sites`：创建新停车场。
- `GET /api/v1/sites/{siteId}/configuration`：查询指定停车场配置。
- `PUT /api/v1/sites/{siteId}/configuration`：更新指定停车场配置。
- `GET /api/v1/sites/{siteId}/rules`：查询指定停车场研判规则。
- `PUT /api/v1/sites/{siteId}/rules`：更新指定停车场研判规则。
- `POST /api/v1/sites/{siteId}/nodes/{nodeId}`：将边缘节点绑定到指定停车场。

### 7. 企业级 Webhook 告警与回调
- 投递支持钉钉 Markdown、飞书 Interactive 卡片与标准 JSON，携带 `X-Webhook-Signature` 与 `X-Idempotency-Key`。

### 8. 远程升级（OTA）云端协同
- `POST /api/v1/upgrade/manifest`：管理员发布新版本固件升级清单：
  ```json
  {
    "version": 2,
    "versionName": "v2.0.0-prod",
    "packageUrl": "https://release.example.com/edge_v2.0.bin",
    "sha256Hash": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
    "signature": "hmac_sha256_hex_signature_string",
    "minCompatibleVersion": 1,
    "releaseNotes": "生产版本加固与算力加速",
    "createdAt": "2026-09-08T12:00:00Z"
  }
  ```
- `GET /api/v1/upgrade/manifest`：获取当前待升级清单。
- `POST /api/v1/nodes/{nodeId}/upgrade-status`：接收边缘上报的升级与回滚状态。
- `GET /api/v1/nodes/upgrade-status`：查询全站边缘网关版本状态大表。

---

## 三、 边缘计算节点接口 (`edge-node`)

### 1. 节点健康与运维指标
- `GET /health`：返回边缘服务就绪状态、看门狗状态、硬件通道状态与配置版本。
- `GET /api/v1/metrics`：**Prometheus 标准文本时序数据抓取接口**：
  ```text
  # HELP chg_edge_frames_processed_total Total video frames processed
  # TYPE chg_edge_frames_processed_total counter
  chg_edge_frames_processed_total{node="edge-01"} 145152
  # HELP chg_edge_violations_detected_total Total occupancy violations detected
  # TYPE chg_edge_violations_detected_total counter
  chg_edge_violations_detected_total{node="edge-01"} 483
  # HELP chg_edge_disk_watermark_percent Current disk usage watermark percent
  # TYPE chg_edge_disk_watermark_percent gauge
  chg_edge_disk_watermark_percent{node="edge-01"} 78.4
  ```
- `GET /api/v1/telemetry`：JSON 格式详细遥测快照，包含存储用量与 90% 水位告警状态：
  ```json
  {
    "nodeId": "edge-01",
    "uptimeSeconds": 86400,
    "framesProcessed": 145152,
    "violationsDetected": 483,
    "alarmsTriggered": 483,
    "sqliteUnuploaded": 0,
    "clockSkewMillis": 12,
    "disk": {
      "totalBytes": 10737418240,
      "usedBytes": 841824000,
      "availableBytes": 9895594240,
      "watermarkPercent": 7.84,
      "alertThresholdPercent": 90.0,
      "isCritical": false
    }
  }
  ```

### 2. 本地决策与防抖引擎
- `POST /api/v1/spaces/{spaceId}/evaluate`：业务规则手动/调试决策。
- `POST /api/v1/spaces/{spaceId}/detections`：单帧检测注入（供独立摄像头或外部检测进程调用）。
- `GET /api/v1/spaces/{spaceId}/tracker`：查询单个车位防抖计时状态（进场观察、在位确认、离场观察）。
- `GET /api/v1/spaces/status`：获取所有车位实时状态快照。
- `GET /api/v1/stream/spaces` & `GET /api/v1/stream/events`：边缘本地 SSE 实时推流。

### 3. 硬件控制器与声光消音闭环
- `GET /api/v1/relays/details`：回读每个车位的物理继电器线圈触点、报警请求、累计触发时间与自动消音状态。
- `POST /api/v1/spaces/{spaceId}/silence`：现场就地一键切断特定车位声光报警，并同步确认关联违规事件。
- `POST /api/v1/events/{eventId}/acknowledge`：边缘端按事件 ID 执行人工消警闭环并复位物理继电器。
- `GET /api/v1/chargers` & `POST /api/v1/chargers/snapshots`：充电桩实时状态与陈旧性降级管理。

### 4. 边缘 OTA 升级与回滚接口
- `GET /api/v1/upgrade/report`：返回当前主程序版本、备份状态机与上一次升级结果。
- `POST /api/v1/upgrade/apply`：执行固件升级。支持通过 JSON 提交新包十六进制或文件下载，边缘自动执行验签、暂存 `.backup`、启动自检探针与失败原子回滚。

---

## 四、 环境变量生产配置字典

| 变量名 | 默认值 | 作用说明 |
|---|---|---|
| `EDGE_NODE_ID` | `edge-local-01` | 边缘节点全局唯一 ID |
| `EDGE_DATA_DIR` | `data` | SQLite 数据库、证据图片及配置持久化根目录 |
| `EDGE_CLOUD_URL` | 无 (本地自治) | 云端管理平台 URL（配置后自动激活心跳、状态上报与升级检查） |
| `EDGE_API_KEY` | 无 | 边缘访问云端的共享密钥，用于请求 HMAC-SHA256 验签 |
| `EDGE_HARDWARE_MODE` | `mock` | 硬件运行模式：`modbus-tcp`、`modbus-rtu` 或 `mock` |
| `EDGE_MODBUS_TCP_ADDR` | `127.0.0.1:502` | Modbus TCP 网络继电器监听地址 |
| `EDGE_MODBUS_RTU_PORT` | `/dev/ttyUSB0` (`COM3`) | Modbus RTU 物理 RS-485 串口设备名 |
| `EDGE_STRICT_HARDWARE` | `false` | **生产严格模式**：设为 `true` 时物理硬件连接失败立即终止进程，禁止伪造执行 |
| `EDGE_STREAM_MODE` | `AUTO` | 视频流模式：`PROD`/`REAL`（全真实流）、`MOCK`（全回放）、`AUTO`（前2路真实，其余回放） |
| `EDGE_GB28181_SIP_PORT` | `5060` | GB28181 SIP UDP 服务监听端口 |
| `EDGE_EVIDENCE_MAX_BYTES` | `5368709120` (5GB) | 本地违规抓拍存储配额，超出自动 FIFO 淘汰老旧抓拍与边车 |
| `EDGE_DISK_CAPACITY_BYTES`| `10737418240` (10GB) | 工控机磁盘配额基准，用于计算 Prometheus >90% 水位报警 |
| `EDGE_UPGRADE_SECRET` | 无（必须配置） | 升级固件包 HMAC-SHA256 验签共享密钥（生产环境必须替换为独立随机密钥） |
| `NOTIFY_SOCKET` | 由 systemd 注入 | systemd 服务状态通信套接字（用于 `sd_notify` 状态流转） |
