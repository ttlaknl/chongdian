#!/usr/bin/env bash
set -e

BASE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$BASE_DIR"

echo "============================================================"
echo "  充电车位防占位预警系统 - Linux 生产环境一键启动"
echo "============================================================"

mkdir -p logs

echo "[1/2] 启动云端管控平台 (端口 8080)..."
nohup bash -c "cd cloud-platform && mvn spring-boot:run" > logs/cloud.log 2>&1 &
CLOUD_PID=$!
echo "云端平台已在后台启动 (PID: $CLOUD_PID)"

echo "等待云端服务准备就绪..."
sleep 5

echo "[2/2] 启动边缘节点服务 (端口 8081)..."
export EDGE_CLOUD_URL="http://localhost:8080"
nohup bash -c "cd edge-node && cargo run" > logs/edge.log 2>&1 &
EDGE_PID=$!
echo "边缘节点已在后台启动 (PID: $EDGE_PID)"

echo ""
echo "============================================================"
echo "  系统启动完成！"
echo "  大屏地址: http://localhost:8080/index.html"
echo "  云端接口: http://localhost:8080/api/v1/health"
echo "  边缘健康: http://localhost:8081/health"
echo "  日志路径: logs/cloud.log, logs/edge.log"
echo "============================================================"
