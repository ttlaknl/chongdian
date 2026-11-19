# 系统升级总结：账号密码登录与多停车场支持

## 1. 完成的核心需求

### 需求 1：后台使用账号、密码登录
- **认证模式升级**：由原单一 API Key 共享密钥输入框，全面升级为标准的「账号/用户名 + 密码」登录体系。
- **角色体系**：
  - 超级管理员：`ADMIN`（默认账号 `admin`，密码由环境变量 `ADMIN_PASSWORD` 配置）。
  - 值班操作员：`OPERATOR`（默认账号 `operator`，密码由环境变量 `OPERATOR_PASSWORD` 配置）。
- **边缘节点安全性保持**：现场工控机（RK3588）继续使用 `EDGE_API_KEY` + HMAC-SHA256 防重放时间戳签名机制进行机器间通信，确保设备上报与下发的最高安全规格。
- **登录界面与交互**：
  - `admin.html` 登录卡片适配用户名 + 密码双输入框，带有明暗文切换眼图。
  - 登录后建立 Spring Session Cookie 会话，安全清除前端 `localStorage` 明文密钥，实现安全闭环。

### 需求 2：系统适配多停车场 (Multi-Parking-Lot)
- **多停车场拓扑支持**：
  - 新增 `chg_sites` 停车场主表和 `chg_node_site_mapping` 节点绑定关系表。
  - 平台配置 `chg_platform_configuration` 及违规事件 `chg_violation_events` 均支持 `site_id` 作用域。
- **全链路 REST API 扩展**：
  - `GET /api/v1/sites`：列出所有停车场（免密公开）。
  - `POST /api/v1/sites`：创建停车场（需要 `ADMIN` 权限）。
  - `GET /api/v1/sites/{siteId}/configuration` 与 `PUT`：按场地独立获取和下发车位硬件绑定策略。
  - `GET /api/v1/sites/{siteId}/rules` 与 `PUT`：按场地独立配置违规研判与超时宽限规则。
  - `GET /api/v1/dashboard/stats?siteId=...`：支持单停车场看板指标与全网聚合指标。
  - `GET /api/v1/events?siteId=...`：支持按停车场过滤违规事件台账。
- **边缘向下兼容保障**：
  - 保持原 `GET /api/v1/configuration` 端点可用，当边缘节点携带 `X-Node-Id` 头时，后端自动透明路由到对应停车场的配置。
- **双端 UI 适配**：
  - **管理后台 `admin.html`**：
    - 顶部导航栏增加「当前停车场」全局下拉切换器。
    - 左侧新增「多停车场管理」Tab 模块，展示所有停车场卡片，支持直接新建与跳转管理。
    - 车位绑定和研判规则根据当前选中的停车场动态读取与持久化保存。
  - **监控大屏 `index.html`**：
    - 顶部 Header 增加「停车场」选择器，支持「全部停车场汇总」或「指定停车场单场监控」，KPI 统计、事件流与车位拓扑随之联动。

---

## 2. 验证与测试结果

1. **单元测试与集成测试**：
   - 运行 `mvn test`：所有 27 个测试用例全部通过（1 个预置的纯外部数据库测试按设计跳过）。
2. **Docker 环境全链路联调**：
   - 启动 PostgreSQL 16 容器，Flyway 自动执行 `V2__multi_site_and_users.sql` 迁移，版本成功升至 `v2`。
   - 验证账号密码登录接口：
     - 输入错误密码：正确返回 `401 Unauthorized`。
     - 输入正确账号密码（`admin / <配置的管理员密码>`）：成功生成 Session，返回角色 `ADMIN` 与 `canConfigure: true`。
   - 验证多停车场 API：
     - 创建新停车场 `lot-guangzhou-01`：返回 201 Created，DB 持久化成功。
     - 查询 `GET /api/v1/sites`：返回默认场地与新建场地列表。
     - 查询 `GET /api/v1/sites/lot-guangzhou-01/configuration`：正常返回该停车场的策略配置。
   - 验证端点连通性：
     - 所有前端页面及公开接口（包括 `/`、`/admin.html`、`/api/v1/sites`、`/api/v1/dashboard/stats` 等）HTTP 状态码均为 200 OK。
   - 验证边缘工控机节点：
     - `chongdian-edge` 与云端平台正常联通，事件上传、心跳维持与策略版本同步无阻碍。
