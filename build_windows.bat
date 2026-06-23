@echo off
REM Build PengyR for Windows
REM Prerequisites:
REM   1. Install Rust: https://rustup.rs/
REM   2. Install Qt6: https://www.qt.io/download-qt-installer
REM      (choose MSVC 64-bit, e.g. Qt 6.10.x → msvc2022_64)
REM   3. Install Visual Studio Build Tools 2022:
REM      https://visualstudio.microsoft.com/downloads/#build-tools-for-visual-studio-2022
REM      (select "Desktop development with C++")
REM   4. Install CMake: https://cmake.org/download/
REM      (or: winget install Kitware.CMake)
REM
REM The script auto-detects VS 2022, so you can run it from any
REM terminal (cmd.exe, PowerShell, etc.).

setlocal enabledelayedexpansion
set ROOT=%~dp0
REM Remove trailing backslash from %~dp0
if "%ROOT:~-1%"=="\" set ROOT=%ROOT:~0,-1%
cd /d "%ROOT%"

REM ── Auto-detect Visual Studio 2022 (if not already in a Developer Prompt) ──
if not defined VSCMD_VER (
    where vswhere >nul 2>nul
    if !ERRORLEVEL! equ 0 (
        for /f "usebackq delims=" %%i in (`vswhere -latest -productId Microsoft.VisualStudio.Product.Community -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath`) do set VSINSTALLDIR=%%i
        if not defined VSINSTALLDIR (
            for /f "usebackq delims=" %%i in (`vswhere -latest -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath`) do set VSINSTALLDIR=%%i
        )
        if defined VSINSTALLDIR (
            echo Found Visual Studio: !VSINSTALLDIR!
            call "!VSINSTALLDIR!\VC\Auxiliary\Build\vcvarsall.bat" x64
        ) else (
            echo WARNING: Could not find Visual Studio 2022 installation via vswhere.
        )
    ) else (
        REM fallback: check some common paths
        if exist "C:\Program Files\Microsoft Visual Studio\2022\Community\VC\Auxiliary\Build\vcvarsall.bat" (
            call "C:\Program Files\Microsoft Visual Studio\2022\Community\VC\Auxiliary\Build\vcvarsall.bat" x64
        ) else if exist "C:\Program Files\Microsoft Visual Studio\2022\Professional\VC\Auxiliary\Build\vcvarsall.bat" (
            call "C:\Program Files\Microsoft Visual Studio\2022\Professional\VC\Auxiliary\Build\vcvarsall.bat" x64
        ) else if exist "C:\Program Files\Microsoft Visual Studio\2022\Enterprise\VC\Auxiliary\Build\vcvarsall.bat" (
            call "C:\Program Files\Microsoft Visual Studio\2022\Enterprise\VC\Auxiliary\Build\vcvarsall.bat" x64
        ) else if exist "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvarsall.bat" (
            call "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvarsall.bat" x64
        ) else (
            echo WARNING: Could not find vswhere or a VS 2022 installation.
            echo          Make sure to run from a "Developer Command Prompt for VS 2022".
        )
    )
)

REM Set Qt6 path — adjust this to your Qt installation
if "%QT6_DIR%"=="" (
    REM Common default locations
    if exist "C:\Qt\6.11.1\msvc2022_64" set QT6_DIR=C:\Qt\6.11.1\msvc2022_64
    if exist "C:\Qt\6.11.0\msvc2022_64" set QT6_DIR=C:\Qt\6.11.0\msvc2022_64
    if exist "C:\Qt\6.10.0\msvc2022_64" set QT6_DIR=C:\Qt\6.10.0\msvc2022_64
    if exist "C:\Qt\6.10.1\msvc2022_64" set QT6_DIR=C:\Qt\6.10.1\msvc2022_64
    if exist "C:\Qt\6.9.0\msvc2022_64"  set QT6_DIR=C:\Qt\6.9.0\msvc2022_64
)

if "%QT6_DIR%"=="" (
    echo ERROR: Could not find Qt6. Set QT6_DIR environment variable.
    echo Example: set QT6_DIR=C:\Qt\6.10.0\msvc2022_64
    exit /b 1
)

echo Using Qt6: %QT6_DIR%
set PATH=%QT6_DIR%\bin;%PATH%

echo.
echo ==^> Building Rust core (release)...
cargo build --release --target x86_64-pc-windows-msvc

echo.
echo ==^> Building Qt6 GUI...
if not exist gui\build_windows mkdir gui\build_windows
cd gui\build_windows

REM The CMake file will find libpengy_core.lib in target/release/
cmake .. ^
    -G "Visual Studio 17 2022" ^
    -A x64 ^
    -DCMAKE_BUILD_TYPE=Release ^
    -DCMAKE_PREFIX_PATH="%QT6_DIR%" ^
    -DRUST_TARGET_DIR="%ROOT%\target\x86_64-pc-windows-msvc\release"

cmake --build . --config Release

echo.
echo ==^> Done!
echo Binary: gui\build_windows\Release\pengy.exe

REM Bundle Qt DLLs
echo ==^> Bundling Qt DLLs...
mkdir "%ROOT%\PengyR-Windows" 2>nul
copy gui\build_windows\Release\pengy.exe "%ROOT%\PengyR-Windows\" >nul
cd "%ROOT%\PengyR-Windows"

REM Use windeployqt to copy all needed Qt DLLs
windeployqt pengy.exe

echo.
echo ==^> Packaged: %ROOT%\PengyR-Windows\
echo ==^> Distribute by zipping the PengyR-Windows folder
endlocal
