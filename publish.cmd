@echo off
rem ============================================================
rem  DeepSeek Harness (Tauri) - network bootstrap publish
rem  Output: publish\DeepSeek Harness.exe
rem  Does NOT embed Node or dsh dependencies.
rem  At startup it uses the system Node environment when complete.
rem  Otherwise it downloads and verifies portable Node, then installs
rem  or updates @deepseek-ai/dsh through npm before starting the GUI.
rem  NOTE: keep this file ASCII-only (cmd parses it as ANSI).
rem ============================================================
setlocal
cd /d "%~dp0"

echo [1/2] Building network-only exe (no bundled Node dependencies)...
set "PATH=%USERPROFILE%\.cargo\bin;%PATH%"
set "CARGO_TARGET_DIR=%CD%\src-tauri\target"
pushd src-tauri
cargo build --release --locked
if errorlevel 1 (
    popd
    echo [ERROR] cargo build failed.
    exit /b 1
)
popd

echo [2/2] Copying to publish\ ...
if not exist publish mkdir publish
copy /y "src-tauri\target\release\deepseek-harness-desktop.exe" "publish\DeepSeek Harness.exe" >nul
if errorlevel 1 (
    echo [ERROR] failed to copy artifact.
    exit /b 1
)

for %%F in ("publish\DeepSeek Harness.exe") do echo        size: %%~zF bytes
echo.
echo ============================================================
echo  Bootstrap publish OK: publish\DeepSeek Harness.exe
echo  Requires internet on first run and when checking dsh updates.
echo ============================================================
endlocal
