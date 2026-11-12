@echo off
rem release.cmd <version>: build handys-fisch with the self-update code on, tag it and publish a GitHub release. Public: run it only on purpose.
rem Never closes or starts the app. No delayed expansion: this folder's path may have a "!" in it, which delayed expansion would strip.
setlocal
cd /d "%~dp0"
set "V=%~1"
set "EXE=handys-fisch.exe"

echo [1/8] checking the version
if "%V%"=="" (echo usage: release.cmd ^<MAJOR.MINOR.PATCH^>& exit /b 2)
powershell -NoProfile -Command "if ($env:V -match '^\d+\.\d+\.\d+$') { exit 0 } else { exit 1 }" || (echo "%V%" is not MAJOR.MINOR.PATCH& exit /b 2)

echo [2/8] checking the working tree
git status --porcelain | findstr "^" >nul && (echo the working tree is not clean: commit or stash first& exit /b 1)
rem Built against the local SDK when it sits next to the app, so it must be exactly what GitHub has.
if not exist "..\sdk" goto sdkok
git -C ..\sdk status --porcelain | findstr "^" >nul && (echo ..\sdk is not clean& exit /b 1)
git -C ..\sdk fetch -q || (echo git fetch failed in ..\sdk& exit /b 1)
for /f %%a in ('git -C ..\sdk rev-parse HEAD') do set "SDK_HEAD=%%a"
for /f %%a in ('git -C ..\sdk rev-parse origin/main') do set "SDK_ORIGIN=%%a"
if not "%SDK_HEAD%"=="%SDK_ORIGIN%" (echo ..\sdk HEAD is not origin/main: push or pull the SDK first& exit /b 1)
:sdkok

echo [3/8] checking tag v%V% is free
git rev-parse -q --verify "refs/tags/v%V%" >nul && (echo tag v%V% already exists locally& exit /b 1)
git ls-remote --tags origin "refs/tags/v%V%" | findstr "^" >nul && (echo tag v%V% already exists on origin& exit /b 1)

echo [4/8] setting version %V% in Cargo.toml
powershell -NoProfile -Command "$p = 'Cargo.toml'; $t = [IO.File]::ReadAllText($p); $t = ([regex]'(?m)^version = \"[^\"]*\"').Replace($t, 'version = \"' + $env:V + '\"', 1); [IO.File]::WriteAllText($p, $t, (New-Object Text.UTF8Encoding $false))" || (echo editing Cargo.toml failed& git checkout -- Cargo.toml& exit /b 1)

echo [5/8] building and testing (HANDY_RELEASE=1)
set HANDY_RELEASE=1
cargo build --release || goto fail
cargo test || goto fail

echo [6/8] collecting the exe and its sha256
set "BUILT="
if exist "target\release\%EXE%" set "BUILT=target\release\%EXE%"
if not defined BUILT if exist "..\target\release\%EXE%" set "BUILT=..\target\release\%EXE%"
if not defined BUILT (echo %EXE% not found under target\release& goto fail)
if exist release rmdir /s /q release
mkdir release || goto fail
copy /y "%BUILT%" "release\%EXE%" >nul || goto fail
powershell -NoProfile -Command "$h = (Get-FileHash -Algorithm SHA256 -LiteralPath ('release\' + $env:EXE)).Hash.ToLower(); [IO.File]::WriteAllText('release\' + $env:EXE + '.sha256', $h + '  ' + $env:EXE + \"`n\")" || goto fail

echo [7/8] committing, tagging and pushing v%V%
git add Cargo.toml Cargo.lock || goto fail
git commit -m "Release v%V%" -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>" || goto fail
git tag "v%V%" || goto fail
git push || exit /b 1
git push origin "v%V%" || exit /b 1

echo [8/8] creating the GitHub release
gh release create "v%V%" "release\%EXE%" "release\%EXE%.sha256" --title "handys-fisch v%V%" --notes "handys-fisch v%V%" || exit /b 1
echo released handys-fisch v%V%
exit /b 0

:fail
echo failed: restoring Cargo.toml
git checkout -- Cargo.toml
exit /b 1
