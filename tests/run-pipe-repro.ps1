$ErrorActionPreference = 'Stop'
$auditRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$auditSource = [IO.File]::ReadAllText((Join-Path $auditRoot 'component/ui/src/ipc.rs'))
$auditStart = $auditSource.IndexOf('fn pipe_write_all(')
$auditEnd = $auditSource.IndexOf('fn set_pipe_nonblocking(', $auditStart)
if ($auditStart -lt 0 -or $auditEnd -le $auditStart) { throw 'Production writer function was not found' }
$testDirectory = Join-Path $auditRoot '.local/pipe-test'
New-Item -ItemType Directory -Force -Path $testDirectory | Out-Null
[IO.File]::WriteAllText((Join-Path $testDirectory 'pipe-write.inc.rs'), $auditSource.Substring($auditStart, $auditEnd - $auditStart))
$previousTestDirectory = $env:EASYBUSY_PIPE_TEST_DIR
try {
    $env:EASYBUSY_PIPE_TEST_DIR = $testDirectory
    & rustc '--edition=2021' (Join-Path $PSScriptRoot 'pipe-repro.rs') '-o' (Join-Path $testDirectory 'pipe-repro.exe')
    if ($LASTEXITCODE -ne 0) { throw 'Pipe regression harness compilation failed' }
    & (Join-Path $testDirectory 'pipe-repro.exe')
    if ($LASTEXITCODE -ne 0) { throw 'Pipe regression test failed' }
} finally {
    $env:EASYBUSY_PIPE_TEST_DIR = $previousTestDirectory
}
