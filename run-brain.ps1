# Start the brain for local development.
#
# Environment variables in PowerShell live and die with the terminal, and the agent
# token has to stay *identical* across restarts — the Pi has it exported in its own
# shell. So the token is generated once, kept in `.garden-token` (gitignored), and
# reused every run. Retyping it by hand is how you end up debugging a 401.
#
#   .\run-brain.ps1
#
# Ctrl-C stops it. SQLite is in WAL mode, so `garden-cli` can read the same database
# while this is running.

$ErrorActionPreference = "Stop"
Set-Location $PSScriptRoot

# --- Token ------------------------------------------------------------------------
$tokenFile = Join-Path $PSScriptRoot ".garden-token"
if (Test-Path $tokenFile) {
    $token = (Get-Content $tokenFile -Raw).Trim()
} else {
    $token = -join ((1..32) | ForEach-Object { '{0:x2}' -f (Get-Random -Maximum 256) })
    Set-Content -Path $tokenFile -Value $token -Encoding ascii -NoNewline
    Write-Host "generated a new agent token in .garden-token" -ForegroundColor Yellow
    Write-Host "the Pi needs the same value:" -ForegroundColor Yellow
    Write-Host "  export GARDEN_AGENT_TOKEN=$token" -ForegroundColor Yellow
}

# --- Address ----------------------------------------------------------------------
# The URL your PHONE and the Pi use, so `localhost` is wrong: notification buttons and
# invitation links are built from it, and the Pi cannot resolve your loopback.
$lanIp = (Get-NetIPAddress -AddressFamily IPv4 |
    Where-Object { $_.IPAddress -notlike "127.*" -and $_.IPAddress -notlike "169.254.*" } |
    Sort-Object -Property InterfaceMetric |
    Select-Object -First 1).IPAddress
if (-not $lanIp) { $lanIp = "localhost" }

# --- Settings ---------------------------------------------------------------------
$env:GARDEN_DB = "sqlite://garden.db"   # your account and gardens live here
$env:GARDEN_AGENT_TOKEN = $token
$env:GARDEN_BASE_URL = "http://${lanIp}:8080"
# `__Host-` cookies require HTTPS. Over plain HTTP, login fails in a way that looks
# like a wrong password rather than a misconfiguration, so this is not optional here.
$env:GARDEN_INSECURE_COOKIES = "1"

Write-Host ""
Write-Host "  brain    http://${lanIp}:8080"
Write-Host "  database $($env:GARDEN_DB)"
Write-Host "  token    $($token.Substring(0,8))... (full value in .garden-token)"
Write-Host ""
Write-Host "  On the Pi:"
Write-Host "    export GARDEN_BRAIN_URL=http://${lanIp}:8080"
Write-Host "    export GARDEN_AGENT_TOKEN=$token"
Write-Host ""

# --- Firewall check ---------------------------------------------------------------
# The commonest reason the Pi times out while the browser works fine.
$rule = Get-NetFirewallRule -DisplayName "Garden brain 8080" -ErrorAction SilentlyContinue
if (-not $rule) {
    Write-Host "No inbound firewall rule for 8080. If the Pi times out, run ONCE as admin:" -ForegroundColor Yellow
    Write-Host "  New-NetFirewallRule -DisplayName 'Garden brain 8080' -Direction Inbound -LocalPort 8080 -Protocol TCP -Action Allow -Profile Private" -ForegroundColor Yellow
    Write-Host ""
}

cargo run --release -p garden-web
