# DLL planting test. Copies the exe into a new folder under %TEMP%, surrounds it with
# harmless fake files named like system DLLs plus a fake external manifest, and starts it.
#
# PASS: the app starts and shows the test folder's stacks; no error box appears.
#       Also open a stack, right-click a file and open the tray menu (some DLLs load late).
# FAIL: Windows shows "Bad Image" naming one of the fake DLLs, or a side-by-side error:
#       that file was looked up next to the exe.
#
# usage:  powershell -ExecutionPolicy Bypass -File tools\planting_test.ps1 path\to\DesktopStacks.exe
param([Parameter(Mandatory = $true)][string]$Exe)
$ErrorActionPreference = 'Stop'
$dir = Join-Path $env:TEMP ("ds-planting-test-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
$desk = Join-Path $dir 'desktop'
New-Item -ItemType Directory -Path $desk | Out-Null
Copy-Item $Exe (Join-Path $dir 'DesktopStacks.exe')
$names = 'dwmapi', 'd2d1', 'd3d11', 'dcomp', 'dwrite', 'dxgi', 'bcryptprimitives', 'shcore', 'version',
    'uxtheme', 'cryptbase', 'userenv', 'profapi', 'wldp', 'propsys', 'windows.storage', 'textshaping',
    'd3d10warp', 'dxcore', 'msimg32', 'winmm', 'cfgmgr32', 'powrprof', 'wtsapi32', 'iertutil', 'urlmon',
    'netapi32', 'mpr', 'sspicli', 'cryptsp', 'ntmarta', 'edputil', 'coremessaging', 'twinapi.appcore',
    'dbghelp', 'comctl32', 'textinputframework', 'msctf', 'imm32', 'oleacc'
foreach ($n in $names) {
    Set-Content -Path (Join-Path $dir "$n.dll") -Value 'This is not a real DLL. Harmless test file.' -Encoding ASCII
}
# An external manifest that tries to redirect two DLL names to the fake files in this folder.
# The exe has its own built-in manifest, so Windows must ignore this one.
$manifest = @'
<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <assemblyIdentity type="win32" name="DesktopStacks" version="0.1.2.0" processorArchitecture="amd64"/>
  <file name="dwmapi.dll"/>
  <file name="d3d11.dll"/>
</assembly>
'@
Set-Content -Path (Join-Path $dir 'DesktopStacks.exe.manifest') -Value $manifest -Encoding UTF8
foreach ($f in 'notes.txt', 'report.pdf', 'photo one.png', 'photo two.png', 'clip.mp4') {
    Set-Content -Path (Join-Path $desk $f) -Value 'test' -Encoding ASCII
}
Write-Host "Test folder: $dir"
Write-Host 'Starting the app. Exit it from the tray icon when done, then delete the test folder.'
Start-Process -FilePath (Join-Path $dir 'DesktopStacks.exe') -ArgumentList '--dir', "`"$desk`""
