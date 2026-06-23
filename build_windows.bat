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

REM ── Try to locate vswhere (ships with Visual Studio) ──
set VSWHERE=
for %%p in (
    "%ProgramFiles(x86)%\Microsoft Visual Studio\Installer\vswhere.exe"
    "%ProgramFiles%\Microsoft Visual Studio\Installer\vswhere.exe"
) do if exist %%p set VSWHERE=%%p

REM ── Find any Visual Studio installation via vswhere ──
set VSINSTALLDIR=
set VS_VERSION_MAJOR=
set VS_CHROOT=

if defined VSWHERE (
    for /f "usebackq delims=" %%i in (`"!VSWHERE!" -latest -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath`) do set VSINSTALLDIR=%%i
    if defined VSINSTALLDIR (
        for /f "usebackq delims=" %%i in (`"!VSWHERE!" -latest -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property catalog_productLineVersion`) do set VS_VERSION_MAJOR=%%i
        for /f "usebackq delims=" %%i in (`"!VSWHERE!" -latest -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationVersion`) do set VS_VERSION_FULL=%%i
    )
)

REM Fallback: try common install paths if vswhere didn't work
if not defined VSINSTALLDIR (
    for %%p in (
        "C:\Program Files\Microsoft Visual Studio\18\Community"
        "C:\Program Files\Microsoft Visual Studio\18\Professional"
        "C:\Program Files\Microsoft Visual Studio\18\Enterprise"
        "C:\Program Files\Microsoft Visual Studio\18\BuildTools"
        "C:\Program Files\Microsoft Visual Studio\2022\Community"
        "C:\Program Files\Microsoft Visual Studio\2022\Professional"
        "C:\Program Files\Microsoft Visual Studio\2022\Enterprise"
        "C:\Program Files\Microsoft Visual Studio\2022\BuildTools"
        "C:\Program Files (x86)\Microsoft Visual Studio\2022\Community"
        "C:\Program Files (x86)\Microsoft Visual Studio\2022\Professional"
        "C:\Program Files (x86)\Microsoft Visual Studio\2022\Enterprise"
        "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools"
        "C:\Program Files\Microsoft Visual Studio\2019\Community"
        "C:\Program Files\Microsoft Visual Studio\2019\Professional"
        "C:\Program Files\Microsoft Visual Studio\2019\Enterprise"
        "C:\Program Files\Microsoft Visual Studio\2019\BuildTools"
        "C:\Program Files (x86)\Microsoft Visual Studio\2019\Community"
        "C:\Program Files (x86)\Microsoft Visual Studio\2019\BuildTools"
    ) do if exist "%%~p\VC\Auxiliary\Build\vcvarsall.bat" set VSINSTALLDIR=%%~p
)

if not defined VSINSTALLDIR (
    echo WARNING: Could not find Visual Studio.
    echo          Install it from: https://visualstudio.microsoft.com/downloads/
    echo          Or run from a "Developer Command Prompt for VS 2022".
    set VS_FOUND=0
) else (
    echo Found Visual Studio: !VSINSTALLDIR!  (version: !VS_VERSION_FULL!)
    set VS_FOUND=1

    REM ── Set up the MSVC compiler environment ──
    REM Try vcvarsall.bat first, then VsDevCmd.bat
    if exist "!VSINSTALLDIR!\VC\Auxiliary\Build\vcvarsall.bat" (
        REM Suppress VsDevCmd.bat telemetry errors in non-interactive shells
        set VSCMD_SKIP_SENDERROR=1
        call "!VSINSTALLDIR!\VC\Auxiliary\Build\vcvarsall.bat" x64
        if !ERRORLEVEL! neq 0 (
            echo [INFO] vcvarsall.bat reported issues, but compiler may still be usable.
        )
    ) else if exist "!VSINSTALLDIR!\Common7\Tools\VsDevCmd.bat" (
        set VSCMD_SKIP_SENDERROR=1
        call "!VSINSTALLDIR!\Common7\Tools\VsDevCmd.bat" -arch=x64
    ) else (
        echo WARNING: Could not find vcvarsall.bat or VsDevCmd.bat.
    )

    REM ── Determine CMake generator based on VS version ──
    if "%VS_VERSION_MAJOR%"=="18" (
        set CMAKE_GENERATOR="Visual Studio 17 2022"
    ) else if "%VS_VERSION_MAJOR%"=="17" (
        set CMAKE_GENERATOR="Visual Studio 17 2022"
    ) else if "%VS_VERSION_MAJOR%"=="16" (
        set CMAKE_GENERATOR="Visual Studio 16 2019"
    ) else (
        REM Try to detect from path
        echo !VSINSTALLDIR! | findstr /C:"\18\" >nul
        if !ERRORLEVEL! equ 0 (
            set CMAKE_GENERATOR="Visual Studio 17 2022"
        ) else (
            echo !VSINSTALLDIR! | findstr /C:"2022" >nul
            if !ERRORLEVEL! equ 0 (
                set CMAKE_GENERATOR="Visual Studio 17 2022"
            ) else (
                echo !VSINSTALLDIR! | findstr /C:"2019" >nul
                if !ERRORLEVEL! equ 0 (
                    set CMAKE_GENERATOR="Visual Studio 16 2019"
                ) else (
                    set CMAKE_GENERATOR="Visual Studio 17 2022"
                )
            )
        )
    )
    echo CMake generator: !CMAKE_GENERATOR!
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
set CMAKE_EXTRA_ARGS=
if defined VSINSTALLDIR (
    set CMAKE_EXTRA_ARGS=-DCMAKE_GENERATOR_INSTANCE="!VSINSTALLDIR!"
)
cmake .. ^
    -G !CMAKE_GENERATOR! ^
    -A x64 ^
    -DCMAKE_BUILD_TYPE=Release ^
    -DCMAKE_PREFIX_PATH="%QT6_DIR%" ^
    -DRUST_TARGET_DIR="%ROOT%\target\x86_64-pc-windows-msvc\release" ^
    !CMAKE_EXTRA_ARGS!

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
