# 项目审计报告

审计日期：2026-09-08  
审计对象：`master` 分支，提交 `4d6c79a`

## 结论

当前代码是功能覆盖较广的云边原型，规则引擎、状态机、SQLite 缓存和基础管理接口已有较好的单元测试基础；但原开发计划将接口占位、模拟实现和生产完成混为一谈。项目适合继续迭代和无硬件联调，尚不具备生产部署或现场验收条件。

## 整改前验证结果

- Rust：`cargo test --offline`，33 项通过。
- Java：`mvn clean test`，7 项通过；`mvn clean package` 通过。
- Docker Compose：配置可解析，仅提示 `version` 字段已弃用。
- Docker 镜像：本机 Docker daemon 未运行，未完成镜像构建和容器健康检查。
- Rust 格式门禁：`cargo fmt --check` 未通过。
- Rust 静态检查：`cargo clippy --all-targets -- -D warnings` 未通过，存在 7 项格式/惯用法问题。
- Git 工作区：审计开始时无未提交改动。

## P0 整改后验证结果

- Rust：`cargo fmt --check` 通过。
- Rust：`cargo clippy --offline --all-targets -- -D warnings` 通过。
- Rust：38 个库测试、1 个配置集成测试通过。
- 无硬件模拟器：6 个业务场景、16 路继电器/语音仿真、1,000 条 SQLite 断网缓存测试通过。
- Java：`mvn clean test` 连续两次通过，8 个控制器测试、2 个存储测试通过；`mvn clean package` 通过。
- Compose：配置校验通过；本机 Docker daemon 未运行，未完成镜像启动验证。
- 边缘 HTTP 冒烟：白名单免报警、事件编号去重、恢复合规消警、实时状态和离线自治健康检查通过。
- 云端真实 HTTP 冒烟：受当前 Windows/JDK 环境无法建立 Java loopback selector 影响，未完成；MockMvc 已覆盖相同接口契约。

## 关键发现

### P0：白名单没有进入边缘判定

云端可维护白名单，但边缘检测和视觉流水线均将 `whitelisted` 固定为 `false`。白名单车辆在真实自动判定链路中仍会报警。需要下发白名单、边缘持久化，并根据车牌在判定前查询。

证据：`edge-node/src/main.rs` 的检测判定调用、`edge-node/src/pipeline.rs:303`。

### P0：配置热更新没有重绑定全部设备

启动时充电桩和继电器根据配置绑定；运行时 `apply_configuration` 只更新摄像头绑定。修改车位、桩号或继电器通道后，内存中的充电桩和继电器仍使用旧映射。需要为三类适配器提供统一的校验、应用、回滚接口。

证据：`edge-node/src/main.rs:68`、`edge-node/src/main.rs:101`、`edge-node/src/main.rs:520`。

### P0：云端 PostgreSQL 配置未接入业务

仓库有 SQL 脚本和数据库容器，但 Java 依赖中没有 JDBC/JPA/PostgreSQL 驱动，业务存储仍由 `CloudStore` 整体写 JSON 文件。Compose 还以 `default` profile 启动云端，因此 PostgreSQL 完全未被业务使用。

同时，应用写入相对路径 `/app/data/cloud_store`，Compose 却挂载 `/data/cloud_store`，容器重建可能丢失云端数据；持久化异常被全部吞掉，API 仍返回成功。

证据：`cloud-platform/pom.xml`、`cloud-platform/src/main/java/com/charging/cloud/CloudStore.java:26`、`cloud-platform/src/main/java/com/charging/cloud/CloudStore.java:187`、`deploy/docker-compose.yml:35`。

### P0：真实视频和 AI 流水线未进入主程序

当前 `FrameGrabber` 生成模拟字节，推理实现是 `MockInferenceEngine`。主程序只接受外部 HTTP 检测结果，没有启动 GB28181 SIP/RTP、解码或视觉流水线任务。摄像头 HTTP 注册/心跳接口不能等同于 GB28181 接入。

证据：`edge-node/src/pipeline.rs:39`、`edge-node/src/pipeline.rs:76`、`edge-node/src/pipeline.rs:117`、`edge-node/src/main.rs:167`。

### P0：RS-485 和硬件健康状态仍是模拟

`SerialPortTransport` 内部使用 `MockTransport`，没有打开真实串口；主程序的 `HardwareHealthMonitor` 也固定监控一个 mock 总线，因此健康状态不能反映真实继电器。主程序可选择 Modbus TCP，但语音双模控制器只在测试/模拟器中组装。

证据：`edge-node/src/hardware.rs:209`、`edge-node/src/hardware.rs:229`、`edge-node/src/main.rs:106`。

### P0：systemd watchdog 配置与程序不匹配

systemd 服务设置 `WatchdogSec=30s`，程序没有发送 `WATCHDOG=1` 的 `sd_notify`。同时，监控器初始化四个子系统后，主运行路径没有调用 `watchdog.feed`；运行约 30 秒后会判定全部子系统超时并停止喂硬件狗。当前配置可能造成 systemd 周期性重启或健康状态持续降级。

证据：`deploy/systemd/charging-edge.service:13`、`edge-node/src/watchdog.rs:67`、`edge-node/src/watchdog.rs:82`。

### P1：事件缓存存在双写和遗留文件

违规事件同时写入 SQLite 和 `EventSpool` 文件目录，但补传逻辑只从 SQLite 读取并标记同步，文件事件没有确认删除。长期运行会积累最多 10,000 个无用文件，并形成两套事实来源。

