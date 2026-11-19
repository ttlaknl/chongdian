@echo off
chcp 65001 > nul
echo ============================================================
echo   充电车位防占位预警系统 - Windows 本地开发环境一键启动
echo ============================================================

set BASE_DIR=%~dp0..
cd /d "%BASE_DIR%"

if not exist logs mkdir logs

set EDGE_CLOUD_URL=http://127.0.0.1:8080
set EDGE_VISION_MODE=external
set EDGE_AI_ENGINE=python-hyperlpr3
set EDGE_HARDWARE_MODE=mock
set EDGE_STRICT_HARDWARE=false
set EDGE_URL=http://127.0.0.1:8081

echo [1/3] 启动云端管控平台与大屏前端 (端口 8080)...
start "Chongdian-Cloud-Platform" cmd /c "cd /d cloud-platform && mvn spring-boot:run"

echo 等待云端平台初始化 (6秒)...
timeout /t 6 /nobreak > nul

echo [2/3] 启动边缘工控机节点核心网关 (端口 8081)...
start "Chongdian-Edge-Node" cmd /c "cd /d edge-node && cargo run"

echo 等待边缘节点监听 8081 (4秒)...
timeout /t 4 /nobreak > nul

echo [3/3] 启动 Python RTSP AI 视觉识别边车 (端口 8090)...
start "Chongdian-Vision-Service" cmd /c "cd /d vision-service && python -u main.py"

echo.
echo ============================================================
echo   系统已启动！
echo   监控大屏: http://localhost:8080/index.html
echo   云端后台: http://localhost:8080/admin.html
echo   边缘运维: http://localhost:8081/admin
echo   视觉健康: http://localhost:8090/health
echo ============================================================
pause
