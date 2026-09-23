@echo off
REM dockur copies /oem to C:\OEM and runs this at the end of unattended install.
powershell.exe -NoProfile -ExecutionPolicy Bypass -File C:\OEM\probe.ps1
exit /b 0
