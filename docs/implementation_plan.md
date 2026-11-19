# P5 阶段技术实施方案：运维、升级与系统验收、集成测试和冒烟测试

本项目已圆满完成 P0~P4 阶段的全部核心工程开发与自动化测试验证。现正式制定 **P5（运维、升级与系统验收、集成测试和冒烟测试）** 的工程实施方案。

---

## User Review Required

> [!IMPORTANT]
> **P5 关键验收与工程设计要点**：
> 1. **看门狗与 systemd `sd_notify` 深度集成**：
>    - 边缘节点支持检测环境变量 `NOTIFY_SOCKET`，在服务就绪时发送 `READY=1`，主循环正常 tick 时发送 `WATCHDOG=1`；
>    - 接入 Linux 真实 `/dev/watchdog` 驱动规范，严格实现 **Magic Close 安全关闭语义**：进程正常退出（SIGTERM/SIGINT）时向设备写入字符 `'V'`，通知内核看门狗安全卸载，避免非故障重启；
>    - 更新 `charging-edge.service` 为 `Type=notify`，启用 `WatchdogSec=15s`。
> 2. **签名远程升级（OTA）与自动健康回滚机制**：
>    - 定义升级清单 `UpgradeManifest`（包含版本号、包下载地址、SHA-256 校验和、HMAC/公钥签名、最低兼容版本）；
>    - 状态机推进：`Idle -> Preparing -> Verifying -> Staging -> HealthChecking -> Committed / RolledBack`；
>    - 实施双版本/备份回滚：升级应用新二进制文件后执行自检探针（测试 SQLite 数据库完整性、规则加载、核心硬件健康），若失败或看门狗告警，立即自动还原备份版本并记录审计告警。
> 3. **系统指标采集、磁盘水位告警与时间同步监控**：
>    - 实现轻量级遥测模块 `metrics.rs`，实时采集磁盘水位（使用率超过 90% 触发预警）、16 路视频帧率、事件处理延时与时钟漂移；
>    - 提供 `GET /api/v1/metrics`（Prometheus 格式）与 `GET /api/v1/telemetry`（JSON 格式）。
> 4. **16 路满载压力、故障注入与冒烟测试**：
>    - 编写 16 路车位并发满载吞吐测试；
>    - 编写 5 组关键故障注入测试：断电/异常杀进程从 SQLite 恢复、断网云边分区补传、运营商接口超时降级、Modbus 继电器掉线重连、磁盘配额占满 FIFO 清理；
>    - 编写可独立运行的端到端冒烟测试工具；
>    - 输出 168 小时等效时间加速压力与稳定性报告（`docs/STABILITY_REPORT_168H.md`）。

---

## Open Questions

无阻塞性疑问。本设计遵循 Linux Watchdog API 驱动规范、systemd 服务通知规范与工业 OTA 升级安全准则。

---

## Proposed Changes

### 一、看门狗与系统守护加固 (`edge-node`)

#### [MODIFY] [edge-node/src/watchdog.rs](file:///d:/clude/chongdian/edge-node/src/watchdog.rs)
- 增加 `sd_notify` 支持：跨平台安全读取 `NOTIFY_SOCKET`，发送 `READY=1`、`WATCHDOG=1`、`STOPPING=1`；
- 实现 Linux `/dev/watchdog` Magic Close 机制：
  - 打开设备后循环周期喂狗（写入 `b"1"`）；
  - 在 `stop()` 或进程退出时写入字符 `b"V"`（Magic Character），解除硬件定时器；
- 扩展子系统健康维度（增加 pipeline、hardware、operator、sqlite、metrics）。

#### [MODIFY] [deploy/systemd/charging-edge.service](file:///d:/clude/chongdian/deploy/systemd/charging-edge.service)
- 将 `Type=simple` 更新为 `Type=notify`；
- 配置 `WatchdogSec=15s`；
- 配置 `Restart=on-failure` 与 `RestartSec=3s`。

---

### 二、远程签名升级与自愈回滚 (`edge-node` & `cloud-platform`)

