param(
    [Parameter(Mandatory = $true)][DateTimeOffset]$Until,
    [string]$OutputDirectory = (Join-Path $env:USERPROFILE '.codex-session-delete\renderer-diagnostics'),
    [int]$CheckIntervalSeconds = 30,
    [int]$StaleAfterSeconds = 90
)
$ErrorActionPreference = 'Stop'
if ($CheckIntervalSeconds -lt 5) { throw 'CheckIntervalSeconds must be at least 5' }
if ($StaleAfterSeconds -lt $CheckIntervalSeconds) { throw 'StaleAfterSeconds must be at least CheckIntervalSeconds' }
$ensurePath = Join-Path $PSScriptRoot 'ensure-monitor.ps1'
while ([DateTimeOffset]::Now -lt $Until) {
    if (Test-Path -LiteralPath (Join-Path $OutputDirectory 'STOP')) { break }
    & powershell.exe -NoProfile -ExecutionPolicy Bypass -File $ensurePath `
        -Until $Until -OutputDirectory $OutputDirectory -StaleAfterSeconds $StaleAfterSeconds | Out-Null
    Start-Sleep -Seconds $CheckIntervalSeconds
}
