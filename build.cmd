@echo off
rem build.cmd [test]: release-build handys-fisch and copy the exe next to this script; "test" also runs every test.
rem No delayed expansion: this folder's path may have a "!" in it, which delayed expansion would strip.
setlocal
cd /d "%~dp0"
set "TEST="
set "OPEN="
set "ERR=0"
:args
if "%~1"=="" goto argsdone
if /i "%~1"=="test" (set "TEST=1") else (echo unknown argument "%~1": only "test" is accepted& exit /b 2)
shift
goto args
:argsdone

rem Built (and tested) while the app keeps running: it runs from the copy in this folder, which the build does not
rem touch. A failed build or test stops here and leaves the app alone.
cargo build --release || exit /b 1
if defined TEST cargo test || exit /b 1

rem Only now is the app closed, at once (/f): no wait, but it cannot release keys it holds then. Then the new exe is copied here, the copy to run:
rem built alone it is in .\target\release, built from the shared folder (..\.cargo\config.toml) in ..\target\release.
call :close || exit /b 1
if exist "target\release\handys-fisch.exe" (copy /y "target\release\handys-fisch.exe" . >nul || set "ERR=1") else if exist "..\target\release\handys-fisch.exe" (copy /y "..\target\release\handys-fisch.exe" . >nul || set "ERR=1")

:reopen
rem Start the app again if it was open (the old exe if the copy failed). Through Explorer: started from a packaged
rem app (Claude desktop) it would stay inside that package and save to private copies of its files. Explorer also
rem doesn't hand the app this script's output pipe, so a piped build doesn't wait for it to exit.
if defined OPEN (if exist "%~dp0handys-fisch.exe" (explorer.exe "%~dp0handys-fisch.exe") else if exist "%~dp0target\release\handys-fisch.exe" (explorer.exe "%~dp0target\release\handys-fisch.exe") else explorer.exe "%~dp0..\target\release\handys-fisch.exe")
exit /b %ERR%

:close
set "EXE=handys-fisch.exe"
tasklist /fi "imagename eq %EXE%" | findstr /i /c:"%EXE%" >nul || exit /b 0
set "OPEN=1"
taskkill /f /im %EXE% >nul 2>&1
rem The exe stays locked until the process is really gone: check again straight away (each check takes ~0.1 s).
set N=0
:wait
tasklist /fi "imagename eq %EXE%" | findstr /i /c:"%EXE%" >nul || exit /b 0
set /a N+=1
if %N% geq 50 (echo %EXE% could not be closed, so it was not replaced.& exit /b 1)
goto wait
