# 云端平台管理员操作后台、权限控制与免密业务大屏实施方案

本方案旨在响应用户的三大核心诉求：
1. **云端管理员操作后台**：构建功能完备、界面专业美观的云端管理员控制台（`admin.html`），实现基于云端下发的全量边缘策略（车位与硬件总线绑定、防占位研判规则、特权车白名单、固件与算法 OTA）；
2. **管理员后台权限控制**：建立清晰的分层权限机制（超级管理员 `ADMIN`、值班操作员 `OPERATOR`、免密访客 `GUEST`），受限管理 API（策略下发、规则修改、白名单维护、审计查看、消警处置）强制校验权限与会话；
3. **业务监控大屏免密公开**：优化 `ApiKeyAuthenticationFilter`，使业务监控大屏（`index.html`）及其依赖的实时感知数据流（车位状态、统计指标、违规流、SSE 实时长连接）完全公开，任何人均可自由查看，无需输入任何访问密钥。

---

## 一、需求与技术架构分析

```
+-------------------------------------------------------------------------------+
|                             用户访问与权限分流架构                            |
+-------------------------------------------------------------------------------+
                                        |
                 +----------------------+----------------------+
                 |                                             |
                 v                                             v
     [业务监控大屏 (index.html)]                   [管理员操作后台 (admin.html)]
   - 任何人均可免密访问 (GUEST)                  - 需凭据登录鉴权 (Session / API Key)
   - 实时感知车位矩阵 (6~16路)                   - 具备多级权限控制 (ADMIN / OPERATOR)
   - 实时 SSE 状态与事件推流                     - 页面路由与 API 访问双重守卫
   - 违规统计大屏与环形占比图
                 |                                             |
                 v (免密公开 GET)                              v (权限受控 PUT/POST/DELETE)
+-------------------------------------------------------------------------------+
|                   云端平台后端 (Spring Boot 3.4 / Java 21)                     |
|                                                                               |
|  [公开大屏 API - isPublic()]                [受控管理 API - roleFor()]         |
|  - GET /api/v1/dashboard/stats             - PUT  /api/v1/configuration (ADMIN)|
|  - GET /api/v1/spaces/status               - PUT  /api/v1/rules (ADMIN)        |
|  - GET /api/v1/events (只读)                - POST /api/v1/whitelist (ADMIN)    |
|  - GET /api/v1/stream/spaces (SSE)         - DEL  /api/v1/whitelist/* (ADMIN)  |
|  - GET /api/v1/stream/events (SSE)         - GET  /api/v1/audit (ADMIN)        |
|  - GET /api/v1/configuration (只读)         - POST /api/v1/events/*/ack (OPER)  |
|  - GET /api/v1/auth/session                - POST /api/v1/upgrade/* (ADMIN)    |
|                                                                               |
|  [边缘节点通信 API - EDGE HMAC]                                                |
|  - POST /api/v1/nodes/*/heartbeat                                             |
|  - POST /api/v1/nodes/*/spaces/status                                         |
|  - POST /api/v1/events                                                        |
+-------------------------------------------------------------------------------+
                                        | (策略下发与配置热同步)
                                        v
+-------------------------------------------------------------------------------+
|               边缘工控机集群 (edge-node, RK3588/x86 NPU)                        |
|  - 定时轮询 / 接收云端 PlatformConfiguration 版本跳变                          |
|  - 毫秒级热加载：6~16路相机绑定、桩位映射、Modbus继电器通道、研判规则、白名单    |
+-------------------------------------------------------------------------------+
```

---

## 二、拟定变更清单 (Proposed Changes)

### 1. 云端后端权限安全层与控制器 (Cloud Security & Auth)
#### [MODIFY] [ApiKeyAuthenticationFilter.java](file:///D:/clude/chongdian/cloud-platform/src/main/java/com/charging/cloud/ApiKeyAuthenticationFilter.java)
- 更新 `isPublic(request)`：
  - 允许大屏所需只读接口免密访问：`GET /api/v1/dashboard/stats`、`GET /api/v1/spaces/status`、`GET /api/v1/events`、`GET /api/v1/stream/spaces`、`GET /api/v1/stream/events`、`GET /api/v1/configuration`、`GET /api/v1/auth/session`。
- 完善 `roleFor(request)` 与权限匹配：
  - `ADMIN`：具备全部权限（下发配置 `PUT /api/v1/configuration`、修改规则 `PUT /api/v1/rules`、白名单增删、查看审计日志 `GET /api/v1/audit`、OTA 升级下发）；
  - `OPERATOR`：具备值班消警权限（`POST /api/v1/events/{id}/acknowledge`）、导出违规台账、查看节点状态；
  - `EDGE`：具备边缘节点心跳与事件上报权限（校验 HMAC 时间戳与签名）。

#### [MODIFY] [AuthController.java](file:///D:/clude/chongdian/cloud-platform/src/main/java/com/charging/cloud/AuthController.java)
- 支持多角色登录识别（`ADMIN` 与 `OPERATOR`），支持注入 `operatorKey`；
- 新增 `GET /api/v1/auth/session` 接口：返回当前会话认证状态、角色名与权限清单，供前端页面统一识别渲染。

---

