# ==========================================================
# FOWL ENGINE SERVICE STARTER
# ==========================================================

# DCS instance's Saved Games folder -- the one DCS/bflib actually run under
# (bflib writes its Logs/state here at runtime). Everything below is a
# subfolder of this, not the old, unused "server_2" folder.
$dcsWriteDir = "C:\Users\ATPAdmin\Saved Games\DCS"

# Path to the netidx resolver config. Only used when $netidxBase below is
# non-empty. One-time setup: copy netidx.exe next to bfdb.exe / campaign CFG
# ($dcsWriteDir), and copy netidx/netidx-client.json.example to
# %APPDATA%\netidx\client.json for BOTH this account and the account that
# runs DCS.exe/bflib.dll.
$netidxResolverConfig = Join-Path $PSScriptRoot "netidx\netidx-resolver.json"

# Path to bfdb.exe (copy it here from target\release\bfdb.exe)
$bfdbExe = Join-Path $dcsWriteDir "bfdb.exe"

# netidx resolver CLI — always next to bfdb.exe and the campaign CFG
$netidxExe = Join-Path $dcsWriteDir "netidx.exe"

# Path where the Sled database will be stored (dedicated folder)
$dbPath = Join-Path $dcsWriteDir "bfdb"

# Path to your campaign branding config JSON
$configPath = Join-Path $dcsWriteDir "campaign.json"

# Path to the campaign ENGINE config JSON that bflib actually loads at
# mission start (distinct from $configPath above, which is just dashboard
# branding). Enables the admin CFG editor at /api/admin/cfg -- without this
# set, GET/POST to that endpoint fails with "engine config not configured".
$engineConfigPath = Join-Path $dcsWriteDir "Caucasus1985-SARH_CFG"

# Path to the stats JSONL file written by bflib
# bflib writes this file as missions run — e.g. DCS\Logs\stats.jsonl
$statsJsonl = Join-Path $dcsWriteDir "Logs\stats.jsonl"

# Path to the netidx-archive stats directory bflib writes alongside
# stats.jsonl (Logs\stats). Enables /api/admin/perf-history (Engine
# Performance History) -- without this set, that endpoint has no data.
$statsDir = Join-Path $dcsWriteDir "Logs\stats"

# Path where bfdb.exe will natively write its plain-text logs
$logFile = Join-Path $dcsWriteDir "Logs\bfdb.log"

# bfdb binds here only. Public HTTPS is Caddy -> this address
# (see deploy/stats.Caddyfile.snippet). Do not expose :8880 on the firewall.
$listenAddress = "127.0.0.1:8880"

# Public hostname users type (obscure URL; not linked from the website yet).
$publicStatsUrl = "https://stats.attrition.cz"

# Admin panel local login (username + password — no Discord required)
# Leave blank to disable local login (Discord-only)
$adminUsername = "admin"

# Discord OAuth2 login. Client ID/secret live in bfsystem.local.ps1 (secrets,
# gitignored) as $discordClientId / $discordClientSecret -- leave those blank
# there to disable Discord login entirely.
#
# Redirect URI must match EXACTLY (byte-for-byte) the redirect URL registered
# under OAuth2 -> Redirects in the Discord Developer Portal for this app.
# Same-origin behind Caddy: use the public HTTPS host, not 127.0.0.1:8880.
$discordRedirectUri  = "https://stats.attrition.cz/api/auth/callback"

# Right-click your Discord server icon -> Copy Server ID (needs Developer
# Mode enabled in Discord: User Settings -> Advanced).
$discordGuildId      = ""

# Right-click the role that should grant dashboard admin access -> Copy Role ID.
$discordAdminRoleId  = ""

# SRS radio panel — Ciribob HTTP API base (bfdb GETs {url}/clients + X-API-KEY).
# Voice clients still use :5002; this is only the localhost HTTP port (often 8081).
# Leave blank to disable (or set srsUrl in campaign.json instead).
$srsUrl = "http://localhost:8081"
# Optional: HTTP_SERVER_API_KEY from SRS.cfg. Empty is fine when the cfg key is blank
# (bfdb still sends the X-API-KEY header). Put a real secret in bfsystem.local.ps1.
$srsApiKey = ""

# DCSServerBot's RestAPI plugin — bfdb has no Discord-link database of its
# own; it resolves a Discord user's ucid by querying the bot's own player
# database here (POST {url}/getuser), and sources the dashboard's restart
# countdown from here too (GET {url}/servers). MUST include whatever
# `prefix` is set to in the bot's restapi.yaml — e.g. if that file has
# `prefix: stats`, this needs to be "http://127.0.0.1:9876/stats", NOT
# just "http://127.0.0.1:9876" (the API doesn't live at the bare root).
# No trailing slash. Leave blank to disable linking entirely (dashboard
# "My Profile", the cockpit UI, and the restart countdown will all behave
# as if nothing is configured, rather than erroring).
$dcsServerBotUrl = ""

# X-API-Key from the bot's restapi.yaml (api_key). Secret -- lives in
# bfsystem.local.ps1 as $dcsServerBotApiKey, not here.

