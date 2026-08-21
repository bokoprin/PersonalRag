@echo off
setlocal
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "%~dp0scripts\verify-and-build-windows.ps1" -Run
set RC=%ERRORLEVEL%
echo.
if not "%RC%"=="0" echo FAILED. See output above.
if "%RC%"=="0" echo PersonalRag build passed and the GUI was launched.
pause
exit /b %RC%