证据：`edge-node/src/main.rs:453`、`edge-node/src/main.rs:465`、`edge-node/src/main.rs:471`。

### P1：事件缺少生命周期，大屏会产生长期假报警

云端事件模型没有关闭时间或处理状态。大屏从最近 8 条历史事件推断车位是否正在报警，历史违规会一直显示为当前报警，车辆离场也无法从云端状态中体现。

证据：`cloud-platform/src/main/resources/static/index.html:329`、`cloud-platform/src/main/resources/static/index.html:341`。

### P1：证据链实现与命名不一致

名为 `fingerprint_sha256` 的字段实际存储 CRC 派生值；所谓水印是把文本追加到帧字节末尾，并未在真实图片上绘制。S3/OSS 上传没有使用 access key/secret 做签名，兼容性和安全性不足。本地证据 URL 也没有对应静态文件服务。

证据：`edge-node/src/pipeline.rs:164`、`edge-node/src/pipeline.rs:191`、`edge-node/src/storage.rs:67`。

### P1：生产 API 没有身份认证

规则、设备配置、事件、白名单和心跳接口均可匿名访问。任何可访问云端端口的客户端都能修改策略或伪造事件。边缘节点连接云端也没有独立设备身份和请求签名。

证据：`cloud-platform/src/main/java/com/charging/cloud/CloudController.java`、`cloud-platform/pom.xml`。

### P1：配置与事件输入校验不足

Java 端只校验绑定数量和非空字段，没有检查车位、摄像头、充电桩或继电器通道重复。云端可能接受一个边缘必然拒绝的版本，导致配置同步持续失败。事件接口也缺少完整字段校验和状态幂等约束。

### P2：大屏与部署仍是原型形态

前端通过公共 CDN 加载 Vue、Tailwind、ECharts 和图标，离线或受限网络环境无法稳定运行；页面没有构建、类型检查或自动化测试。Compose 固定暴露数据库端口、包含固定密码并以 privileged 方式运行边缘容器，需要按现场安全边界收敛。

### P2：部分文档声明超出现有实现

原计划把真实 S3、SHA-256、水印、RS-485、AI、PostgreSQL、watchdog、16 路压力和现场试运行标记为已完成。当前证据只支持协议/模拟测试，不能支持现场完成结论。开发计划已按“已验证、原型可用、接口占位、未开始”重置状态。

## 保留优势

- 业务规则边界清晰，燃油车、未充电、充电完成滞留、故障和未知状态均已有测试。
- 在位状态机具备进入/离开防抖和遮挡恢复逻辑。
- 边缘 SQLite 使用 WAL，并具备待同步事件查询和补传标记。
- 云边配置使用版本号，设备绑定限制为 1~16 路。
- Modbus TCP/RTU 报文、继电器通道隔离和报警自动停止已有模拟测试基础。
- 云端具备配置、事件、白名单、统计和 CSV 导出的原型接口。

## 审计后的开发原则

1. 先修运行闭环与数据正确性，再接真实硬件。
2. 模拟通过、契约通过、设备实测和现场验收分开记录。
3. 生产模式不得静默回退 mock；降级状态必须可见并告警。
4. 每个“完成”项必须对应自动化测试、集成测试或现场记录。
5. 单一事实来源：事件补传、配置、实时状态和证据元数据分别只保留一个权威存储。

## P0 整改状态（2026-09-08）

本报告记录的是整改前基线。当前工作树已完成 P0 运行闭环：白名单下发与边缘判定、三类设备热重绑定与失败回滚、事件生命周期和会话去重、SQLite 单一补传队列、实时车位状态上报、大屏实时状态切换、配置应用回报、Rust 格式/Clippy 门禁和 CI。systemd 的 `WatchdogSec` 已暂时移除，待实现 `sd_notify` 后再启用。

## P1 整改状态（2026-09-08）

P1 整改已经完成：PostgreSQL/JDBC/Flyway profile、事务写入、审计记录、管理员 HttpOnly 会话、管理员/边缘 API key、边缘 HMAC 60 秒防重放、数据库异常 503 响应、非 root 镜像、生产 Compose 密钥变量、前端本地构建资源和 Testcontainers PostgreSQL 用例均已加入。边缘 HTTP 客户端使用 Rustls，生产 Alpine 镜像不依赖动态 OpenSSL。

P1 收口后新增了 API key/HMAC 回归测试、数据库时间类型处理、审计查询、前端离线构建和 Compose 环境变量校验。Java 当前 21 个测试通过，Rust 当前 38 个库测试和 3 个边缘服务测试通过。

真实 PostgreSQL 16 和生产 Compose 已在本机完成验收：两个生产镜像构建成功，云端、边缘节点和数据库三个容器正常启动；Flyway 成功应用 1 个迁移且重复启动保持幂等；公开健康检查返回 200，未授权管理请求返回 401，管理员仪表盘、事件和审计接口返回 200；边缘心跳、6 个实时车位状态和配置版本 1 的 `APPLIED` 回报到达云端。数据库重启前后记录保持为 Flyway 1 条、配置 2 条、心跳 1 条、审计 1 条，重启后云端健康与仪表盘接口仍返回 200。另一个隔离 PostgreSQL 集成验收已验证事件、白名单、配置、心跳和审计数据在容器重启后全部保留。