# Same-origin behind Caddy (stats host serves SPA + proxies /api): leave empty.
# Only set origins if the SPA is on a different host than bfdb (then cookies
# need SameSite=None; Secure and bfdb must be reached over HTTPS).
$corsOrigins = @()

# Must match "netidx_base" in the engine CFG ($engineConfigPath). bflib and
# bfdb both append the mission Sortie themselves (…/<sortie>/api/…, …/log);
# do NOT put the sortie in this value. Leave blank to run bfdb without
# netidx (JSONL stats still work; LIVE badge / engine RPC / engine logs will not).
# One-time setup: netidx.exe in $dcsWriteDir (beside bfdb.exe / CFG), resolver
# at netidx\netidx-resolver.json, and %APPDATA%\netidx\client.json on BOTH the
# bfdb account and the DCS account (see netidx\netidx-client.json.example).
$netidxBase = "/local/fowl/campaign"

# ==========================================================
# Secrets: bfsystem.ps1 is tracked in git (public repo). $adminPassword lives
# in bfsystem.local.ps1 instead, which is gitignored and never committed.
# Copy bfsystem.local.ps1.example to bfsystem.local.ps1 and fill in a real
# password to get started.
# Defaults here so an older bfsystem.local.ps1 that predates Discord login
# (and so doesn't set these) doesn't leave them $null -- $null -ne "" is
# true in PowerShell, which would otherwise pass a literal null through to
# bfdb's argument list.
$discordClientId     = ""
$discordClientSecret = ""
$dcsServerBotApiKey  = ""
if ($null -eq $srsApiKey) { $srsApiKey = "" }
$localSecrets = Join-Path $PSScriptRoot "bfsystem.local.ps1"
if (-not (Test-Path $localSecrets)) {
    Write-Host "Missing $localSecrets -- copy bfsystem.local.ps1.example and set a real `$adminPassword." -ForegroundColor Red
    exit 1
}
. $localSecrets
if ([string]::IsNullOrWhiteSpace($adminPassword)) {
    Write-Host "`$adminPassword is empty in $localSecrets -- set a real password." -ForegroundColor Red
    exit 1
}

# ==========================================================

