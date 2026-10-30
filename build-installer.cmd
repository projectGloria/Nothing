@echo off
rem Builds the app, then the self-contained installer that embeds it.
rem Output: target\release\nothing-setup.exe
cd /d "%~dp0"
where cargo >nul 2>nul || set "PATH=%USERPROFILE%\.cargo\bin;%PATH%"
cargo build --release -p nothing -p nothing-svc || exit /b 1
cargo build --release -p nothing-setup || exit /b 1
echo.
echo Installer: %~dp0target\release\nothing-setup.exe
