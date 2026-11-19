-- V4: 边缘工控机授权与真实硬件遥测、摄像头接入持久化

ALTER TABLE chg_node_site_mapping ADD COLUMN IF NOT EXISTS node_name VARCHAR(128) DEFAULT '';
ALTER TABLE chg_node_site_mapping ADD COLUMN IF NOT EXISTS auth_key VARCHAR(128) DEFAULT '';
ALTER TABLE chg_node_site_mapping ADD COLUMN IF NOT EXISTS ip_address VARCHAR(64) DEFAULT '';

ALTER TABLE chg_node_heartbeats ADD COLUMN IF NOT EXISTS cpu_usage DOUBLE PRECISION DEFAULT 0.0;
ALTER TABLE chg_node_heartbeats ADD COLUMN IF NOT EXISTS memory_usage DOUBLE PRECISION DEFAULT 0.0;
ALTER TABLE chg_node_heartbeats ADD COLUMN IF NOT EXISTS temperature DOUBLE PRECISION DEFAULT 0.0;
ALTER TABLE chg_node_heartbeats ADD COLUMN IF NOT EXISTS uptime_seconds BIGINT DEFAULT 0;
ALTER TABLE chg_node_heartbeats ADD COLUMN IF NOT EXISTS frames_processed BIGINT DEFAULT 0;
ALTER TABLE chg_node_heartbeats ADD COLUMN IF NOT EXISTS violations_detected BIGINT DEFAULT 0;
ALTER TABLE chg_node_heartbeats ADD COLUMN IF NOT EXISTS alarms_triggered BIGINT DEFAULT 0;
ALTER TABLE chg_node_heartbeats ADD COLUMN IF NOT EXISTS disk_watermark DOUBLE PRECISION DEFAULT 0.0;
ALTER TABLE chg_node_heartbeats ADD COLUMN IF NOT EXISTS version VARCHAR(32) DEFAULT 'v2.0.0';

CREATE TABLE IF NOT EXISTS chg_cameras (
    camera_id VARCHAR(64) NOT NULL,
    site_id VARCHAR(64) NOT NULL REFERENCES chg_sites(site_id) ON DELETE CASCADE,
    camera_name VARCHAR(128) NOT NULL,
    node_id VARCHAR(64),
    space_id VARCHAR(32),
    protocol VARCHAR(32) NOT NULL DEFAULT 'RTSP',
    stream_url VARCHAR(512) DEFAULT '',
    sip_device_id VARCHAR(128) DEFAULT '',
    resolution VARCHAR(32) DEFAULT '1920x1080',
    codec VARCHAR(32) DEFAULT 'H.264',
    status VARCHAR(32) DEFAULT 'ONLINE',
    last_frame_at TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (camera_id, site_id)
);
