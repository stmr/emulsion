@echo off
setlocal
set "distribution_dir=%~dp0"

pushd "%distribution_dir%..\.." || exit /b 1
cargo build --release --features=networking
if errorlevel 1 (
	popd
	exit /b 1
)
popd

if not exist "%distribution_dir%program" mkdir "%distribution_dir%program"
if errorlevel 1 exit /b 1
copy /y "%distribution_dir%..\..\target\release\emulsion.exe" "%distribution_dir%program\emulsion.exe"
if errorlevel 1 exit /b 1
