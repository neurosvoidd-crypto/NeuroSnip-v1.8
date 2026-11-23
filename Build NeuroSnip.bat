@echo off
setlocal EnableExtensions
cd /d "%~dp0"
if errorlevel 1 goto folder_error

echo === NeuroSnip Windows Rust build ===
where cargo >nul 2>nul
if errorlevel 1 goto rust_missing
where rustup >nul 2>nul
if errorlevel 1 goto rust_missing

rem The source ZIP intentionally excludes the large FFmpeg binary.
rem Create its destination before PowerShell tries to download into it.
if not exist "assets" mkdir "assets"
if errorlevel 1 goto assets_error

if exist "assets\ffmpeg.exe" goto ffmpeg_ready
call :download_ffmpeg
if errorlevel 1 goto ffmpeg_error

:ffmpeg_ready
echo Installing Windows MSVC target if needed...
rustup target add x86_64-pc-windows-msvc
if errorlevel 1 goto target_error

echo Building optimized Windows executable...
cargo build --release --target x86_64-pc-windows-msvc
if errorlevel 1 goto build_error

if not exist "publish" mkdir "publish"
if errorlevel 1 goto publish_error
if not exist "target\x86_64-pc-windows-msvc\release\neurosnip.exe" goto build_error
copy /y "target\x86_64-pc-windows-msvc\release\neurosnip.exe" "publish\NeuroSnip.exe" >nul
if errorlevel 1 goto publish_error

echo.
echo SUCCESS: %CD%\publish\NeuroSnip.exe
pause
exit /b 0

:download_ffmpeg
echo Downloading FFmpeg for built-in screen recording...
powershell.exe -NoProfile -ExecutionPolicy Bypass -Command "$ErrorActionPreference='Stop'; $ProgressPreference='SilentlyContinue'; $zip='assets\ffmpeg.zip'; $folder='assets\ffmpeg_unpack'; try { Invoke-WebRequest -Uri 'https://www.gyan.dev/ffmpeg/builds/ffmpeg-release-essentials.zip' -OutFile $zip; Expand-Archive -LiteralPath $zip -DestinationPath $folder -Force; $ffmpeg=Get-ChildItem -LiteralPath $folder -Filter 'ffmpeg.exe' -File -Recurse | Select-Object -First 1; if ($null -eq $ffmpeg) { throw 'ffmpeg.exe was not found inside the downloaded archive.' }; Copy-Item -LiteralPath $ffmpeg.FullName -Destination 'assets\ffmpeg.exe' -Force; if ((Get-Item -LiteralPath 'assets\ffmpeg.exe').Length -lt 1000000) { throw 'The downloaded ffmpeg.exe is unexpectedly small.' } } catch { Remove-Item -LiteralPath 'assets\ffmpeg.exe' -Force -ErrorAction SilentlyContinue; Write-Host ('ERROR downloading FFmpeg: ' + $_.Exception.Message) -ForegroundColor Red; exit 1 } finally { Remove-Item -LiteralPath $zip -Force -ErrorAction SilentlyContinue; Remove-Item -LiteralPath $folder -Recurse -Force -ErrorAction SilentlyContinue }"
exit /b %errorlevel%

:folder_error
echo ERROR: Could not open the folder containing this build script.
goto fail
:rust_missing
echo ERROR: Rust/Cargo is missing. Install Rust from https://rustup.rs/ and restart the terminal.
goto fail
:assets_error
echo ERROR: Cannot create the assets folder. Check folder permissions.
goto fail
:ffmpeg_error
echo ERROR: FFmpeg could not be downloaded or extracted. Check the internet connection and try again.
goto fail
:target_error
echo ERROR: Could not install the Windows MSVC target.
goto fail
:build_error
echo ERROR: Rust compilation failed. Check the compiler errors above and make sure MSVC C++ Build Tools are installed.
goto fail
:publish_error
echo ERROR: Could not write the finished executable to the publish folder.
goto fail
:fail
echo.
pause
exit /b 1
