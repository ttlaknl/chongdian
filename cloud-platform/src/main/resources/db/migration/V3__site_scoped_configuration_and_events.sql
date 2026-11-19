CREATE TABLE IF NOT EXISTS chg_site_configuration (
    site_id VARCHAR(64) NOT NULL REFERENCES chg_sites(site_id) ON DELETE CASCADE,
    config_version BIGINT NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL,
    rules_json TEXT NOT NULL,
    bindings_json TEXT NOT NULL,
    whitelist_json TEXT NOT NULL DEFAULT '[]',
    PRIMARY KEY (site_id, config_version)
);

CREATE INDEX IF NOT EXISTS idx_chg_site_configuration_latest
    ON chg_site_configuration(site_id, config_version DESC);

INSERT INTO chg_site_configuration(
    site_id, config_version, updated_at, rules_json, bindings_json, whitelist_json)
SELECT DISTINCT ON (site_id)
    site_id, 1, updated_at, rules_json, bindings_json, whitelist_json
FROM chg_platform_configuration
WHERE site_id IS NOT NULL AND site_id <> 'default'
ORDER BY site_id, updated_at DESC
ON CONFLICT (site_id, config_version) DO NOTHING;

UPDATE chg_violation_events SET site_id = 'default' WHERE site_id IS NULL;
ALTER TABLE chg_violation_events ALTER COLUMN site_id SET DEFAULT 'default';
ALTER TABLE chg_violation_events ALTER COLUMN site_id SET NOT NULL;

DROP INDEX IF EXISTS idx_chg_events_session;
CREATE UNIQUE INDEX IF NOT EXISTS idx_chg_events_site_session
    ON chg_violation_events(site_id, session_id)
    WHERE session_id IS NOT NULL AND session_id <> '';
