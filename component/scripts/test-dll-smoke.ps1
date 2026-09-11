param(
    [Parameter(Mandatory=$true)][string]$Dll,
    [ValidateSet('x86_64-pc-windows-msvc','i686-pc-windows-msvc')]
    [string]$Target = 'x86_64-pc-windows-msvc',
    [string]$ExpectedUI
)
$ErrorActionPreference = 'Stop'
$smokeRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$smokeOutput = Join-Path $smokeRoot "target/dll-smoke-$Target.exe"
if ($env:CARGO_TARGET_DIR) { $smokeOutput = Join-Path $env:CARGO_TARGET_DIR "dll-smoke-$Target.exe" }
New-Item -ItemType Directory -Force -Path (Split-Path $smokeOutput) | Out-Null
& rustc --edition=2021 --target $Target (Join-Path $PSScriptRoot 'dll-smoke.rs') -o $smokeOutput
if ($LASTEXITCODE -ne 0) { throw 'Smoke harness compilation failed' }
if ($ExpectedUI) { & $smokeOutput $Dll $ExpectedUI } else { & $smokeOutput $Dll }
if ($LASTEXITCODE -ne 0) { throw 'DLL smoke test failed' }
