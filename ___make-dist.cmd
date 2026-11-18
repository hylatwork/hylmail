@echo off

rem Builds HylMail and every Windows distribution artifact:
rem   sys-src/target/release   - both launchers (hylmail-gui.exe, hylmail-cli.exe)
rem   sys-src/dist             - the Inno Setup installer and the WiX MSI
rem
rem The version is never typed here.  It is read from [workspace.package] in
rem sys-src/Cargo.toml - the single source of truth the release pipeline uses
rem as well - and passed to the two installer compilers.
rem
rem Each installer is only built when its tool is at hand: Inno Setup 6
rem (ISCC.exe, installed or on PATH) for the per-user installer, WiX 3
rem (candle.exe and light.exe, on PATH or unpacked into sys-src\wix314) for the
rem per-machine MSI.  A missing tool is reported and skipped, so a machine
rem without either still produces the release binaries.
rem
rem Usage:  ___make-dist.cmd [nopause]
rem
cd /D "%~dp0"

if not exist "sys-src\Cargo.toml" (
    echo ERROR: run this script from the HylMail repository root - "sys-src\Cargo.toml" not found. 1>&2
    pause
    exit /b 1
)

setlocal EnableExtensions

set "_PAUSE=1"
if /I "%~1"=="nopause"    set "_PAUSE=0"
if /I "%~1"=="--nopause"  set "_PAUSE=0"
if /I "%~1"=="--no-pause" set "_PAUSE=0"

echo === HylMail - building distribution =====================================

rem --- the version: single source of truth -----------------------------------
set "_VERSION="
for /f "tokens=3" %%v in ('findstr /R /C:"^version" "sys-src\Cargo.toml"') do if not defined _VERSION set "_VERSION=%%~v"
if not defined _VERSION (
    echo ERROR: cannot read the version from "sys-src\Cargo.toml". 1>&2
    if "%_PAUSE%"=="1" pause
    exit /b 1
)
echo [version]   %_VERSION%
echo [profile]   release

pushd "sys-src"
if not exist "dist" mkdir "dist"

rem --- the release build ------------------------------------------------------
echo [cargo]     build --workspace --release
cargo build --workspace --release
if errorlevel 1 call :fail "cargo build --workspace --release"

rem --- Inno Setup: the per-user installer ------------------------------------
set "_ISCC="
if exist "%ProgramFiles(x86)%\Inno Setup 6\ISCC.exe" set "_ISCC=%ProgramFiles(x86)%\Inno Setup 6\ISCC.exe"
if not defined _ISCC if exist "%ProgramFiles%\Inno Setup 6\ISCC.exe" set "_ISCC=%ProgramFiles%\Inno Setup 6\ISCC.exe"
if not defined _ISCC for %%p in (ISCC.exe) do if not "%%~$PATH:p"=="" set "_ISCC=%%~$PATH:p"

if not defined _ISCC goto skip_inno
echo [inno]      dist\HylMail-%_VERSION%-setup.exe
"%_ISCC%" "/DHylMailVersion=%_VERSION%" "packaging\windows\hylmail.iss"
if errorlevel 1 call :fail "Inno Setup (ISCC.exe) failed"
goto after_inno
:skip_inno
echo [skip]      Inno Setup - ISCC.exe not found ^(install Inno Setup 6^)
:after_inno

rem --- WiX 3: the per-machine MSI --------------------------------------------
set "_CANDLE="
set "_LIGHT="
if exist "wix314\candle.exe" set "_CANDLE=%CD%\wix314\candle.exe"
if exist "wix314\light.exe"  set "_LIGHT=%CD%\wix314\light.exe"
if not defined _CANDLE for %%p in (candle.exe) do if not "%%~$PATH:p"=="" set "_CANDLE=%%~$PATH:p"
if not defined _LIGHT  for %%p in (light.exe)  do if not "%%~$PATH:p"=="" set "_LIGHT=%%~$PATH:p"

if not defined _CANDLE goto skip_wix
if not defined _LIGHT goto skip_wix
echo [wix]       dist\HylMail-%_VERSION%.msi
"%_CANDLE%" -nologo -dHylMailVersion=%_VERSION% -out "dist\hylmail.wixobj" "packaging\windows\hylmail.wxs"
if errorlevel 1 call :fail "candle.exe failed"
"%_LIGHT%" -nologo -sice:ICE61 -out "dist\HylMail-%_VERSION%.msi" "dist\hylmail.wixobj"
if errorlevel 1 call :fail "light.exe failed"
del /Q "dist\hylmail.wixobj" 2>NUL
goto after_wix
:skip_wix
echo [skip]      WiX - candle.exe/light.exe not found ^(unpack WiX 3 into sys-src\wix314 or add it to PATH^)
:after_wix

popd

rem --- what came out ----------------------------------------------------------
echo.
echo === artifacts ==========================================================
if not exist "sys-src\dist\*" goto no_artifacts
for %%f in ("sys-src\dist\*") do echo   %%~nxf
goto report_done
:no_artifacts
echo   [none]      sys-src\dist is empty
:report_done
echo =========================================================================

if "%_PAUSE%"=="1" pause
endlocal
exit /b 0

rem --- failure path -----------------------------------------------------------
:fail
echo ERROR: %~1 1>&2
popd
if "%_PAUSE%"=="1" pause
endlocal
exit /b 1
