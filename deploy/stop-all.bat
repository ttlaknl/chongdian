@echo off
chcp 65001 > nul
echo ============================================================
echo   正在停止充电车位防占位预警系统各组件进程...
echo ============================================================

taskkill /f /im charging-space-edge.exe 2>nul
wmic process where "name='java.exe' and commandline like '%%charging-cloud%%'" delete 2>nul

echo.
echo 所有相关进程已停止！
pause
