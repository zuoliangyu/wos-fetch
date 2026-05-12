@echo off
setlocal enabledelayedexpansion
title wos-fetch
cd /d "%~dp0"

echo ============================================
echo  wos-fetch  ^|  http://127.0.0.1:8001
echo ============================================
echo.

REM ---- Detect a real Python 3.10+ ---------------------------------
REM Order: py launcher (python.org installs), then python, then python3.
REM The Microsoft Store stub fails the `import sys; sys.exit(...)` check.
set "PYTHON_EXE="
for %%C in ("py -3" "py" "python" "python3") do (
    if not defined PYTHON_EXE (
        %%~C -c "import sys; sys.exit(0 if sys.version_info >= (3, 10) else 1)" >nul 2>&1
        if not errorlevel 1 set "PYTHON_EXE=%%~C"
    )
)

if not defined PYTHON_EXE (
    echo [ERROR] Python 3.10 or newer was not found on this machine.
    echo.
    echo Please install Python from:
    echo   https://www.python.org/downloads/windows/
    echo.
    echo IMPORTANT: during installation, tick "Add python.exe to PATH".
    echo            After installing, re-run this script.
    echo.
    pause
    exit /b 1
)

for /f "tokens=*" %%i in ('%PYTHON_EXE% --version 2^>^&1') do echo [Python] %%i  ^(via %PYTHON_EXE%^)

REM ---- Create venv on first run -----------------------------------
if not exist ".venv\Scripts\python.exe" (
    echo [SETUP] Creating virtual environment ^(.venv^)...
    %PYTHON_EXE% -m venv .venv
    if errorlevel 1 (
        echo [ERROR] Failed to create virtual environment.
        echo         Make sure the python you have can run "python -m venv".
        pause
        exit /b 1
    )
)

set "VENV_PY=.venv\Scripts\python.exe"

REM ---- Skip pip install when requirements.txt is unchanged --------
set "REQ_MARKER=.venv\.requirements_sha256"
set "REQ_HASH="
for /f "skip=1 tokens=*" %%H in ('certutil -hashfile requirements.txt SHA256 2^>nul') do (
    if not defined REQ_HASH (
        set "LINE=%%H"
        echo !LINE! | findstr /v "CertUtil" | findstr /v "hash of" >nul && set "REQ_HASH=!LINE: =!"
    )
)

set "STORED_HASH="
if exist "%REQ_MARKER%" set /p STORED_HASH=<"%REQ_MARKER%"

if "!REQ_HASH!"=="!STORED_HASH!" if not "!REQ_HASH!"=="" (
    echo [PIP] Dependencies up to date, skipping install.
) else (
    echo [PIP] Installing dependencies ^(this is a one-time setup^)...
    "%VENV_PY%" -m pip install --disable-pip-version-check -q -r requirements.txt
    if errorlevel 1 (
        echo [ERROR] pip install failed. Check your network connection,
        echo         or try running this script again.
        pause
        exit /b 1
    )
    >"%REQ_MARKER%" echo !REQ_HASH!
)

echo.
echo  Server starting at http://127.0.0.1:8001
echo  Press Ctrl+C to stop.
echo.

start /b cmd /c "timeout /t 2 >nul && start http://127.0.0.1:8001"
"%VENV_PY%" -m uvicorn main:app --host 127.0.0.1 --port 8001

echo.
echo [STOPPED]
pause
