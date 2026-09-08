# 充电车位防占位预警系统

当前仓库包含一个可运行的云边协同 MVP：

- `edge-node`：Rust 边缘节点，提供最多 16 路能力声明、规则判定、配置读写和继电器控制抽象。
- `cloud-platform`：Java/Spring Boot 云端平台，提供规则、违规事件、节点心跳和健康检查接口。
- `DEVELOPMENT_PLAN.md`：开发阶段、里程碑、验收基线和后续集成顺序。

## 本地运行

### 边缘节点

```text
cd edge-node
cargo run --offline
```

默认监听 `8081`。设置 `EDGE_CLOUD_URL=http://localhost:8080` 可启用违规事件上报和节点心跳，设置 `EDGE_NODE_ID` 可指定节点编号。云端短暂不可用时，边缘节点将事件放入有界队列并每 5 秒重试。

### 云端平台

```text
cd cloud-platform
mvn spring-boot:run
```

默认监听 `8080`。当前 MVP 使用内存存储，接入 PostgreSQL、对象存储和 MQTT 前应先完成接口联调。

## 边缘接口

`GET /health` 返回节点状态和 `max_video_channels=16`。

`GET /api/v1/rules`、`PUT /api/v1/rules` 读取和更新边缘规则缓存。

`POST /api/v1/spaces/{spaceId}/evaluate` 提交一个车位识别结果并触发该车位继电器状态。请求中的时间使用 ISO-8601，字段使用 camelCase，车辆类型使用 `FUEL`、`NEW_ENERGY`、`UNKNOWN`，充电状态使用 `IDLE`、`PREPARING`、`CHARGING`、`COMPLETED`、`FAULT`、`OFFLINE`、`UNKNOWN`。

运行 `cargo run --offline --bin simulator` 可在没有摄像头、充电桩和继电器硬件的情况下执行六个标准场景。设置 `SIM_EDGE_URL=http://localhost:8081` 后，模拟器会改为调用运行中的边缘节点接口。

边缘节点的事件和规则数据默认写入 `edge-node/data`，可通过 `EDGE_DATA_DIR` 指定持久化目录。

## 云端接口

- `GET /api/v1/health`
- `GET/PUT /api/v1/configuration`
- `GET/POST /api/v1/cameras`
- `POST /api/v1/cameras/{cameraId}/heartbeat`
- `GET/PUT /api/v1/rules`
- `POST/GET /api/v1/events`
- `POST /api/v1/nodes/{nodeId}/heartbeat`
- `GET /api/v1/nodes/heartbeats`

## 下一步集成

1. 接入真实 GB28181 SIP/RTP 媒体流和识别模型。
2. 实现充电桩平台适配器，并将状态映射到统一枚举。
3. 将内存继电器控制替换为工业 I/O 驱动。
4. 增加 PostgreSQL、对象存储、MQTT/TLS、鉴权和审计。
5. 增加硬件 watchdog、A/B 远程升级和 16 路压力测试。
