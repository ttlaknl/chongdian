@echo off
chcp 65001 > nul
echo ============================================================
echo   充电车位防占位预警系统 - Windows 本地环境一键启动
echo ============================================================

set BASE_DIR=%~dp0..
cd /d "%BASE_DIR%"

echo [1/2] 启动云端管控平台与大屏前端 (端口 8080)...
start "Chongdian-Cloud-Platform" cmd /c "cd /d cloud-platform && mvn spring-boot:run"

echo 等待云端平台初始化 (5秒)...
timeout /t 5 /nobreak > nul

echo [2/2] 启动边缘工控机节点服务 (端口 8081)...
set EDGE_CLOUD_URL=http://localhost:8080
start "Chongdian-Edge-Node" cmd /c "cd /d edge-node && cargo run"

echo.
echo ============================================================
echo   系统已启动完成！
echo   监控大屏访问地址: http://localhost:8080/index.html
echo   云端健康检查地址: http://localhost:8080/api/v1/health
echo   边缘节点健康接口: http://localhost:8081/health
echo ============================================================
pause
