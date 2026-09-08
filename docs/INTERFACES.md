# 云边接口契约（v1）

所有时间使用 ISO-8601 UTC 格式，所有接口保留 `/api/v1` 前缀。边缘节点在配置了 `EDGE_CLOUD_URL` 时才向云端上报事件和发送心跳；未配置时保持本地自治运行。边缘节点使用有界磁盘事件仓库缓存云端暂不可达时的违规事件，网络恢复后自动重试，最多保存 10,000 条，进程重启后仍可继续补传。

## 识别结果到边缘判定

`POST /api/v1/spaces/{spaceId}/evaluate`

```json
{
  "spaceId": "A-01",
  "plate": "粤A12345",
  "vehicleType": "NEW_ENERGY",
  "chargerStatus": "IDLE",
  "occupiedSince": "2026-09-07T10:00:00Z",
  "chargeCompletedAt": null,
  "now": "2026-09-07T10:11:00Z",
  "whitelisted": false
}
```

返回值包含 `decision`、`violation_type`、`should_alarm` 和 `event_id`。边缘节点根据 `should_alarm` 控制该车位对应的继电器。

## 边缘事件上报云端

`POST {EDGE_CLOUD_URL}/api/v1/events`

```json
{
  "eventId": "d6f7d1f6-2a4c-4c38-a4f8-6c1b0e60f6b4",
  "spaceId": "A-01",
  "plate": "粤A12345",
  "vehicleType": "NEW_ENERGY",
  "violationType": "NOT_CHARGING_OVERTIME",
  "occurredAt": "2026-09-07T10:11:00Z",
  "evidenceUrl": null
}
```

## 节点心跳

`POST {EDGE_CLOUD_URL}/api/v1/nodes/{nodeId}/heartbeat`

边缘节点每 30 秒发送一次。云端按最后心跳时间判断在线状态。

## 规则配置

`GET/PUT /api/v1/rules`

阈值单位为秒，动作取 `ALLOW`、`WAIT`、`VIOLATE`。边缘节点在写入前校验配置，并将配置持久化到 `EDGE_DATA_DIR/rules.json`。

```json
{
  "unchargedGraceSeconds": 600,
  "completedGraceSeconds": 900,
  "vehicleEnterConfirmSeconds": 3,
  "vehicleLeaveConfirmSeconds": 5,
  "alarmRepeatSeconds": 60,
  "alarmAutoStopSeconds": 300,
  "unknownVehicleAction": "WAIT",
  "unknownStatusAction": "WAIT",
  "faultStatusAction": "ALLOW"
}
```

## 设备绑定与云边配置

`GET/PUT /api/v1/configuration`

云端配置包含递增的 `version`、规则和车位设备绑定。绑定中的 `spaceId`、`cameraId`、`chargerId` 与 `relayChannel` 共同确定一条车位控制链路。云端更新时自动生成新版本；边缘节点只应用通过校验且版本更高的配置，断网期间继续使用本地最后有效版本。

```json
{
  "version": 1,
  "updatedAt": "2026-09-07T10:00:00Z",
  "rules": { "unchargedGraceSeconds": 600, "completedGraceSeconds": 900 },
  "bindings": [
    { "siteId": "site-1", "spaceId": "A-01", "cameraId": "camera-01", "chargerId": "charger-01", "relayChannel": 0, "enabled": true }
  ]
}
```

## 充电桩适配器边界

适配器应将厂商状态转换为 `IDLE`、`PREPARING`、`CHARGING`、`COMPLETED`、`FAULT`、`OFFLINE` 或 `UNKNOWN`，并提供车位号、状态时间和充电会话时间。规则引擎不依赖厂商字段。

## 继电器适配器边界

继电器实现 `set_alarm(space_id, enabled)`。硬件驱动替换 `MemoryRelayController` 后，规则和 HTTP 接口保持不变；每个车位必须独立控制并返回执行错误。

## GB28181 摄像头通道管理（边缘节点）

边缘节点最多维护 16 个摄像头通道。当前阶段先完成注册、车位绑定和心跳，真实 SIP/RTP 媒体接入在此通道管理层之下实现。

- `GET /api/v1/cameras`：查看通道状态、关联车位和最后心跳；
- `POST /api/v1/cameras`：注册或更新一个摄像头通道；
- `POST /api/v1/cameras/{cameraId}/heartbeat`：模拟或真实 GB28181 会话心跳。

摄像头注册数据示例：

```json
{
  "cameraId": "camera-01",
  "channelId": "34020000001320000001",
  "spaceId": "A-01",
  "gbDeviceId": "34020000001110000001",
  "state": "REGISTERED",
  "streamUrl": null,
  "lastHeartbeat": null
}
```

## 视觉目标检测与在位防抖接口（边缘节点）

- `POST /api/v1/spaces/{spaceId}/detections`：相机或算法模块推送单帧检测结果，边缘节点维护状态机（`Empty` -> `Entering` -> `Occupied` -> `Leaving`），应用 `vehicleEnterConfirmSeconds` 与 `vehicleLeaveConfirmSeconds` 防抖。离场时自动消警，违规时自动附加证据图片路径上报。
- `GET /api/v1/spaces/{spaceId}/tracker`：查询指定车位当前在位阶段与防抖持续时间。

## 继电器详细状态与防扰民监控（边缘节点）

- `GET /api/v1/relays/details`：返回每个车位的独立通道号、物理闭合状态、业务报警请求状态、最后触发时间与 `autoMuted`（是否因超出 `alarmAutoStopSeconds` 触发声光防扰民保护）。

## 充电桩实时状态与离线软降级接口（边缘节点）

- `GET /api/v1/chargers`：获取所有关联车位当前充电桩快照与健康状态。
- `POST /api/v1/chargers/snapshots`：充电桩平台主动同步实时状态（`IDLE`, `PREPARING`, `CHARGING`, `COMPLETED`, `FAULT`, `OFFLINE`）。超期未同步自动降级为 `OFFLINE`。

## 云端白名单与监控大屏接口（云端平台）

- `GET /api/v1/whitelist`：查询白名单车辆列表。
- `POST /api/v1/whitelist`：新增白名单车辆。
- `DELETE /api/v1/whitelist/{plate}`：移出白名单。
- `GET /api/v1/dashboard/stats`：监控大屏全局概览，包含累计与今日违规数、违规类型分布、车位频度分布、边缘节点在线率等。

