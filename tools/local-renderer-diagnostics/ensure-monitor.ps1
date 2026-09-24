param(
    [Parameter(Mandatory = $true)][DateTimeOffset]$Until,
    [string]$OutputDirectory = (Join-Path $env:USERPROFILE '.codex-session-delete\renderer-diagnostics'),
    [int]$StaleAfterSeconds = 90,
    [switch]$CheckOnly
)
$ErrorActionPreference = 'Stop'
$runnerPath = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot 'record.mjs'))
$outputPath = [System.IO.Path]::GetFullPath($OutputDirectory)
$statePath = [System.IO.Path]::GetFullPath((Join-Path $outputPath 'monitor-state.json'))
if ($runnerPath.Contains('"') -or $outputPath.Contains('"')) { throw 'Invalid quoted path' }
if ($Until -le [DateTimeOffset]::Now) {
    [pscustomobject]@{ Status = 'expired'; Until = $Until.ToString('o') } | ConvertTo-Json
    exit 0
}
if (Test-Path -LiteralPath (Join-Path $outputPath 'STOP')) {
    [pscustomobject]@{ Status = 'manually-stopped' } | ConvertTo-Json
    exit 0
}
$expectedExpiresAt = $Until.ToUnixTimeMilliseconds()
$now = [DateTimeOffset]::Now.ToUnixTimeMilliseconds()
$existing = @(Get-CimInstance Win32_Process -Filter "Name='node.exe'" |
    Where-Object { $_.CommandLine -and $_.CommandLine.Contains($runnerPath) -and $_.CommandLine -notmatch '--(once|stop)' })
$state = $null
if (Test-Path -LiteralPath $statePath) {
    try { $state = Get-Content -LiteralPath $statePath -Raw | ConvertFrom-Json } catch { $state = $null }
}
$healthyProcess = $null
if ($state -and $state.pid -and [int64]$state.expiresAt -eq $expectedExpiresAt -and $state.lastHeartbeatAt) {
    $healthyProcess = $existing | Where-Object { [int]$_.ProcessId -eq [int]$state.pid } | Select-Object -First 1
    if ($healthyProcess -and ([int64]$state.lastHeartbeatAt -lt ($now - ($StaleAfterSeconds * 1000)))) {
        $healthyProcess = $null
    }
}
if ($healthyProcess) {
    [pscustomobject]@{
        Status = 'healthy'
        ProcessIds = @($healthyProcess.ProcessId)
        LastHeartbeatAt = [int64]$state.lastHeartbeatAt
        ExpiresAt = $expectedExpiresAt
    } | ConvertTo-Json
    exit 0
}
if ($CheckOnly) {
    [pscustomobject]@{
        Status = if ($existing.Count -gt 0) { 'stale-or-mismatched' } else { 'needs-start' }
        ProcessIds = @($existing | ForEach-Object ProcessId)
        ExpiresAt = $expectedExpiresAt
    } | ConvertTo-Json
    exit 0
}
foreach ($process in $existing) {
    Stop-Process -Id $process.ProcessId -Force -ErrorAction SilentlyContinue
}
[System.IO.Directory]::CreateDirectory($outputPath) | Out-Null
$nodePath = (Get-Command node -ErrorAction Stop).Source
$stamp = Get-Date -Format 'yyyyMMdd-HHmmss-fff'
$arguments = '"{0}" "{1}" --until={2} --state="{3}"' -f $runnerPath, $outputPath, $Until.ToUniversalTime().ToString('o'), $statePath
$monitor = Start-Process -FilePath $nodePath -ArgumentList $arguments -WorkingDirectory $PSScriptRoot -WindowStyle Hidden -PassThru `
    -RedirectStandardOutput (Join-Path $outputPath "monitor-host-$stamp.out") `
    -RedirectStandardError (Join-Path $outputPath "monitor-host-$stamp.err")
[pscustomobject]@{ Status = 'started'; ProcessId = $monitor.Id; Until = $Until.ToString('o'); StatePath = $statePath } | ConvertTo-Json
