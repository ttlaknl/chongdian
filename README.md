# 充电车位智能防占位与声光预警系统 (EV Charging Bay Occupancy Monitoring System)

[![Rust Tests](https://img.shields.io/badge/Rust%20Tests-Passing-brightgreen)](#)
[![Java Tests](https://img.shields.io/badge/Java%20Tests-Passing-brightgreen)](#)
[![Smoke Tests](https://img.shields.io/badge/Smoke%20Tests-7%2F7%20Passed-brightgreen)](#)
[![License](https://img.shields.io/badge/License-Proprietary-blue)](#)

本项目是一套高可靠、工业级的**云边协同充电车位防占位预警系统**。系统通过边缘工控机就地接入高清摄像头（GB28181/RTSP）与充电桩平台实时数据，利用端侧 AI 视觉识别算法与车位状态机，精准识别**燃油车占位**与**新能源车超时不充电**等违规行为，并通过 Modbus RTU/TCP 联动现场高分贝语音报警器与网络继电器进行声光告警与自动消音闭环，同时支持离线断点续传上报至云端管控中心。

---

## 一、 系统架构与核心特性

```
┌─────────────────────────────────────────────────────────────┐
│                       云端管控平台 (Spring Boot 3)            │
│  - 规则热同步 / 白名单管理   - 违规事件全周期台账与CSV导出        │
│  - SSE 毫秒级大屏实时推流    - 企业级告警 Webhook (钉钉/飞书)    │
│  - PostgreSQL/Flyway 持久化  - 远程固件 OTA 清单与节点监控       │
└──────────────────────────────▲──────────────────────────────┘
                               │ HTTP/2 + HMAC-SHA256 签名 (断网本地暂存续传)
┌──────────────────────────────▼──────────────────────────────┐
│                    边缘计算工控机 (Rust Edge Node)            │
│ ┌───────────────────────┐   ┌─────────────────────────────┐ │
│ │ GB28181 / RTSP 媒体流  │   │ 充电桩 API 适配器 (陈旧降级)  │ │
│ └──────────┬────────────┘   └──────────────┬──────────────┘ │
│            ▼                               ▼                │
│ ┌─────────────────────────────────────────────────────────┐ │
│ │ 边缘 AI 推理引擎 (YOLO+LPR) + 三段式防抖状态机 (进入/在位/离开)│ │
│ └──────────────────────────┬──────────────────────────────┘ │
│                            ▼                                │
│ ┌─────────────────────────────────────────────────────────┐ │
│ │ 工业硬件控制层 (Modbus TCP / Modbus RTU RS485 / 声光防扰民) │ │
│ └──────────────────────────┬──────────────────────────────┘ │
│                            ▼                                │
│ ┌─────────────────────────────────────────────────────────┐ │
│ │ 可靠性守护：硬件 Watchdog + systemd notify + 双备份 OTA   │ │
│ └──────────────────────────└──────────────────────────────┘ │
└─────────────────────────────────────────────────────────────┘
```

### 核心特性矩阵 (P0 ~ P5 完整实现)
1. **多路多协议视频接入**：支持 GB28181 SIP/RTP、RTSP H.264/H.265 工业级码流解包，单工控机支持 6~16 路视频并发处理。
2. **端侧高并发 AI 识别**：就地执行车型分类（燃油车/绿牌新能源车）与车牌 OCR 识别，证据图片本地防篡改加盖水印。
3. **精准三段式状态机防抖**：进入观察期（防临时调头误报）、在位检测期（结合充电桩功率与持续违规计时）、离场平滑消警。
4. **工业声光与防扰民控制**：支持 Modbus TCP 与 RS-485 Modbus RTU，具备单次报警最大时长切断（默认 300s）与夜间静音防护机制。
5. **断网自治与断点续传**：断网时边缘 SQLite 有界存储，网络恢复后带 FIFO 配额自清洁并全自动顺序补传至云端。
6. **金融级接口通信安全**：云边链路基于 API Key 与时间戳 HMAC-SHA256 防重放签名，管理端采用 HttpOnly Cookie 隔离。
7. **全周期固件 OTA 升级**：固件双区备份、下载完整性哈希校验、自动探测自检失败秒级回滚。
8. **工业高可用守护**：软硬件双看门狗、Linux `systemd` `sd_notify` 心跳集成，Prometheus 运维指标毫秒抓取。

---

## 二、 目录结构

```text
chongdian/
├── edge-node/                  # [Rust 2021] 边缘工控机核心服务
│   ├── src/
│   │   ├── main.rs             # 边缘服务主循环、管道装配与 API 路由
│   │   ├── gb28181.rs          # GB28181 SIP 信令与 RTP 媒体接收器
│   │   ├── rtsp.rs             # RTSP 客户端与 H.264 关键帧解析
│   │   ├── pipeline.rs         # 视频流解码与多通道帧队列分发
│   │   ├── ai/                 # AI 模型抽象与端侧推理引擎 (含 rknn.rs)
│   │   ├── tracker.rs          # 车位三段式在位跟踪与防抖状态机
│   │   ├── modbus.rs           # Modbus TCP/RTU 继电器驱动与防扰民保护
│   │   ├── storage.rs          # 证据抓拍水印落盘、边车元数据与存储清理
│   │   ├── upgrade.rs          # 固件升级管理器、备份验证与原子回滚
│   │   ├── watchdog.rs         # 硬件看门狗喂狗与 systemd sd_notify
│   │   └── metrics.rs          # Prometheus 性能指标与遥测采集
│   ├── tests/                  # 集成测试、并发压测与故障注入测试
│   └── Cargo.toml              # 依赖声明与 Release 构建优化
├── cloud-platform/             # [Java 21 / Spring Boot 3.4] 云端中心平台
│   └── src/main/java/com/charging/cloud/
│       ├── CloudController.java    # RESTful 接口控制器
│       ├── AuthController.java     # 认证与权限控制器
│       ├── ApiKeyAuthenticationFilter.java # HMAC 验签与安全拦截器
│       ├── CloudStore.java         # 数据存储与审计日志
│       └── ...                     # 其他领域模型与服务类
├── frontend/                   # Tailwind CSS 构建脚本 (编译静态资源至 static/vendor/)
├── deploy/                     # 生产部署资产 (docker-compose, systemd, .env.example)
└── docs/                       # 核心技术文档库
    ├── INTERFACES.md           # 云边全量 API 契约与通信协议规范
    ├── AUDIT_REPORT.md         # 架构与安全合规审计报告 (权威审计结论)
    ├── DEVELOPMENT_PLAN.md     # 阶段里程碑规划与验收基线
    └── RK3588_DEPLOYMENT_GUIDE.md # RK3588 边缘工控机部署指南
```

---

## 三、 本地快速启动

### 1. 边缘节点 (`edge-node`)

> **运行环境需求**：Rust 1.75+ (支持纯离线编译构建)

```bash
cd edge-node

# 1. 编译并运行本地边缘服务 (默认监听 8081 端口)
cargo run --offline

# 2. 运行自动化全量测试集 (单元测试 + 集成测试)
cargo test --offline

# 3. 运行工业级 7 大子系统冒烟自检
cargo run --bin smoke_test --offline
```

### 2. 云端管控平台 (`cloud-platform`)

> **运行环境需求**：JDK 21+，Maven 3.8+ (支持纯离线构建)

```bash
cd cloud-platform

# 1. 本地免数据库默认模式启动 (基于本地持久化目录，监听 8080 端口)
#    默认开启认证，需通过环境变量或 application.yml 配置管理员密码
mvn spring-boot:run

# 2. 运行云端全量单元与切片集成测试
mvn test -o
```

---

## 四、 生产环境部署

### 1. 云端 Docker Compose 一键部署

生产环境使用 PostgreSQL 存储（全面兼容人大金仓 Kingbase、达梦 DM8 等国产化数据库规范）并开启多租户鉴权。

```bash
# 1. 配置生产环境安全密钥
cp deploy/.env.example deploy/.env
# 修改 deploy/.env 中的 DB_PASS, ADMIN_USERNAME, ADMIN_PASSWORD, EDGE_API_KEY 为高强度生产密钥

# 2. 启动云端微服务与数据库容器
docker compose --env-file deploy/.env -f deploy/docker-compose.yml up -d --build

# 3. 检查服务健康状态
curl http://localhost:8080/api/v1/health
```

### 2. 边缘工控机 systemd 部署

在工控机（如 Ubuntu Core / Debian）上通过 systemd 进行常驻守护，结合硬件看门狗保活：

```bash
# 1. 编译极速优化发布二进制
cd edge-node
cargo build --release --offline

# 2. 安装二进制到系统目录
sudo mkdir -p /opt/chongdian
sudo cp target/release/charging-space-edge /opt/chongdian/charging-space-edge
sudo useradd -r -s /bin/false chongdian 2>/dev/null || true
sudo chown -R chongdian:chongdian /opt/chongdian

# 3. 部署 systemd 单元文件
sudo cp deploy/systemd/charging-edge.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now charging-edge

# 4. 查看运行状态与看门狗心跳
sudo systemctl status charging-edge
```

---

## 五、 环境变量生产配置字典

| 环境变量名 | 默认值 | 说明与取值建议 |
|---|---|---|
| `EDGE_NODE_ID` | `edge-local-01` | 边缘节点唯一标识，与云端注册节点 ID 一致 |
| `EDGE_DATA_DIR` | `data` | SQLite 数据库、本地违规图片及临时抓拍持久化目录 |
| `EDGE_CLOUD_URL` | 无 (本地自治) | 云端管控中心地址（例如 `http://10.0.0.10:8080`），配置后自动启动双向同步 |
| `EDGE_API_KEY` | 无 | 边缘向云端通信的共享密钥，用于计算请求 HMAC-SHA256 签名 |
| `EDGE_HARDWARE_MODE` | `mock` | 硬件继电器工作模式：`modbus-tcp`、`modbus-rtu` 或 `mock` |
| `EDGE_MODBUS_TCP_ADDR` | `127.0.0.1:502` | Modbus TCP 网络继电器设备 IP 与端口 |
| `EDGE_MODBUS_RTU_PORT` | `/dev/ttyUSB0` (`COM3`) | 物理 RS-485 串口设备节点（Windows 为 `COMx`） |
| `EDGE_STRICT_HARDWARE` | `false` | **生产严格模式**：设为 `true` 时，若物理硬件连接失败立即退出进程，严禁回退伪造 |
| `EDGE_STREAM_MODE` | `AUTO` | 视频流工作模式：`PROD`/`REAL`（全真实视频流）、`MOCK`（仿真视频流）、`AUTO`（混合） |
| `EDGE_GB28181_SIP_PORT`| `5060` | GB28181 SIP UDP 服务本地监听端口 |
| `EDGE_EVIDENCE_MAX_BYTES`| `5368709120` (5GB) | 本地证据存储配额，超过上限自动触发 FIFO 循环覆盖 |
| `EDGE_UPGRADE_SECRET`  | `replace-with-a-distinct-random-upgrade-signing-key` | 边缘固件包 OTA 验签预共享密钥（**必须**替换为随机值） |

---

## 六、 硬件模式与真实/仿真隔离机制

系统严格遵循**“生产不伪造、测试不依赖实体机”**的设计原则：

1. **生产硬件模式硬阻断**：
   - 生产环境中配置 `EDGE_HARDWARE_MODE=modbus-tcp`（或 `modbus-rtu`）并开启 `EDGE_STRICT_HARDWARE=true`；
   - 若现场 485 总线断开或继电器断电，边缘节点在启动自检阶段调用 `hardware_monitor.verify_ready()` 立即退出（Exit Code 1），杜绝伪造假执行。
2. **仿真数据显式打标**：
   - 在未接摄像机或离线开发调试时，系统自动或按需加载测试回放数据，并在所有事件、上报和接口中显式打上 `is_simulated = true` 及 `[MOCK_SIMULATED]` 标签，云端管理大屏可清晰区分真伪源。
3. **残留测试数据清零**：
   - 生产发布版本默认清空内置历史演示事件，数据库初装为初始干净状态。

---

## 七、 质量保障与自动化测试

所有模块均经过多层次严苛验证，支持在离线安全沙箱环境中复现：

```bash
# 边缘节点 Rust 单元与集成测试
cd edge-node
cargo test --offline

# 边缘节点代码规范与静态语义安全检查
cargo fmt --check
cargo clippy --offline --all-targets -- -D warnings

# 边缘端 7 大子系统冒烟自检 (7/7 Passed)
cargo run --bin smoke_test --offline

# 云端平台 Java 自动化测试
cd ../cloud-platform
mvn test -o
```

---

## 八、 相关技术文档导航

- 📘 [API 接口契约规范 (docs/INTERFACES.md)](docs/INTERFACES.md)：收录云端与边缘端全量接口及协议规范。
- 🔒 [安全与架构合规审计 (docs/AUDIT_REPORT.md)](docs/AUDIT_REPORT.md)：通信签名、防篡改水印与权限隔离说明（权威审计结论）。
- 🗺️ [全阶段规划与验收基线 (docs/DEVELOPMENT_PLAN.md)](docs/DEVELOPMENT_PLAN.md)：P0 ~ P5 阶段执行明细与阶段验收。
- 🚀 [瑞芯微 RK3588 边缘工控机落地指南 (docs/RK3588_DEPLOYMENT_GUIDE.md)](docs/RK3588_DEPLOYMENT_GUIDE.md)：NPU 算力分配、VPU 多路硬解、交叉编译与现场接线实施。