#### [NEW] [edge-node/src/upgrade.rs](file:///d:/clude/chongdian/edge-node/src/upgrade.rs)
- 定义 `UpgradeManifest`、`UpgradeStatus`、`UpgradeState`；
- 实现签名校验（SHA-256 哈希校验 + HMAC-SHA256 数字签名）；
- 实现安全就地升级与回滚：
  - 暂存当前运行文件为 `.backup`；
  - 写入新版本二进制；
  - 执行 `health_check()` 自检（数据库访问、配置校验、内存检查）；
  - 自检成功则提交生效并删除备份，自检失败则原子恢复 `.backup` 并上报 `RolledBack`。

#### [MODIFY] [cloud-platform/src/main/java/com/charging/cloud/CloudController.java](file:///d:/clude/chongdian/cloud-platform/src/main/java/com/charging/cloud/CloudController.java)
- 增加节点升级包分发与状态查询接口：
  - `POST /api/v1/upgrade/manifest`：发布最新版本升级清单；
  - `GET /api/v1/upgrade/manifest`：获取当前待升级清单；
  - `POST /api/v1/nodes/{nodeId}/upgrade-status`：边缘回报升级进度与回滚状态。

---

### 三、指标采集与磁盘水位预警 (`edge-node`)

#### [NEW] [edge-node/src/metrics.rs](file:///d:/clude/chongdian/edge-node/src/metrics.rs)
- 统计指标采集：
  - 系统启动时间与持续运行时间（Uptime）；
  - 各车位帧处理计数与平均推理耗时；
  - 本地 SQLite 缓存未同步事件计数；
  - 磁盘空间总大小、已用大小、可用大小及使用率百分比（水位）；
  - 磁盘水位 > 90% 时触发告警回调。
- 格式化输出：Prometheus 文本格式与 JSON 结构体。

#### [MODIFY] [edge-node/src/main.rs](file:///d:/clude/chongdian/edge-node/src/main.rs)
- 挂载 `GET /api/v1/metrics` 与 `GET /api/v1/telemetry`；
- 挂载 `POST /api/v1/upgrade/apply` 端点。

---

### 四、集成测试、故障注入与冒烟测试套件

#### [NEW] [edge-node/tests/concurrency_16ch_test.rs](file:///d:/clude/chongdian/edge-node/tests/concurrency_16ch_test.rs)
- 16 路车位满载测试：模拟 16 个车位同时接入并发视频帧、车辆进入、离场与充电状态同步，验证多线程无死锁、无内存泄漏。

#### [NEW] [edge-node/tests/fault_injection_test.rs](file:///d:/clude/chongdian/edge-node/tests/fault_injection_test.rs)
- 模拟 5 项工业现场真实故障：
  1. 进程突然被杀（模拟断电）后重启，SQLite 幂等恢复与未完成事件重传；
  2. 云端断网时边缘自主声光驱离，网络恢复后补传；
  3. 充电桩运营商 API 接口延迟与陈旧性降级；
  4. 硬件继电器断线，严格模式立即报错，非严格模式显式打标降级；
  5. 证据存储磁盘配额满，FIFO 自动清理老旧文件及 Sidecar JSON。

#### [NEW] [edge-node/src/bin/smoke_test.rs](file:///d:/clude/chongdian/edge-node/src/bin/smoke_test.rs)
- 独立冒烟测试执行器，一键校验边缘健康、云端通信、SSE 事件推流、看门狗、配额清理及指标端点。

#### [NEW] [docs/STABILITY_REPORT_168H.md](file:///d:/clude/chongdian/docs/STABILITY_REPORT_168H.md)
- 168 小时等效加速压力测试记录与稳定性验收报告。

---

## Verification Plan

### Automated Tests
1. **Rust 单元与集成测试**：
   ```powershell
   cargo test --offline
   cargo clippy --offline --all-targets -- -D warnings
   cargo fmt --check
   ```
2. **Java 云端测试**：
   ```powershell
   mvn test
   ```
3. **冒烟测试验证**：
   ```powershell
   cargo run --bin smoke_test --offline
   ```

### Manual Verification
- 检查 `docs/` 目录下全部文档与报告；
- 验证 systemd 服务单元配置语法与环境对齐。
