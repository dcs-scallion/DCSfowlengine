@echo off
cd /d "%~dp0"
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0WeatherVOTE.ps1" %*
if errorlevel 1 (
  echo.
  echo WeatherVOTE failed.
  pause
)