### 2. 云端管理员操作后台开发 (Admin Management Console UI)
#### [NEW] [admin.html](file:///D:/clude/chongdian/cloud-platform/src/main/resources/static/admin.html)
基于 Vue 3 + Tailwind CSS + FontAwesome 构建专业工程化管理员操作后台，包含六大核心模块：
1. **统一鉴权守卫 (Auth Guard)**：
   - 未登录时展示深色工业风管理员登录界面，支持超级管理员 (`ADMIN`) 与值班操作员 (`OPERATOR`) 认证；
   - 顶部展示当前登录身份、角色标签、退出登录与「返回业务监控大屏」快捷按钮。
2. **车位与边缘硬件绑定管理 (Space & Hardware Bindings)**：
   - 查看全量 6~16 路车位绑定拓扑（车位号、站点、关联摄像头 RTSP/国标 SIP ID、充电桩 ID、Modbus 继电器通道 0~15、启用状态）；
   - 新增车位、编辑车位、删除车位、启用/禁用一键切换；
   - **一键「下发至边缘工控机」**：保存并自增配置版本号，触发云端审计与边缘热同步。
3. **边缘研判策略配置 (Edge Detection & Alarm Rules)**：
   - 规则参数表单：新能源未充宽限期、充完滞留宽限期、入位/离位确认时间、报警重呼间隔、声光防扰民自动消警上限；
   - 异常状态研判行为：未知车型处理策略、充电桩未知状态策略、故障桩豁免策略；
   - 预设策略模板（如「商业收费站严格模式」、「高速服务区高流转模式」、「政企内部站宽松模式」一键应用）；
   - **一键「保存并下发策略」**。
4. **特权白名单策略管理 (Privileged Whitelist)**：
   - 车牌号、动力类型、登记原因、有效期设定、启用状态；
   - 录入、批量查询、快速删除、状态切换。
5. **违规事件处置与台账 (Violation Events Ledger)**：
   - 支持车位筛选、违规类型筛选、报警/已消警状态过滤；
   - 现场核查与人工消警操作（带消警备注与经办人记录）；
   - CSV 历史台账导出。
6. **操作审计日志与边缘监管 (Audit & Edge Nodes)**：
   - 实时审计流水表格：操作人、动作、资源对象、详细信息、时间；
   - 边缘工控机心跳状态、配置同步版本、存储水位与健康度监控。

---

### 3. 业务监控大屏免密优化 (Public Monitor Big Screen)
#### [MODIFY] [index.html](file:///D:/clude/chongdian/cloud-platform/src/main/resources/static/index.html)
- 移除大屏默认数据加载（统计、车位状态、事件流、SSE）中的 API Key 强制要求；
- 任何人进入大屏均自动免密加载数据并开启 SSE 实时推送；
- 顶部导航栏增加醒目的「管理员后台」入口按钮（已登录显示身份，未登录点击一键跳转 `admin.html` 登录）；
- 优化一键消警交互：未登录管理员时点击消警弹出快捷登录弹窗或引导至管理后台。

---

### 4. 自动化测试与验证
#### [MODIFY] [ApiKeyAuthenticationFilterTest.java](file:///D:/clude/chongdian/cloud-platform/src/test/java/com/charging/cloud/ApiKeyAuthenticationFilterTest.java)
- 更新单元测试，断言大屏只读 API（`GET /api/v1/dashboard/stats`、`GET /api/v1/spaces/status`、`GET /api/v1/events` 等）允许免密公开访问；
- 验证写操作与管理操作（`PUT /api/v1/configuration`、`PUT /api/v1/rules`、`POST /api/v1/whitelist` 等）被非管理员请求拦截并返回 401；
- 验证 `OPERATOR` 角色权限隔离。

#### [NEW] [docs/ADMIN_PORTAL_GUIDE.md](file:///D:/clude/chongdian/docs/ADMIN_PORTAL_GUIDE.md)
- 归档详细的《云端平台管理员操作后台与权限系统用户手册》，包含角色定义、操作指南、边缘策略下发机制与免密大屏说明。

---

## 三、验证方案 (Verification Plan)

### 1. 自动化单元与集成测试
- 执行 `mvn test`（验证 27+ 项 Java 测试全部通过，特别是鉴权过滤器与安全角色）；
- 执行 `cargo test`（验证 70 项 Rust 边缘测试持续健康）。

### 2. 端到端功能与免密验证
- **免密访问测试**：使用匿名 `curl`（不带任何 Header 与 Cookie）请求 `GET /api/v1/dashboard/stats`、`GET /api/v1/spaces/status`、`GET /api/v1/events`、`GET /api/v1/stream/spaces`，确认返回 HTTP 200 且数据正常；
- **越权拦截测试**：匿名发起 `PUT /api/v1/configuration` 或 `POST /api/v1/whitelist`，确认返回 HTTP 401 `{"error":"unauthorized"}`；
- **管理员登录与策略下发测试**：通过 `/api/v1/auth/login` 获取会话，新增绑定车位 A-07 并调整宽限时间至 800 秒，确认云端版本号自增，边缘机成功同步；
- **监控大屏与管理后台界面联动测试**：在大屏免密浏览，点击进入管理员后台完成登录与策略下发。
