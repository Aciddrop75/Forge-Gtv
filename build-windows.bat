@echo off
rem The Forge GTV - Copyright (C) 2026 [TFG]Aciddrop
rem SPDX-License-Identifier: GPL-3.0-or-later
rem This program comes with ABSOLUTELY NO WARRANTY; see the LICENSE file.
REM Double-click wrapper for build-windows.ps1
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0build-windows.ps1"
pause