function Start-FowlStats {
    Write-Host "Cleaning up existing processes..." -ForegroundColor Gray
    Stop-Process -Name "bfdb" -Force -ErrorAction SilentlyContinue
    Get-Job -Name "DBEngine" -ErrorAction SilentlyContinue | Remove-Job -Force -ErrorAction SilentlyContinue

    # Only recycle the netidx resolver if it's NOT already serving. Killing a
    # healthy resolver drops bflib's publisher inside DCS for ~60s (its
    # heartbeat TTL) -- so if the FowlEngine supervisor relaunches bfdb, we
    # don't want to also blind the engine for a minute. If 4564 is already
    # listening, leave the resolver (and its job) alone.
    $resolverUp = Test-NetConnection -ComputerName 127.0.0.1 -Port 4564 -InformationLevel Quiet -WarningAction SilentlyContinue
    if (-not $resolverUp) {
        Stop-Process -Name "netidx" -Force -ErrorAction SilentlyContinue
        Get-Job -Name "NetidxResolver" -ErrorAction SilentlyContinue | Remove-Job -Force -ErrorAction SilentlyContinue
    } else {
        Write-Host "netidx resolver already listening on 127.0.0.1:4564 -- leaving it running." -ForegroundColor Gray
    }

    # Ensure DB directory exists
    if (-not (Test-Path $dbPath)) {
        New-Item -ItemType Directory -Path $dbPath | Out-Null
        Write-Host "Created DB directory: $dbPath" -ForegroundColor Gray
    }

    # bfdb's --base needs a running netidx resolver to subscribe to, and
    # bflib (inside DCS) needs one to publish to -- start it first so it's
    # up before bfdb tries to connect. netidx.exe lives next to bfdb.exe / CFG
    # ($dcsWriteDir); %APPDATA%\netidx\client.json must exist for this account.
    if ($netidxBase -ne "" -and $resolverUp) {
        Write-Host "Skipping netidx resolver start -- already up on 127.0.0.1:4564." -ForegroundColor Gray
    }
    elseif ($netidxBase -ne "") {
        if (-not (Test-Path $netidxExe)) {
            Write-Host "netidx.exe not found at $netidxExe -- copy it next to bfdb.exe / the campaign CFG, or clear `$netidxBase to skip the resolver." -ForegroundColor Red
        } elseif (-not (Test-Path $netidxResolverConfig)) {
            Write-Host "Resolver config missing: $netidxResolverConfig" -ForegroundColor Red
        } else {
            Write-Host "Starting netidx resolver ($netidxExe)..." -ForegroundColor Cyan
            Start-Job -Name "NetidxResolver" -ScriptBlock {
                & $using:netidxExe resolver-server -f -c $using:netidxResolverConfig
            } | Out-Null
            Start-Sleep -Seconds 2
            $resolverJob = Get-Job -Name "NetidxResolver"
            if ($resolverJob.State -eq "Failed" -or $resolverJob.State -eq "Completed") {
                Write-Host "Resolver job exited immediately (state: $($resolverJob.State)) -- it's not actually running. Output below:" -ForegroundColor Red
                Receive-Job -Name "NetidxResolver" -Keep
            } elseif (-not (Test-NetConnection -ComputerName 127.0.0.1 -Port 4564 -InformationLevel Quiet -WarningAction SilentlyContinue)) {
                Write-Host "Resolver job is running but nothing is listening on 127.0.0.1:4564 yet -- check netidx\netidx-resolver.json addr and the resolver output in the status loop below." -ForegroundColor Yellow
            } else {
                Write-Host "Resolver is listening on 127.0.0.1:4564." -ForegroundColor Green
            }
        }
    }

    Write-Host "Starting bfdb..." -ForegroundColor Cyan

    Start-Job -Name "DBEngine" -ScriptBlock {
        $argList = @(
            "--db",             $using:dbPath,
            "--config",         $using:configPath,
            "--stats-jsonl",    $using:statsJsonl,
            "--listen-address", $using:listenAddress,
            "--admin-username", $using:adminUsername,
            "--admin-password", $using:adminPassword
        )
        if ($using:srsUrl -ne "") {
            $argList += "--srs-url", $using:srsUrl
            $argList += "--srs-api-key", $using:srsApiKey
        }
        if ($using:dcsServerBotUrl -ne "" -and $using:dcsServerBotApiKey -ne "") {
            $argList += "--dcsserverbot-url", $using:dcsServerBotUrl
            $argList += "--dcsserverbot-api-key", $using:dcsServerBotApiKey
        }
        if ($using:netidxBase -ne "") {
            $argList += "--base", $using:netidxBase
        }
        if ($using:engineConfigPath -ne "") {
            $argList += "--engine-config", $using:engineConfigPath
        }
        if ($using:statsDir -ne "") {
            $argList += "--stats-dir", $using:statsDir
        }
        foreach ($origin in $using:corsOrigins) {
            $argList += "--cors-origin", $origin
        }
        if ($using:discordClientId -ne "" -and $using:discordClientSecret -ne "") {
            $argList += "--discord-client-id",     $using:discordClientId
            $argList += "--discord-client-secret", $using:discordClientSecret
            $argList += "--discord-redirect-uri",  $using:discordRedirectUri
            $argList += "--discord-guild-id",      $using:discordGuildId
            $argList += "--discord-admin-role-id", $using:discordAdminRoleId
        }
        if ($using:logFile -ne "") {
            $argList += "--log-file", $using:logFile
        }
        & $using:bfdbExe @argList
    } | Out-Null

    $dashPort = $listenAddress.Split(':')[1]
    Write-Host "bfdb API  : http://127.0.0.1:$dashPort/  (Caddy only)" -ForegroundColor Green
    Write-Host "Public UI : $publicStatsUrl" -ForegroundColor Green
    Write-Host "Press 'Q' to stop and exit.`n" -ForegroundColor Yellow

    while ($true) {
        if ([console]::KeyAvailable) {
            $key = [console]::ReadKey($true)
            if ($key.Key -eq 'Q') { Stop-FowlStats; break }
        }

        Clear-Host
        if (Get-Job -Name "NetidxResolver" -ErrorAction SilentlyContinue) {
            Write-Host "=========== NETIDX RESOLVER ===========" -ForegroundColor DarkCyan
            $resolverJob = Get-Job -Name "NetidxResolver"
            Write-Host "Job state: $($resolverJob.State)" -ForegroundColor Gray
            Receive-Job -Name "NetidxResolver" -Keep | Select-Object -Last 10
            Write-Host ""
        }
        Write-Host "=========== FOWL ENGINE DB ===========" -ForegroundColor Magenta
        $dbJob = Get-Job -Name "DBEngine" -ErrorAction SilentlyContinue
        if ($dbJob -and $dbJob.State -eq "Running") {
            Write-Host "Status: Running" -ForegroundColor Green
            Write-Host "Logs   : $logFile" -ForegroundColor Gray
        } else {
            $state = if ($dbJob) { $dbJob.State } else { "Not started" }
            Write-Host "Status: $state" -ForegroundColor Red
            Receive-Job -Name "DBEngine" -Keep | Select-Object -Last 20
        }

        Write-Host "`n======================================"
        Write-Host "Public: $publicStatsUrl  |  API: 127.0.0.1:$dashPort  |  Press 'Q' to shutdown" -ForegroundColor Gray

        Start-Sleep -Seconds 2
    }
}

function Stop-FowlStats {
    Write-Host "`nShutting down..." -ForegroundColor Red
    Get-Job | Stop-Job  -ErrorAction SilentlyContinue
    Get-Job | Remove-Job -Force -ErrorAction SilentlyContinue
    Stop-Process -Name "bfdb" -Force -ErrorAction SilentlyContinue
    Stop-Process -Name "netidx" -Force -ErrorAction SilentlyContinue
    Write-Host "Stopped." -ForegroundColor Green
    Start-Sleep -Seconds 1
    exit
}

Start-FowlStats