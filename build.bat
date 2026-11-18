@echo off
rem Builds the widget into %LOCALAPPDATA%\Programs\ResMon\resmon.exe
rem IMPORTANT: close the running widget before rebuilding.
rem Do not run the exe from a low-integrity directory: any exe launched from
rem such a folder inherits the low integrity label, which prevents it from
rem reading normal (medium integrity) processes.
rem
rem Rust toolchain: https://rustup.rs  (cargo must be on PATH)
setlocal
where cargo >nul 2>nul
if errorlevel 1 (
  echo cargo not found - install Rust via https://rustup.rs and reopen the shell
  exit /b 1
)

cd /d "%~dp0"
set "DEST=%LOCALAPPDATA%\Programs\ResMon"
if not exist "%DEST%" mkdir "%DEST%"

cargo build --release
if errorlevel 1 (
  echo BUILD FAILED
  exit /b 1
)

copy /y "target\release\resmon.exe" "%DEST%\resmon.exe" >nul
if errorlevel 1 (
  echo COPY FAILED
  exit /b 1
)
copy /y "target\release\resmon-dump.exe" "%DEST%\resmon-dump.exe" >nul
if errorlevel 1 (
  echo COPY FAILED
  exit /b 1
)
echo BUILD OK -^> %DEST%\resmon.exe
