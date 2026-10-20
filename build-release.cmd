@echo off
REM ---------------------------------------------------------------------------
REM build-release.cmd - build App Monitor in release mode (Windows / MSVC).
REM
REM Why this wrapper exists:
REM   cc-rs normally discovers the MSVC toolchain through reg.exe. When that is
REM   blocked by local policy the discovery silently fails and C dependencies fail
REM   to compile with an opaque "cl.exe ... exit code: 2". Exporting INCLUDE / LIB
REM   here makes the build work regardless of registry access.
REM
REM   Adjust VCTOOLS / SDKVER below if your VS Build Tools or Windows SDK differ
REM   (look under "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\
REM   VC\Tools\MSVC" and "C:\Program Files (x86)\Windows Kits\10\Include").
REM
REM   If you are already in an "x64 Native Tools Command Prompt for VS 2022",
REM   plain `cargo build --release` works and this script is unnecessary.
REM ---------------------------------------------------------------------------

setlocal

set "VCTOOLS=C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Tools\MSVC\14.44.35207"
set "SDK=C:\Program Files (x86)\Windows Kits\10"
set "SDKVER=10.0.26100.0"

if not exist "%VCTOOLS%\include" (
  echo [error] MSVC headers not found at "%VCTOOLS%\include"
  echo         Edit VCTOOLS in this script to match your installed toolset.
  exit /b 1
)
if not exist "%SDK%\Include\%SDKVER%\ucrt" (
  echo [error] Windows SDK headers not found at "%SDK%\Include\%SDKVER%\ucrt"
  echo         Edit SDKVER in this script, or run: dir "%SDK%\Include"
  exit /b 1
)

set "INCLUDE=%VCTOOLS%\include;%SDK%\Include\%SDKVER%\ucrt;%SDK%\Include\%SDKVER%\shared;%SDK%\Include\%SDKVER%\um;%SDK%\Include\%SDKVER%\winrt;%SDK%\Include\%SDKVER%\cppwinrt"
set "LIB=%VCTOOLS%\lib\x64;%SDK%\Lib\%SDKVER%\ucrt\x64;%SDK%\Lib\%SDKVER%\um\x64"
set "PATH=%VCTOOLS%\bin\HostX64\x64;%PATH%"

cd /d "%~dp0"
cargo build --release %*
if errorlevel 1 exit /b 1

echo.
echo [ok] built target\release\app-monitor.exe
echo      data (config / tracks / history) is kept next to the exe in app_monitor_data\
