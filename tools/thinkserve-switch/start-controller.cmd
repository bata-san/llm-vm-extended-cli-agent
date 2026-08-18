@echo off
cd /d "%~dp0"
start "ThinkServe Switch" powershell.exe -NoProfile -WindowStyle Hidden -ExecutionPolicy Bypass -File "%~dp0ThinkServe.ps1"
