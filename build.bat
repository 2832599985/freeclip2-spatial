@echo off
REM Double-click to build. Pass -Installer to also produce the MSI.
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0build.ps1" %*
pause
