<#>
.SYNOPSIS
    NexusLink NUE-gateway smoke test.
.NOTES
    Requires: cargo, curl. Uses temp config directory.
    Upstream: expects AI-2 host at http://127.0.0.1:3124 (healthz 200).
#>

[CmdletBinding()]
param(
    [switch]$Release,
    [string]$UpstreamBaseUrl = "http://127.0.0.1:3124",
    [string]$UpstreamApiKey = "",
    [string]$BinaryPath = ""
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

function Pass { Write-Host "PASS  $($args[0])" -ForegroundColor Green }
function Fail { Write-Host "FAIL  $($args[0])" -ForegroundColor Red; exit 1 }
function Info { Write-Host "INFO $($args[0])" -ForegroundColor Cyan }

# Resolve paths
$RepoRoot = (git rev-parse --show-toplevel 2>$null).Trim()
if (-not $RepoRoot) { $RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path }
$TargetDir = if ($Release) { "target/release" } else { "target/debug" }
$Binary = if ($BinaryPath) { $BinaryPath } else { Join-Path $RepoRoot "$TargetDir/nexuslink.exe" }

if (-not (Test-Path $Binary)) {
    Info "Binary not found at $Binary, building..."
    $BuildArgs = @("build", "--bin", "nexuslink")
    if ($Release) { $BuildArgs += "--release" }
    $exit = & cargo @BuildArgs 2>&1
    if ($LASTEXITCODE -ne 0) { Fail "cargo build failed: $exit" }
}
if (-not (Test-Path $Binary)) { Fail "Binary still not found at $Binary after build" }

# Temp config directory with .nexuslink subdir (required by config loader)
$TmpDir = [System.IO.Path]::GetTempPath()
$CfgDir = Join-Path $TmpDir "nexuslink-smoke-$([guid]::NewGuid())"
$NexusDir = Join-Path $CfgDir ".nexuslink"
New-Item -ItemType Directory -Path $NexusDir -Force | Out-Null
$KeysFile = Join-Path $NexusDir "nue_keys.json"
$ConfigFile = Join-Path $NexusDir "config.json"

$script:GatewayPid = $null

function Cleanup {
    if ($script:GatewayPid) {
        Write-Host "Stopping gateway (PID $($script:GatewayPid))..." -ForegroundColor Yellow
        Stop-Process -Id $script:GatewayPid -Force -ErrorAction SilentlyContinue
        Start-Sleep -Milliseconds 500
    }
    if (Test-Path $CfgDir) {
        Remove-Item -Recurse -Force $CfgDir -ErrorAction SilentlyContinue
    }
}

# Check upstream
Info "Checking upstream AI-2 at $UpstreamBaseUrl/healthz ..."
$UpstreamHealthy = $false
try {
    $r = Invoke-WebRequest -Uri "$UpstreamBaseUrl/healthz" -Method GET -TimeoutSec 5 -UseBasicParsing -ErrorAction Stop
    if ($r.StatusCode -eq 200) {
        $UpstreamHealthy = $true
        Info "Upstream AI-2 healthy at $UpstreamBaseUrl"
    }
} catch {
    Write-Warning "Upstream $UpstreamBaseUrl/healthz not reachable: $_"
}
if (-not $UpstreamHealthy) {
    Write-Warning "Upstream AI-2 not reachable at $UpstreamBaseUrl - passthrough checks will fail"
}

# Generate test token
$IncomingToken = "nl-smoke-$(([guid]::NewGuid()).ToString().Substring(0,8))"

# Write temp config (uses free ports 17679/17680)
$Config = @{
    incoming_token = $IncomingToken
    port = 17679
    nue_port = 17680
    providers = @()
    nue_upstream = @{
        base_url = $UpstreamBaseUrl
        api_key = if ($UpstreamApiKey) { $UpstreamApiKey } else { "test-key" }
    }
} | ConvertTo-Json -Depth 5
$Config | Set-Content -Path $ConfigFile -Encoding UTF8

'{"keys":[]}' | Set-Content -Path $KeysFile -Encoding UTF8

Info "Config written to $ConfigFile"
Info "Keys file: $KeysFile"
Get-Content $ConfigFile

# HOME/USERPROFILE must point to parent of .nexuslink
$Env:HOME = $CfgDir
$Env:USERPROFILE = $CfgDir

Info "Starting nexuslink gateway on port 17679 + NUE port 17680..."
$Gateway = Start-Process -FilePath $Binary -ArgumentList "serve" -PassThru -WindowStyle Hidden
$script:GatewayPid = $Gateway.Id
Info "Gateway started (PID $($Gateway.Id))"
Start-Sleep -Seconds 3
# Check ports bound
$ports = netstat -ano | Where-Object { $_ -match ':17679|:17680' }
Info "Ports bound: $ports"
# Get PID from netstat (more reliable than Start-Process return)
$portPids = netstat -ano | Where-Object { $_ -match ':17679|:17680' } | ForEach-Object { ($_ -split '\s+')[-1] } | Select-Object -Unique
if ($portPids) {
    $script:GatewayPid = $portPids[0]
    Info "Gateway process alive on ports (PID: $($script:GatewayPid))"
} else {
    Fail "Gateway process died immediately - no ports bound"
}

function Pass { Write-Host "PASS  $($args[0])" -ForegroundColor Green }
function Fail { Write-Host "FAIL  $($args[0])" -ForegroundColor Red; exit 1 }
function Info { Write-Host "INFO $($args[0])" -ForegroundColor Cyan }

$NueBase = "http://127.0.0.1:17680"
$MainBase = "http://127.0.0.1:17679"

try {
    Info "=== NUE Gateway checks (port 17680) ==="

    # 1. healthz
    Info "GET /healthz on NUE port"
    $r = Invoke-WebRequest -Method GET -Uri "$NueBase/healthz" -TimeoutSec 5 -UseBasicParsing
    if ($r.StatusCode -ne 200) { Fail "GET /healthz -> $($r.StatusCode) (want 200)" }
    Pass "GET /healthz -> 200"

    # 2. register NUE key
    Info "POST /nue/register"
    $r = Invoke-WebRequest -Method POST -Uri "$NueBase/nue/register" -TimeoutSec 10 -UseBasicParsing
    if ($r.StatusCode -ne 200) { Fail "/nue/register returned $($r.StatusCode)" }
    $Reg = $r.Content | ConvertFrom-Json
    $NueKey = $Reg.key
    if (-not $NueKey -or -not $NueKey.StartsWith("nue_")) { Fail "register returned invalid key: $NueKey" }
    Pass "/nue/register -> 200 + nue_ key: $($NueKey.Substring(0,12))..."

    # 3. passthrough GET /v1/runs with valid key
    Info "GET /v1/runs with valid NUE key"
    $r = Invoke-WebRequest -Method GET -Uri "$NueBase/v1/runs?limit=5" -Headers @{Authorization="Bearer $NueKey"} -TimeoutSec 10 -UseBasicParsing
    if ($r.StatusCode -eq 401) { Fail "GET /v1/runs returned 401 with valid NUE key" }
    Pass "GET /v1/runs with valid key -> $($r.StatusCode) (not 401)"

    # 4. wrong key -> 401
    Info "GET /v1/runs with invalid NUE key"
    try {
        $r = Invoke-WebRequest -Method GET -Uri "$NueBase/v1/runs?limit=5" -Headers @{Authorization="Bearer nue_wrongkey123"} -TimeoutSec 10 -UseBasicParsing -ErrorAction Stop
        $code = $r.StatusCode
    } catch {
        $code = if ($_.Exception.Response) { $_.Exception.Response.StatusCode.value__ } else { 0 }
    }
    if ($code -ne 401) { Fail "Wrong key returned $code (expected 401)" }
    Pass "Wrong NUE key -> 401"

    # 5. no auth -> 401
    Info "GET /v1/runs without auth"
    try {
        $r = Invoke-WebRequest -Method GET -Uri "$NueBase/v1/runs?limit=5" -TimeoutSec 10 -UseBasicParsing -ErrorAction Stop
        $code = $r.StatusCode
    } catch {
        $code = if ($_.Exception.Response) { $_.Exception.Response.StatusCode.value__ } else { 0 }
    }
    if ($code -ne 401) { Fail "No auth returned $code (expected 401)" }
    Pass "No auth -> 401"

    # 6. deny-list: /v1/keys should 404
    Info "GET /v1/keys (deny-listed) should 404"
    try {
        $r = Invoke-WebRequest -Method GET -Uri "$NueBase/v1/keys" -Headers @{Authorization="Bearer $NueKey"} -TimeoutSec 10 -UseBasicParsing -ErrorAction Stop
        $code = $r.StatusCode
    } catch {
        $code = if ($_.Exception.Response) { $_.Exception.Response.StatusCode.value__ } else { 0 }
    }
    if ($code -ne 404) { Fail "/v1/keys returned $code (expected 404)" }
    Pass "/v1/keys deny-listed -> 404"

    # 7. deny-list: /_bus should 404
    Info "GET /_bus (deny-listed) should 404"
    try {
        $r = Invoke-WebRequest -Method GET -Uri "$NueBase/_bus" -Headers @{Authorization="Bearer $NueKey"} -TimeoutSec 10 -UseBasicParsing -ErrorAction Stop
        $code = $r.StatusCode
    } catch {
        $code = if ($_.Exception.Response) { $_.Exception.Response.StatusCode.value__ } else { 0 }
    }
    if ($code -ne 404) { Fail "/_bus returned $code (expected 404)" }
    Pass "/_bus deny-listed -> 404"

    # 8. deny-list: /v1/startup should 404
    Info "GET /v1/startup (deny-listed) should 404"
    try {
        $r = Invoke-WebRequest -Method GET -Uri "$NueBase/v1/startup" -Headers @{Authorization="Bearer $NueKey"} -TimeoutSec 10 -UseBasicParsing -ErrorAction Stop
        $code = $r.StatusCode
    } catch {
        $code = if ($_.Exception.Response) { $_.Exception.Response.StatusCode.value__ } else { 0 }
    }
    if ($code -ne 404) { Fail "/v1/startup returned $code (expected 404)" }
    Pass "/v1/startup deny-listed -> 404"

    # Main gateway sanity
    Info "=== Main Gateway sanity (port 17679) ==="
    $r = Invoke-WebRequest -Method GET -Uri "http://127.0.0.1:17679/health" -TimeoutSec 5 -UseBasicParsing -ErrorAction SilentlyContinue
    if ($r.StatusCode -eq 200) { Pass "Main /health -> 200" } else { Write-Warning "Main /health: $($r.StatusCode)" }

    Write-Host "`n=== ALL NUE SMOKE CHECKS PASSED ===" -ForegroundColor Green
}
catch {
    Write-Host "FAIL: $_" -ForegroundColor Red
    throw
}
finally {
    if ($script:GatewayPid) {
        Write-Host "Stopping gateway (PID $($script:GatewayPid))..." -ForegroundColor Yellow
        Stop-Process -Id $script:GatewayPid -Force -ErrorAction SilentlyContinue
        Start-Sleep -Milliseconds 500
    }
    if (Test-Path $CfgDir) {
        Remove-Item -Recurse -Force $CfgDir -ErrorAction SilentlyContinue
    }
}