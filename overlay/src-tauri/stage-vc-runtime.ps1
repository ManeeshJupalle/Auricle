# Copies the Visual C++ runtime DLLs the installer ships into redist/, from
# the Visual Studio that built the engine.
#
# The engine links whisper.cpp and ONNX Runtime, which use the dynamic C++
# runtime, so auricle.exe will not start without these DLLs, and Windows does
# not come with them. tauri.release.conf.json puts them beside auricle.exe in
# the MSI. This script fails if the engine imports a runtime DLL that file
# does not list, so a new dependency can't ship without its DLL.
#
# Run after the engine sidecar is staged and before `tauri build`:
#   overlay/src-tauri/stage-vc-runtime.ps1
#   cd overlay; npx tauri build --config src-tauri/tauri.release.conf.json
param(
    [string]$Engine = (Join-Path $PSScriptRoot 'binaries/auricle-x86_64-pc-windows-msvc.exe')
)
$ErrorActionPreference = 'Stop'

$vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
$vs = & $vswhere -latest -products * `
    -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
if (-not $vs) { throw 'No Visual Studio with the C++ tools was found.' }

# The newest toolset under a `VC\...\MSVC\<version>\...` pattern. Redist\MSVC
# also holds a `v143` folder with nothing but the redistributable installers.
function Get-Newest([string]$Pattern) {
    Get-ChildItem (Join-Path $vs $Pattern) |
        Where-Object { $_.FullName -match '\\MSVC\\([\d.]+)\\' } |
        Sort-Object { [version]([regex]::Match($_.FullName, '\\MSVC\\([\d.]+)\\').Groups[1].Value) } |
        Select-Object -Last 1
}
$dumpbin = Get-Newest 'VC\Tools\MSVC\*\bin\Hostx64\x64\dumpbin.exe'
$crt = Get-Newest 'VC\Redist\MSVC\*\x64\Microsoft.VC*.CRT'
if (-not $dumpbin -or -not $crt) { throw "dumpbin or the x64 CRT folder is missing under $vs." }

$dependents = @(& $dumpbin.FullName /nologo /dependents $Engine |
        ForEach-Object { $_.Trim() } | Where-Object { $_ -match '\.dll$' })
if ($LASTEXITCODE -ne 0 -or $dependents.Count -eq 0) {
    throw "dumpbin could not read the imports of $Engine."
}
$needed = @($dependents | Where-Object { $_ -match '^(vcruntime|msvcp|vcomp|vccorlib|concrt)\d' })

$config = Get-Content -Raw (Join-Path $PSScriptRoot 'tauri.release.conf.json') | ConvertFrom-Json
$listed = @($config.bundle.resources.PSObject.Properties.Name |
        Where-Object { $_ -like 'redist/*' } | ForEach-Object { Split-Path $_ -Leaf })

$unlisted = @($needed | Where-Object { $listed -notcontains $_ })
if ($unlisted.Count -gt 0) {
    throw "The engine imports $($unlisted -join ', '), which tauri.release.conf.json does not ship."
}

$redist = Join-Path $PSScriptRoot 'redist'
New-Item -ItemType Directory -Force $redist | Out-Null
foreach ($name in $listed) {
    Copy-Item (Join-Path $crt.FullName $name) $redist -Force
}
Write-Output "Staged $($listed -join ', ') from $($crt.FullName)"
Write-Output "The engine imports: $($needed -join ', ')"
