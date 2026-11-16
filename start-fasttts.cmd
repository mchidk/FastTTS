@echo off
setlocal
cd /d "%~dp0"
set "PYTHONPATH=%~dp0src;%PYTHONPATH%"

where python >nul 2>&1
if errorlevel 1 (
  echo Python was not found on PATH.
  pause
  exit /b 1
)

python -c "import numpy, sounddevice, faster_whisper, pystray, PIL" >nul 2>&1
if errorlevel 1 (
  echo FastTTS needs its Python packages.
  echo Install them with:
  echo     python -m pip install -e .
  pause
  exit /b 1
)

if not exist "%~dp0models\faster-whisper-small.en\model.bin" (
  echo The bundled English model is missing.
  echo Expected: %~dp0models\faster-whisper-small.en\model.bin
  echo Restore that folder from the FastTTS checkout.
  pause
  exit /b 1
)

where pythonw >nul 2>&1
if errorlevel 1 (
  start "" python -m fasttts
) else (
  start "" pythonw -m fasttts
)
