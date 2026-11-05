@echo off
rem Launcher for the workspace gate (spec 9.5 step 2). Kept as a .cmd so the gate
rem is one double-click or one short command rather than a PowerShell invocation
rem somebody has to remember the exact form of.
rem
rem -ExecutionPolicy Bypass is scoped to this process only. It is not a change to
rem the machine policy and it does not persist.
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0ci.ps1" %*
exit /b %ERRORLEVEL%
