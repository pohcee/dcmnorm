# Fails unless the given Windows binary (a .node, .pyd or .dll) is self-contained with respect to
# FFmpeg: it must link FFmpeg statically, because a consumer gets just this one file and any
# av*/sw* DLL import would be a load failure on their machine. Used by the release workflow's
# Windows binding builds:  pwsh packaging/check-windows-deps.ps1 <file>
param([Parameter(Mandatory = $true)][string]$Path)
$ErrorActionPreference = 'Stop'
$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
$vs = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
$dumpbin = Get-ChildItem "$vs\VC\Tools\MSVC" -Recurse -Filter dumpbin.exe | Where-Object { $_.FullName -match 'Hostx64\\x64' } | Select-Object -First 1
if (-not $dumpbin) { throw 'dumpbin.exe not found' }
$deps = & $dumpbin.FullName /dependents $Path
$deps | Write-Host
$bad = $deps | Where-Object { $_ -match '^\s+(av|sw|postproc)[a-z]*-\d+\.dll\s*$' }
if ($bad) { throw "$Path imports FFmpeg DLLs (not statically linked): $bad" }
