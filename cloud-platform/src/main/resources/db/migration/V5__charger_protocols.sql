-- V5: 充电桩接口协议管理与车位品牌配置式接入

CREATE TABLE IF NOT EXISTS chg_charger_protocols (
    protocol_id VARCHAR(64) PRIMARY KEY,
    brand_name VARCHAR(128) NOT NULL,
    protocol_category VARCHAR(32) NOT NULL DEFAULT 'HTTP_API',
    description TEXT DEFAULT '',
    default_port INT DEFAULT 80,
    connection_config_schema TEXT DEFAULT '{}',
    status_mapping TEXT DEFAULT '{}',
    data_parser_rules TEXT DEFAULT '{}',
    is_builtin BOOLEAN NOT NULL DEFAULT FALSE,
    enabled BOOLEAN NOT NULL DEFAULT TRUE,
    created_at TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP
);

-- 预置 6 大主流内置品牌协议 (特来电、星星充电、国家电网、云快充、通用Modbus-TCP、标准OCPP 1.6-J)
INSERT INTO chg_charger_protocols (
    protocol_id, brand_name, protocol_category, description, default_port,
    connection_config_schema, status_mapping, data_parser_rules, is_builtin, enabled
) VALUES
(
    'proto-teld',
    '特来电 (TELD)',
    'HTTP_API',
    '特来电开放云平台 OpenAPI 协议，支持多桩状态实时推送与高频主动轮询',
    443,
    '{"operatorId":"MA123456","appId":"teld_app_01","appSecret":"******","stationId":"110108001","pollIntervalSeconds":5}',
    '{"1":"IDLE","2":"PREPARING","3":"CHARGING","4":"COMPLETED","255":"FAULT"}',
    '{"statusPath":"$.data.connectorStatus","socPath":"$.data.soc","powerPath":"$.data.powerKw","voltagePath":"$.data.voltage","currentPath":"$.data.current"}',
    TRUE,
    TRUE
),
(
    'proto-starcharge',
    '星星充电 (StarCharge)',
    'HTTP_API',
    '万帮星星充电开放互联规约，支持标准 REST 签名校验与 MQTT 实时工况流',
    443,
    '{"operatorId":"310000000","operatorSecret":"******","stubId":"ST-001","connectorId":"1","pollIntervalSeconds":5}',
    '{"0":"OFFLINE","1":"IDLE","2":"CHARGING","3":"COMPLETED","4":"FAULT"}',
    '{"statusPath":"$.connectorStatusInfo.status","socPath":"$.connectorStatusInfo.soc","powerPath":"$.connectorStatusInfo.power","voltagePath":"$.connectorStatusInfo.voltage","currentPath":"$.connectorStatusInfo.current"}',
    TRUE,
    TRUE
),
(
    'proto-stategrid',
    '国家电网 (StateGrid)',
    'HTTP_API',
    '符合中电联 GB/T 32694 电动汽车充换电服务信息交换规范及车联网互联协议',
    8080,
    '{"operatorId":"91110000717882239X","operatorSecret":"******","equipmentId":"10000000000001","connectorId":"1"}',
    '{"1":"IDLE","2":"PREPARING","3":"CHARGING","4":"COMPLETED","5":"FAULT","255":"OFFLINE"}',
    '{"statusPath":"$.connectorStatusInfo.status","socPath":"$.connectorStatusInfo.soc","powerPath":"$.connectorStatusInfo.power"}',
    TRUE,
    TRUE
),
(
    'proto-ykc',
    '云快充 (YKC 1.5/1.6)',
    'TCP_GATEWAY',
    '南京云快充通用物联协议，支持主流第三方运营商聚合平台与网关接入',
    9000,
    '{"gatewayIp":"192.168.1.210","gatewayPort":9000,"pileCode":"320100000001","gunNo":1}',
    '{"0":"IDLE","1":"CHARGING","2":"COMPLETED","3":"FAULT","4":"PREPARING"}',
    '{"statusPath":"$.data.gunStatus","socPath":"$.data.soc","powerPath":"$.data.realPower"}',
    TRUE,
    TRUE
),
(
    'proto-modbus-tcp',
    '通用 Modbus-TCP 电桩',
    'MODBUS_TCP',
    '支持标准工业以太网 Modbus-TCP 直连各类直流充电机、交流充电桩控制器与智能电表',
    502,
    '{"host":"192.168.1.200","port":502,"slaveId":1,"statusRegister":40001,"socRegister":40002,"powerRegister":40003,"voltageRegister":40004,"currentRegister":40005}',
    '{"0":"IDLE","1":"PREPARING","2":"CHARGING","3":"COMPLETED","4":"FAULT"}',
    '{"statusRegister":40001,"socRegister":40002,"powerRegister":40003,"voltageRegister":40004,"currentRegister":40005}',
    TRUE,
    TRUE
),
(
    'proto-ocpp-16j',
    '国际标准 OCPP 1.6-J',
    'OCPP_JSON',
    'Open Charge Point Protocol 1.6-J 国际开放充电协议，支持 WebSocket JSON 格式',
    9220,
    '{"centralSystemUrl":"ws://localhost:9220/ocpp","chargePointId":"CP-001","connectorId":1}',
    '{"Available":"IDLE","Preparing":"PREPARING","Charging":"CHARGING","Finishing":"COMPLETED","SuspendedEVSE":"COMPLETED","Faulted":"FAULT","Unavailable":"OFFLINE"}',
    '{"statusPath":"$.status","socPath":"$.soc","powerPath":"$.meterValue.power"}',
    TRUE,
    TRUE
)
ON CONFLICT (protocol_id) DO NOTHING;
