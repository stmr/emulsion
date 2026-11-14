
call update_files.bat
if errorlevel 1 exit /b 1
makensis /DVERSION=0.0.0-test-build emulsion.nsi
