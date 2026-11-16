# Copies the Visual C++ runtime DLLs the installer ships into redist/, from
# the Visual Studio that built the engine, and the Vulkan loader (see the end).
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

$runtime = @($listed | Where-Object { $_ -ne 'vulkan-1.dll' })
$redist = Join-Path $PSScriptRoot 'redist'
New-Item -ItemType Directory -Force $redist | Out-Null
foreach ($name in $runtime) {
    Copy-Item (Join-Path $crt.FullName $name) $redist -Force
}
Write-Output "Staged $($runtime -join ', ') from $($crt.FullName)"
Write-Output "The engine imports: $($needed -join ', ')"

# The Vulkan loader. A release engine is built with whisper.cpp's Vulkan
# backend and links vulkan-1.dll, which comes with graphics drivers, not with
# Windows. Shipped beside the engine, it lets a PC with no Vulkan driver start
# the engine anyway: ggml finds no GPU there, and Whisper runs on the CPU.
# The copy is the one the Vulkan SDK installer put in System32 (its runtime
# component), and it must be that SDK's version, signed by LunarG.
if ($dependents -notcontains 'vulkan-1.dll') {
    throw 'The engine does not link vulkan-1.dll: build it with `--features vulkan`, as release.yml does.'
}
if ($listed -notcontains 'vulkan-1.dll') {
    throw 'The engine imports vulkan-1.dll, which tauri.release.conf.json does not ship.'
}
if (-not $env:VULKAN_SDK) { throw 'VULKAN_SDK is not set, so the loader to ship cannot be checked.' }
$sdkVersion = Split-Path $env:VULKAN_SDK -Leaf
$loader = Join-Path $env:SystemRoot 'System32\vulkan-1.dll'
$loaderVersion = (Get-Item $loader).VersionInfo.FileVersion
if ($loaderVersion -ne $sdkVersion) {
    throw "$loader is version $loaderVersion, not the Vulkan SDK's $sdkVersion."
}
$signature = Get-AuthenticodeSignature $loader
if ($signature.Status -ne 'Valid' -or $signature.SignerCertificate.Subject -notmatch '^CN="LunarG, Inc\."') {
    throw "$loader is not signed by LunarG ($($signature.Status))."
}
Copy-Item $loader $redist -Force
Write-Output "Staged vulkan-1.dll $loaderVersion from $loader"
