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

rem A running app locks its exe: close it. Never /f: a killed app can't release the keys it holds, so one still open
rem after its wait is left running and nothing is built. fisch lands the fish in play first (up to 200 s), so one close.
call :close || (set "ERR=1" & goto reopen)

cargo build --release || (set "ERR=1" & goto reopen)
rem The exe also goes into this folder: that copy is the one to run. Built alone it is in .\target\release; built from the
rem shared folder (..\.cargo\config.toml) it is in ..\target\release.
if exist "target\release\handys-fisch.exe" (copy /y "target\release\handys-fisch.exe" . >nul || set "ERR=1") else if exist "..\target\release\handys-fisch.exe" (copy /y "..\target\release\handys-fisch.exe" . >nul || set "ERR=1")
if defined TEST cargo test || set "ERR=1"

:reopen
rem Start the app again if it was open (the old exe if the build failed). Through Explorer: started from a packaged
rem app (Claude desktop) it would stay inside that package and save to private copies of its files. Explorer also
rem doesn't hand the app this script's output pipe, so a piped build doesn't wait for it to exit.
if defined OPEN (if exist "%~dp0handys-fisch.exe" (explorer.exe "%~dp0handys-fisch.exe") else if exist "%~dp0target\release\handys-fisch.exe" (explorer.exe "%~dp0target\release\handys-fisch.exe") else explorer.exe "%~dp0..\target\release\handys-fisch.exe")
exit /b %ERR%

:close
set "EXE=handys-fisch.exe"
tasklist /fi "imagename eq %EXE%" | findstr /i /c:"%EXE%" >nul || exit /b 0
set "OPEN=1"
set "LIMIT=200"
taskkill /im %EXE% >nul 2>&1
set N=0
:wait
tasklist /fi "imagename eq %EXE%" | findstr /i /c:"%EXE%" >nul || exit /b 0
set /a N+=1
if %N% geq %LIMIT% (echo %EXE% did not close within %LIMIT% s, so nothing was built: close it yourself, then build again.& exit /b 1)
ping -n 2 127.0.0.1 >nul
goto wait