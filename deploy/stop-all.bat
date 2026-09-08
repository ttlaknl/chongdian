@echo off
chcp 65001 > nul
echo ============================================================
echo   正在停止充电车位防占位预警系统各组件进程...
echo ============================================================

taskkill /f /im charging-space-edge.exe 2>nul
taskkill /f /im java.exe 2>nul

echo.
echo 所有相关进程已停止！
pause
