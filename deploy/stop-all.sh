#!/usr/bin/env bash

echo "============================================================"
echo "  正在停止充电车位防占位预警系统服务进程..."
echo "============================================================"

pkill -f "charging-space-edge" || true
pkill -f "charging-cloud" || true

echo "所有相关进程已停止！"
