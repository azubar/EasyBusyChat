param(
    [ValidateSet('x86_64-pc-windows-msvc', 'i686-pc-windows-msvc')]
    [string]$Target = 'x86_64-pc-windows-msvc',
    [string]$SigningThumbprint,
    [string]$TimestampUrl = 'http://timestamp.digicert.com'
)
$ErrorActionPreference = 'Stop'
$buildRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$previousExe = $env:EASYBUSYCHAT_UI_EXE
$previousSkip = $env:EASYBUSYCHAT_SKIP_UI_BUILD
Push-Location $buildRoot
try {
    & node scripts/check-release-defaults.cjs
    if ($LASTEXITCODE -ne 0) { throw 'Release defaults validation failed' }
    if ($env:RUSTFLAGS -or $env:CARGO_ENCODED_RUSTFLAGS) {
        throw 'Remove RUSTFLAGS/CARGO_ENCODED_RUSTFLAGS overrides to use the supported static CRT configuration'
    }
    Remove-Item Env:EASYBUSYCHAT_SKIP_UI_BUILD -ErrorAction SilentlyContinue
    & cargo build --locked --release --package easy_busy_chat_ui --bin EasyBusyUI --target $Target --target-dir target/ui_target
    if ($LASTEXITCODE -ne 0) { throw 'UI build failed' }
    $uiArtifact = Join-Path $buildRoot "target/ui_target/$Target/release/EasyBusyUI.exe"
    if ($SigningThumbprint) {
        & signtool sign /sha1 $SigningThumbprint /tr $TimestampUrl /td sha256 /fd sha256 $uiArtifact
        if ($LASTEXITCODE -ne 0) { throw 'UI signing failed' }
        & signtool verify /pa $uiArtifact
        if ($LASTEXITCODE -ne 0) { throw 'UI signature verification failed' }
    }
    $uiHash = (Get-FileHash -LiteralPath $uiArtifact -Algorithm SHA256).Hash
    $env:EASYBUSYCHAT_UI_EXE = $uiArtifact
    & cargo build --locked --release --package easy_busy_chat_addin --target $Target --target-dir target
    if ($LASTEXITCODE -ne 0) { throw 'DLL build failed' }
    if ((Get-FileHash -LiteralPath $uiArtifact -Algorithm SHA256).Hash -ne $uiHash) { throw 'UI changed during embedding' }
    $dllArtifact = Join-Path $buildRoot "target/$Target/release/EasyBusyChat.dll"
    if ($SigningThumbprint) {
        & signtool sign /sha1 $SigningThumbprint /tr $TimestampUrl /td sha256 /fd sha256 $dllArtifact
        if ($LASTEXITCODE -ne 0) { throw 'DLL signing failed' }
        & signtool verify /pa $dllArtifact
        if ($LASTEXITCODE -ne 0) { throw 'DLL signature verification failed' }
    }
    [PSCustomObject]@{ DLL = $dllArtifact; EmbeddedUI = $uiArtifact; UISha256 = $uiHash }
} finally {
    $env:EASYBUSYCHAT_UI_EXE = $previousExe
    $env:EASYBUSYCHAT_SKIP_UI_BUILD = $previousSkip
    Pop-Location
}
