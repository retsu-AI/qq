param([Parameter(Mandatory)][string]$TestBinary)
$ErrorActionPreference = 'Stop'
$root = Join-Path $env:RUNNER_TEMP 'qq-cargo-access-trace'
New-Item -ItemType Directory -Path $root -Force | Out-Null
function Invoke-TraceCommand([string[]]$Arguments, [int]$TimeoutMs) {
    $command = Start-Process -FilePath $procmon -ArgumentList $Arguments -PassThru
    if (!$command.WaitForExit($TimeoutMs)) {
        $command.Kill($true)
        throw 'Process Monitor command exceeded bounded wait'
    }
    if ($command.ExitCode -ne 0) { throw "Process Monitor command failed: $($command.ExitCode)" }
}
$pml = Join-Path $root 'private-trace.pml'
$csv = Join-Path $root 'private-trace.csv'
$selected = Join-Path $env:RUNNER_TEMP 'qq-cargo-access-denials.json'
# Raw tracing stays on the disposable runner; only four selected fields leave it.
$capture = $null
$traceReady = $false
$windowExpired = $false
try {
    $zip = Join-Path $root 'procmon.zip'
    Invoke-WebRequest -Uri 'https://download.sysinternals.com/files/ProcessMonitor.zip' -OutFile $zip -TimeoutSec 30
    Expand-Archive -LiteralPath $zip -DestinationPath $root
    $name = if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') { 'Procmon64a.exe' } else { 'Procmon64.exe' }
    $procmon = Join-Path $root $name
    $signature = Get-AuthenticodeSignature -LiteralPath $procmon
    if ($signature.Status -ne 'Valid' -or $signature.SignerCertificate.Subject -notmatch 'O=Microsoft Corporation(?:,|$)') {
        throw 'Expected valid Microsoft-signed Process Monitor'
    }
    $captureClock = [Diagnostics.Stopwatch]::StartNew()
    $capture = Start-Process -FilePath $procmon -ArgumentList @('/AcceptEula', '/Quiet', '/Minimized', '/NoFilter', '/Runtime', '30', '/BackingFile', "`"$pml`"") -PassThru
    Invoke-TraceCommand @('/AcceptEula', '/WaitForIdle') 10000
    $traceReady = $true
} catch {
    Write-Host "Selected access trace setup incomplete: $($_.Exception.Message)"
}
$testExit = 1
try {
    & $TestBinary runtime::windows_cargo_tests::installed_cargo_startup_through_real_auto_worker --exact --nocapture 2>&1 |
        Tee-Object -FilePath (Join-Path $env:RUNNER_TEMP 'qq-cargo-startup.log')
    $testExit = $LASTEXITCODE
} finally {
    if ($null -ne $capture) {
        $windowExpired = $captureClock.Elapsed.TotalSeconds -ge 30
        try { Invoke-TraceCommand @('/AcceptEula', '/Terminate', '/Quiet') 10000 }
        catch { $traceReady = $false; Write-Host "Selected access trace stop incomplete: $($_.Exception.Message)" }
        if (!$capture.WaitForExit(10000)) { $capture.Kill($true); $traceReady = $false; Write-Host 'Trace shutdown unconfirmed' }
    }
}
try {
    if (!$traceReady) { throw 'Trace was not ready and confirmed stopped' }
    if (!(Test-Path -LiteralPath $pml) -or (Get-Item -LiteralPath $pml).Length -gt 134217728) {
        throw 'Trace missing or exceeds 128 MiB export bound'
    }
    Invoke-TraceCommand @('/AcceptEula', '/Quiet', '/OpenLog', "`"$pml`"", '/SaveAs', "`"$csv`"") 30000
    $rows = @(Import-Csv -LiteralPath $csv | Where-Object {
        $_.'Process Name' -in @('cargo.exe', 'windows_cargo_images.exe') -and $_.Result -eq 'ACCESS DENIED'
    } | Select-Object -First 129 | ForEach-Object {
        [pscustomobject]@{ process = $_.'Process Name'; operation = $_.Operation; path = $_.Path.Substring(0, [Math]::Min($_.Path.Length, 4096)); result = $_.Result }
    })
    [pscustomobject]@{ incomplete = ($windowExpired -or $rows.Count -gt 128); capture_window_expired = $windowExpired; denials = @($rows | Select-Object -First 128) } |
        ConvertTo-Json -Depth 4 | Set-Content -LiteralPath $selected -Encoding utf8NoBOM
    Get-Content -LiteralPath $selected
} catch {
    # Trace failure must not hide the real worker/Cargo result.
    Write-Host "Selected access trace incomplete: $($_.Exception.Message)"
} finally {
    Remove-Item -LiteralPath $pml, $csv -Force -ErrorAction SilentlyContinue
}
exit $testExit
