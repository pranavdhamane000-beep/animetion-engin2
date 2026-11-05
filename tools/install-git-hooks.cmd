@echo off
rem Install the pre-commit gate for this clone. Run once after cloning.
rem .git/hooks is not versioned, so the hook cannot simply be committed.
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0install-git-hooks.ps1" %*
exit /b %ERRORLEVEL%
