-- V2: 多停车场 + 账号密码登录 数据库迁移
-- 执行时机：Flyway 自动在 V1 之后运行

-- 1. 停车场主表
CREATE TABLE IF NOT EXISTS chg_sites (
    site_id VARCHAR(64) PRIMARY KEY,
    site_name VARCHAR(128) NOT NULL,
    address VARCHAR(256) DEFAULT '',
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    enabled BOOLEAN NOT NULL DEFAULT TRUE
);

-- 2. 边缘节点-停车场绑定表（多对一：一个节点属于一个停车场）
CREATE TABLE IF NOT EXISTS chg_node_site_mapping (
    node_id VARCHAR(64) PRIMARY KEY,
    site_id VARCHAR(64) NOT NULL REFERENCES chg_sites(site_id) ON DELETE CASCADE,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

-- 3. 用户账号表（账号密码登录）
CREATE TABLE IF NOT EXISTS chg_users (
    username VARCHAR(64) PRIMARY KEY,
    password_hash VARCHAR(255) NOT NULL,
    role VARCHAR(32) NOT NULL,
    display_name VARCHAR(128),
    enabled BOOLEAN NOT NULL DEFAULT TRUE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    last_login_at TIMESTAMPTZ
);
CREATE INDEX IF NOT EXISTS idx_chg_users_role ON chg_users(role);

-- 4. 扩展配置表：增加 site_id 列
ALTER TABLE chg_platform_configuration ADD COLUMN IF NOT EXISTS
    site_id VARCHAR(64) DEFAULT 'default';
UPDATE chg_platform_configuration SET site_id = 'default' WHERE site_id IS NULL;

-- 5. 扩展事件表：增加 site_id 列（带索引）
ALTER TABLE chg_violation_events ADD COLUMN IF NOT EXISTS
    site_id VARCHAR(64) DEFAULT 'default';
CREATE INDEX IF NOT EXISTS idx_chg_events_site ON chg_violation_events(site_id);

-- 6. 扩展心跳表：增加 site_id 列
ALTER TABLE chg_node_heartbeats ADD COLUMN IF NOT EXISTS
    site_id VARCHAR(64);

-- 7. 初始化默认停车场（迁移现有数据用）
INSERT INTO chg_sites(site_id, site_name, address)
    VALUES ('default', '默认停车场', '')
    ON CONFLICT (site_id) DO NOTHING;
