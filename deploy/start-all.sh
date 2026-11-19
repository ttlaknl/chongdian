#!/usr/bin/env bash
set -euo pipefail

BASE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$BASE_DIR"

echo "============================================================"
echo "  充电车位防占位预警系统 - 本地源码开发一键启动"
echo "============================================================"

mkdir -p logs

if [ -f "deploy/.env" ]; then
  set -a
  # shellcheck disable=SC1091
  source "deploy/.env"
  set +a
fi

export EDGE_CLOUD_URL="${EDGE_CLOUD_URL:-http://127.0.0.1:8080}"
export EDGE_VISION_MODE="${EDGE_VISION_MODE:-external}"
export EDGE_AI_ENGINE="${EDGE_AI_ENGINE:-python-hyperlpr3}"
export EDGE_HARDWARE_MODE="${EDGE_HARDWARE_MODE:-mock}"
export EDGE_STRICT_HARDWARE="${EDGE_STRICT_HARDWARE:-false}"
export EDGE_DATA_DIR="${EDGE_DATA_DIR:-$BASE_DIR/edge-node/data}"

echo "[1/3] 启动云端管控平台 (端口 8080)..."
nohup bash -c "cd '$BASE_DIR/cloud-platform' && mvn spring-boot:run" > logs/cloud.log 2>&1 &
CLOUD_PID=$!
echo "云端平台启动中 (PID: $CLOUD_PID)"

echo "等待云端就绪..."
sleep 6

echo "[2/3] 启动边缘节点核心网关 (端口 8081)..."
nohup bash -c "cd '$BASE_DIR/edge-node' && cargo run" > logs/edge.log 2>&1 &
EDGE_PID=$!
echo "边缘节点启动中 (PID: $EDGE_PID)"

if [ -d "vision-service" ] && command -v python3 >/dev/null 2>&1; then
  echo "[3/3] 启动 RTSP AI 视觉边车 vision-service (端口 8090)..."
  export EDGE_URL="${EDGE_URL:-http://127.0.0.1:8081}"
  nohup bash -c "cd '$BASE_DIR/vision-service' && python3 -u main.py" > logs/vision.log 2>&1 &
  VISION_PID=$!
  echo "视觉识别服务已启动 (PID: $VISION_PID)"
else
  echo "[3/3] 跳过 vision-service（未找到 python3 或依赖；生产推荐 Docker Compose 启动）"
fi

echo ""
echo "============================================================"
echo "  系统启动完成！"
echo "  监控大屏: http://localhost:8080/index.html"
echo "  云端后台: http://localhost:8080/admin.html"
echo "  边缘后台: http://localhost:8081/admin"
echo "  视觉健康: http://localhost:8090/health"
echo "  日志目录: $BASE_DIR/logs/"
echo "============================================================"
