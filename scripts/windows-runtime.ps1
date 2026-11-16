# App-local deployment from Visual Studio's redistributable directory. No
# administrator rights or machine-wide runtime installation on the user's PC.
param([Parameter(Mandatory = $true)][string]$Destination)
$ErrorActionPreference = 'Stop'
$vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio/Installer/vswhere.exe'
$vs = & $vswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
if (!$vs) { throw 'Visual Studio C++ tools installation not found' }
$redistRoot = Join-Path $vs 'VC/Redist/MSVC'
$crt = Get-ChildItem $redistRoot -Directory |
    Where-Object { $_.Name -match '^\d+(\.\d+)+$' } |
    Sort-Object { [version]$_.Name } -Descending |
    ForEach-Object { Get-ChildItem (Join-Path $_.FullName 'x64') -Directory -Filter 'Microsoft.VC*.CRT' -ErrorAction SilentlyContinue } |
    Select-Object -First 1
if (!$crt) { throw "x64 redistributable CRT not found in $redistRoot" }
$names = @('msvcp140.dll', 'msvcp140_1.dll', 'vcruntime140.dll', 'vcruntime140_1.dll')
foreach ($name in $names) {
    $source = Join-Path $crt.FullName $name
    if (!(Test-Path $source)) { throw "Required runtime missing: $source" }
    Copy-Item $source $Destination -Force
}
# Check the dependency closure too: adding another runtime import must fail
# packaging until its app-local copy and updater support are added together.
$tools = Get-ChildItem (Join-Path $vs 'VC/Tools/MSVC') -Directory |
    Sort-Object { [version]$_.Name } -Descending | Select-Object -First 1
$dumpbin = Join-Path $tools.FullName 'bin/Hostx64/x64/dumpbin.exe'
foreach ($file in @('donsetch.exe', 'pdfium.dll', 'onnxruntime.dll') + $names) {
    $imports = & $dumpbin /dependents (Join-Path $Destination $file)
    if ($LASTEXITCODE -ne 0) { throw "Cannot inspect dependencies of $file" }
    foreach ($line in $imports) {
        if ($line -match '^\s*((?:msvcp|vcruntime|concrt)\S*\.dll)\s*$') {
            if (!(Test-Path (Join-Path $Destination $Matches[1]))) {
                throw "$file requires an unbundled Visual C++ runtime: $($Matches[1])"
            }
        }
    }
}
Write-Host "Staged app-local Visual C++ runtime from $($crt.FullName)"
