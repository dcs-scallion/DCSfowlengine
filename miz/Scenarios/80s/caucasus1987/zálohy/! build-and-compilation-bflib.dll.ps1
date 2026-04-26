############################################################
# Load shared file/location configuration                  #
############################################################
$locationsFile = Join-Path -Path $PSScriptRoot -ChildPath "- EDIT-FILE-LOCATIONS.txt"
if (-not (Test-Path -LiteralPath $locationsFile)) {
    Write-Host "ERROR: Missing configuration file: $locationsFile" -ForegroundColor Red
    Write-Host "Create or update '- EDIT-FILE-LOCATIONS.txt' next to this script." -ForegroundColor Red
    exit 1
}
$locationsContent = Get-Content -LiteralPath $locationsFile -Raw -ErrorAction Stop
. ([ScriptBlock]::Create($locationsContent))


# When the DLL is locked (e.g. DCS running), retry copy for this long / this interval.
$CopyRetry_MaxWaitSeconds      = 300   # 5 minutes
$CopyRetry_IntervalSeconds     = 5
$CopyRetry_ProgressEveryNAttempts = 6  # status line every 6 * 5s = 30s

##################################
# Compile and copy bflib.dll     #
##################################

function Copy-ItemWithLockRetry {
    <#
    .SYNOPSIS
        Copy a file; if the destination is locked or busy, retry until timeout.
    #>
    param(
        [Parameter(Mandatory = $true)][string]$Source,
        [Parameter(Mandatory = $true)][string]$Destination,
        [Parameter(Mandatory = $true)][string]$ManualInstructions,
        [int]$MaxWaitSeconds = 300,
        [int]$IntervalSeconds = 5,
        [int]$ProgressEveryNAttempts = 6
    )

    $deadline = (Get-Date).AddSeconds($MaxWaitSeconds)
    $attempt = 0
    $warned = $false

    while ($true) {
        $attempt++
        try {
            $destDir = Split-Path -Parent $Destination
            if (-not (Test-Path -LiteralPath $destDir)) {
                New-Item -ItemType Directory -Path $destDir -Force -ErrorAction Stop | Out-Null
            }
            Copy-Item -LiteralPath $Source -Destination $Destination -Force -ErrorAction Stop
            return
        }
        catch {
            if ((Get-Date) -ge $deadline) {
                throw (@"
$ManualInstructions

Timed out while writing to:
  $Destination
"@)
            }
            if (-not $warned) {
                Write-Host "Cannot write to: $Destination" -ForegroundColor Yellow
                Write-Host "  ($($_.Exception.Message))" -ForegroundColor DarkYellow
                Write-Host "  Retrying every ${IntervalSeconds}s for up to ${MaxWaitSeconds}s (e.g. quit DCS to release bflib.dll)." -ForegroundColor Yellow
                $warned = $true
            }
            if (($attempt % $ProgressEveryNAttempts) -eq 0) {
                $remaining = [int][math]::Ceiling([math]::Max(0, ($deadline - (Get-Date)).TotalSeconds))
                Write-Host "  Still waiting... ~$remaining s remaining" -ForegroundColor DarkYellow
            }
            Start-Sleep -Seconds $IntervalSeconds
        }
    }
}

$LogFile = Join-Path -Path $PSScriptRoot -ChildPath "! build-and-compilation-bflib.dll-LOG.txt"
Start-Transcript -Path $LogFile -Append

try {
    foreach ($name in @("work_path_engine", "path_engine_mission", "DCS_user_path")) {
        $v = (Get-Variable -Name $name -ErrorAction SilentlyContinue).Value
        if ([string]::IsNullOrWhiteSpace($v)) {
            throw "Configuration variable '$name' is missing or empty in '- EDIT-FILE-LOCATIONS.txt'."
        }
    }

    Set-Location -Path "$work_path_engine" -ErrorAction Stop

    $env:LUA_LIB = Get-Location
    $env:LUA_LINK = "dylib"
    $env:LUA_LIB_NAME = "lua"

    Write-Host "`n--- Rust / Lua env (LUA_*) ---" -ForegroundColor Cyan
    Get-ChildItem Env:LUA*
    Write-Host "------------------------------------------`n" -ForegroundColor Cyan

    Write-Host "`n--- bflib.dll build started: $(Get-Date) ---" -ForegroundColor Cyan

    Write-Host "`nRunning cargo clean..."
    cargo clean
    if ($LASTEXITCODE -ne 0) {
        Write-Host "WARNING: cargo clean failed (files under target\ may be locked by IDE, another cargo, or AV). Continuing with release build without clean." -ForegroundColor Yellow
    }

    Write-Host "`nStarting release build: package bflib..." -ForegroundColor Yellow
    cargo build --release --package=bflib
    $buildSuccess = ($LASTEXITCODE -eq 0)

    if ($buildSuccess) {
        Write-Host "`nBuild succeeded." -ForegroundColor Green

        $destMizRoot = Join-Path $work_path_engine "miz\bflib.dll"
        $destMission = Join-Path $work_path_engine ($path_engine_mission.TrimStart('\') + "\bflib.dll")
        $destDcs = Join-Path $DCS_user_path "Scripts\bflib.dll"
        $srcDll = Join-Path $work_path_engine "target\release\bflib.dll"

        if (-not (Test-Path -LiteralPath $srcDll)) {
            throw "Build reported success but DLL not found: $srcDll"
        }

        $manualAll = @"
Could not copy bflib.dll to at least one destination after waiting $CopyRetry_MaxWaitSeconds seconds (file still locked or busy).

Built DLL (source — copy this file manually when nothing is using it):
  $srcDll

Copy it to all of these (close DCS first if the Scripts copy fails):
  1) Repo miz folder:
     $destMizRoot
  2) This scenario folder:
     $destMission
  3) DCS user Scripts (used at runtime):
     $destDcs
"@

        $copyTargets = @(
            @{ Path = $destMizRoot;   Label = "repo miz" }
            @{ Path = $destMission;   Label = "scenario folder" }
            @{ Path = $destDcs;       Label = "DCS Scripts" }
        )

        foreach ($t in $copyTargets) {
            Write-Host "`nCopying bflib.dll to: $($t.Path) ($($t.Label))" -ForegroundColor Cyan
            Copy-ItemWithLockRetry -Source $srcDll -Destination $t.Path -ManualInstructions $manualAll `
                -MaxWaitSeconds $CopyRetry_MaxWaitSeconds `
                -IntervalSeconds $CopyRetry_IntervalSeconds `
                -ProgressEveryNAttempts $CopyRetry_ProgressEveryNAttempts
            Write-Host "  OK: $($t.Path)" -ForegroundColor Green
        }

        Write-Host "`nAll copy steps completed." -ForegroundColor Green
    }
    else {
        Write-Host "`nBuild failed. Check errors above." -ForegroundColor Red
        $skip = "Build failed: skipping copy of bflib.dll (miz, mission folder, and DCS Scripts were not updated)."
        Write-Host $skip -ForegroundColor Yellow
    }
}
catch {
    Write-Host "`nERROR:" -ForegroundColor Red
    Write-Host $_.Exception.Message -ForegroundColor Red
    Write-Host "`nScript stopped early." -ForegroundColor Red
    exit 1
}
finally {
    Write-Host "`n--- Process finished: $(Get-Date) ---"
    Stop-Transcript
    Read-Host "Press Enter to close"
}
