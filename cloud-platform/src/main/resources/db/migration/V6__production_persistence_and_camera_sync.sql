-- V6: 生产级全量数据库持久化、多场地白名单隔离、摄像头 ROI 同步及取证审计增强

-- 1. 摄像头扩展字段：ROI 区域坐标与启用状态
ALTER TABLE chg_cameras ADD COLUMN IF NOT EXISTS roi_json TEXT DEFAULT '';
ALTER TABLE chg_cameras ADD COLUMN IF NOT EXISTS enabled BOOLEAN NOT NULL DEFAULT TRUE;

-- 2. 边缘节点映射表扩展：分配车位与远程重启标记
ALTER TABLE chg_node_site_mapping ADD COLUMN IF NOT EXISTS assigned_spaces_json TEXT DEFAULT '[]';
ALTER TABLE chg_node_site_mapping ADD COLUMN IF NOT EXISTS reboot_requested_at TIMESTAMPTZ;

-- 3. 违规事件扩展：确认人与确认时间
ALTER TABLE chg_violation_events ADD COLUMN IF NOT EXISTS acknowledged_by VARCHAR(64);
ALTER TABLE chg_violation_events ADD COLUMN IF NOT EXISTS acknowledged_at TIMESTAMPTZ;

-- 4. 白名单按停车场隔离 (site_id, plate)
ALTER TABLE chg_whitelist ADD COLUMN IF NOT EXISTS site_id VARCHAR(64) NOT NULL DEFAULT 'default';
ALTER TABLE chg_whitelist DROP CONSTRAINT IF EXISTS chg_whitelist_pkey;
ALTER TABLE chg_whitelist ADD PRIMARY KEY (site_id, plate);

-- 5. 车位实时状态持久化表
CREATE TABLE IF NOT EXISTS chg_space_runtime_status (
    site_id VARCHAR(64) NOT NULL,
    space_id VARCHAR(32) NOT NULL,
    node_id VARCHAR(64) NOT NULL,
    phase VARCHAR(32) NOT NULL,
    charger_status VARCHAR(32) NOT NULL,
    plate VARCHAR(32),
    vehicle_type VARCHAR(32) NOT NULL,
    alarming BOOLEAN NOT NULL DEFAULT FALSE,
    active_event_id VARCHAR(64),
    observed_at TIMESTAMPTZ NOT NULL,
    is_simulated BOOLEAN NOT NULL DEFAULT FALSE,
    stream_type VARCHAR(64) DEFAULT '',
    snapshot_url VARCHAR(512) DEFAULT '',
    confidence DOUBLE PRECISION DEFAULT 0.0,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (site_id, space_id)
);

-- 6. AI 车牌识别真实流水持久化表
CREATE TABLE IF NOT EXISTS chg_plate_recognition_records (
    record_id VARCHAR(64) PRIMARY KEY,
    site_id VARCHAR(64) NOT NULL,
    space_id VARCHAR(32) NOT NULL,
    camera_id VARCHAR(64) DEFAULT '',
    plate VARCHAR(32),
    plate_color VARCHAR(32) DEFAULT 'UNKNOWN',
    vehicle_type VARCHAR(32) NOT NULL DEFAULT 'UNKNOWN',
    confidence DOUBLE PRECISION NOT NULL DEFAULT 0.0,
    snapshot_url VARCHAR(512) DEFAULT '',
    direction VARCHAR(32) NOT NULL DEFAULT 'STATIONARY',
    judgment_status VARCHAR(64) NOT NULL DEFAULT 'NORMAL',
    is_simulated BOOLEAN NOT NULL DEFAULT FALSE,
    recognized_at TIMESTAMPTZ NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_chg_lpr_site_time
    ON chg_plate_recognition_records(site_id, recognized_at DESC);

-- 7. 配置下发应用回执持久化表
CREATE TABLE IF NOT EXISTS chg_configuration_reports (
    node_id VARCHAR(64) PRIMARY KEY,
    config_version BIGINT NOT NULL,
    status VARCHAR(32) NOT NULL,
    message TEXT DEFAULT '',
    reported_at TIMESTAMPTZ NOT NULL
);

-- 8. OTA 固件发布清单持久化表
CREATE TABLE IF NOT EXISTS chg_upgrade_manifests (
    version BIGINT PRIMARY KEY,
    version_name VARCHAR(64) NOT NULL,
    package_url VARCHAR(512) NOT NULL,
    sha256_hash VARCHAR(128) NOT NULL,
    signature VARCHAR(128) NOT NULL,
    min_compatible_version BIGINT NOT NULL DEFAULT 1,
    release_notes TEXT DEFAULT '',
    created_at TIMESTAMPTZ NOT NULL
);

-- 9. 边缘节点 OTA 升级状态报告持久化表
CREATE TABLE IF NOT EXISTS chg_upgrade_node_reports (
    node_id VARCHAR(64) PRIMARY KEY,
    current_version BIGINT NOT NULL,
    target_version BIGINT,
    state VARCHAR(32) NOT NULL,
    message TEXT DEFAULT '',
    last_error TEXT,
    updated_at TIMESTAMPTZ NOT NULL
);

-- 10. 边缘节点定向升级队列持久化表
CREATE TABLE IF NOT EXISTS chg_pending_node_upgrades (
    node_id VARCHAR(64) PRIMARY KEY,
    manifest_json TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

-- 11. Webhook 告警投递台账持久化表
CREATE TABLE IF NOT EXISTS chg_webhook_deliveries (
    record_id VARCHAR(64) PRIMARY KEY,
    event_id VARCHAR(64) NOT NULL,
    channel VARCHAR(32) NOT NULL,
    target_url VARCHAR(512) NOT NULL,
    success BOOLEAN NOT NULL,
    attempts INT NOT NULL,
    last_error TEXT,
    sent_at TIMESTAMPTZ NOT NULL,
    is_simulated BOOLEAN NOT NULL DEFAULT FALSE,
    data_source VARCHAR(128) DEFAULT ''
);
CREATE INDEX IF NOT EXISTS idx_chg_webhook_sent
    ON chg_webhook_deliveries(sent_at DESC);
