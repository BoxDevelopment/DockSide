@echo off
setlocal
rem Builds DockSide. Usage: build.bat [release^|debug] [run]
rem   (no args)  fast build    -> target\fast\dockside.exe     (optimized, rebuilds in seconds)
rem   release    release build -> target\release\dockside.exe  (fully optimized, for shipping)
rem   debug      debug build   -> target\debug\dockside.exe
rem   run        launch DockSide after a successful build

cd /d "%~dp0"

where cargo >nul 2>nul
if errorlevel 1 (
    echo [build] cargo was not found. Install Rust ^(MSVC toolchain^) from https://rustup.rs
    goto :fail
)

set "PROFILE=fast"
set "PROFILE_FLAG=--profile fast"
set "RUN="
for %%A in (%*) do (
    if /i "%%~A"=="release" (
        set "PROFILE=release"
        set "PROFILE_FLAG=--release"
    )
    if /i "%%~A"=="debug" (
        set "PROFILE=debug"
        set "PROFILE_FLAG="
    )
    if /i "%%~A"=="run" set "RUN=1"
)

echo [build] Building DockSide (%PROFILE%)...
cargo build %PROFILE_FLAG%
if errorlevel 1 (
    echo.
    echo [build] Build FAILED.
    goto :fail
)

echo.
echo [build] Done: %~dp0target\%PROFILE%\dockside.exe

if defined RUN (
    start "" "%~dp0target\%PROFILE%\dockside.exe"
    exit /b 0
)

rem Keep the window open when launched by double-click.
echo %cmdcmdline% | find /i "%~0" >nul && pause
exit /b 0

:fail
echo %cmdcmdline% | find /i "%~0" >nul && pause
exit /b 1
