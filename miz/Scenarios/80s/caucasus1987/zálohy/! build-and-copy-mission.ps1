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

$log_file = Join-Path -Path $PSScriptRoot -ChildPath "! build-and-copy-mission-LOG.txt"

# Remove previous log
if (Test-Path $log_file) { Remove-Item $log_file -Force }

# Skip Start-Transcript (it duplicates lines); log manually instead.

function Ensure-WeaponBridgeHookRemoved {
    param(
        [Parameter(Mandatory = $true)][string]$DcsUserPath,
        [Parameter(Mandatory = $true)][string]$LogFile,
        [int]$MaxWaitSeconds = 300,
        [int]$PollSeconds = 5
    )

    $hookPath = Join-Path $DcsUserPath "Scripts\Hooks\Fowl_engine_weapon_bridge_export.lua"
    if (-not (Test-Path -LiteralPath $hookPath -PathType Leaf)) {
        return
    }

    $startMsg = "Detected temporary Lua hook in Hooks: $hookPath"
    Write-Host $startMsg -ForegroundColor Yellow
    $startMsg | Out-File -FilePath $LogFile -Append

    $deadline = (Get-Date).AddSeconds($MaxWaitSeconds)
    while ($true) {
        try {
            if (Test-Path -LiteralPath $hookPath -PathType Leaf) {
                Remove-Item -LiteralPath $hookPath -Force -ErrorAction Stop
            }
            if (-not (Test-Path -LiteralPath $hookPath -PathType Leaf)) {
                $ok = "Temporary Lua hook removed: $hookPath"
                Write-Host $ok -ForegroundColor Green
                $ok | Out-File -FilePath $LogFile -Append
                return
            }
        }
        catch {
            # most often file lock because DCS is still running; handled by wait loop below
        }

        if ((Get-Date) -ge $deadline) {
            $timeout = @"
ERROR: Mission build aborted. DCS must be closed and this Lua hook removed before build:
  $hookPath

Close DCS, delete the hook, then run '! build-and-copy-mission.ps1' again.
"@
            Write-Host $timeout -ForegroundColor Red
            $timeout | Out-File -FilePath $LogFile -Append
            throw "Lua hook cleanup timed out (5 minutes): $hookPath"
        }

        $remaining = [int][math]::Ceiling([math]::Max(0, ($deadline - (Get-Date)).TotalSeconds))
        $waitMsg = "Waiting for hook removal (close DCS if running). Retrying in ${PollSeconds}s; ~${remaining}s left."
        Write-Host $waitMsg -ForegroundColor Yellow
        $waitMsg | Out-File -FilePath $LogFile -Append
        Start-Sleep -Seconds $PollSeconds
    }
}

try {
    Write-Host "--- Process Started: $(Get-Date) ---" -ForegroundColor Cyan
    "--- Process Started: $(Get-Date) ---" | Out-File -FilePath $log_file -Append

    foreach ($name in @("DCS_user_path", "mission_name", "target_cfg_name")) {
        $v = (Get-Variable -Name $name -ErrorAction SilentlyContinue).Value
        if ([string]::IsNullOrWhiteSpace($v)) {
            throw "Configuration variable '$name' is missing or empty in '- EDIT-FILE-LOCATIONS.txt'."
        }
    }
    Ensure-WeaponBridgeHookRemoved -DcsUserPath $DCS_user_path -LogFile $log_file

    $env:RUST_LOG = "trace"
    Set-Location -Path $PSScriptRoot -ErrorAction Stop

    Write-Host "Running bftools..." -ForegroundColor Yellow

    $tagColors = @{
        "ERROR" = [ConsoleColor]::Red
        "WARN"  = [ConsoleColor]::Yellow
        "INFO"  = [ConsoleColor]::Green
        "DEBUG" = [ConsoleColor]::Cyan
        "TRACE" = [ConsoleColor]::Magenta
    }

    $exe_path = "../../../../bftools/bftools.exe"

    # Local *_CFG: clone from another *_CFG if the expected name is missing (before bftools --campaign-cfg)
    if (-not (Test-Path "./$target_cfg_name")) {
        $old_cfg = Get-ChildItem -Path "." -Filter "*_CFG" | Where-Object { $_.Name -ne $target_cfg_name } | Select-Object -First 1
        if ($null -ne $old_cfg) {
            Copy-Item -Path $old_cfg.FullName -Destination "./$target_cfg_name" -Force
            $msg = "CFG cloned as $target_cfg_name."
            Write-Host $msg -ForegroundColor Green
            $msg | Out-File -FilePath $log_file -Append
        }
    }

    $weaponArg = "./weapon.miz"
    $warehouseArg = "./warehouse.miz"
    if (Test-Path "./$target_cfg_name") {
        $cfgObj = Get-Content -LiteralPath "./$target_cfg_name" -Raw -ErrorAction Stop | ConvertFrom-Json
        $decade = $cfgObj.campaign_decade
        if ([string]::IsNullOrWhiteSpace($decade)) {
            throw "CFG ./$target_cfg_name must set `"campaign_decade`" (Fowl 2.0: weapon<campaign_decade>.miz and warehouse<campaign_decade>.miz)."
        }
        $weaponArg = "./weapon${decade}.miz"
        $warehouseArg = "./warehouse${decade}.miz"
        foreach ($p in @($weaponArg, $warehouseArg)) {
            if (-not (Test-Path -LiteralPath $p)) {
                throw "Missing template $p (campaign_decade=$decade in ./$target_cfg_name)."
            }
        }
    }

    $bftoolsArgs = @(
        "miz",
        "--output", "./$mission_name.miz",
        "--base", "./base.miz",
        "--weapon", $weaponArg,
        "--warehouse", $warehouseArg
    )
    if (Test-Path "./$target_cfg_name") {
        $bftoolsArgs += "--campaign-cfg"
        $bftoolsArgs += "./$target_cfg_name"
        $ccMsg = "Using --campaign-cfg ./$target_cfg_name (decade templates: $weaponArg, $warehouseArg)."
        Write-Host $ccMsg -ForegroundColor Cyan
        $ccMsg | Out-File -FilePath $log_file -Append
    }
    else {
        $noCfg = "No ./$target_cfg_name — bftools runs without --campaign-cfg."
        Write-Host $noCfg -ForegroundColor DarkYellow
        $noCfg | Out-File -FilePath $log_file -Append
    }

    # Capture output first so $LASTEXITCODE reflects bftools (not the pipeline)
    $bftoolsOutput = & $exe_path @bftoolsArgs 2>&1
    $buildSuccess = ($LASTEXITCODE -eq 0)
    $delayedMissingDefaultWarehouseKeys = @()

    foreach ($item in $bftoolsOutput) {
        $line = $item.ToString().TrimEnd()

        if (-not [string]::IsNullOrWhiteSpace($line)) {

            if ($line -match '^BFNEXT_MISSING_DEFAULT_WAREHOUSE_KEYS:(.*)$') {
                $keysCsv = $Matches[1].Trim()
                if (-not [string]::IsNullOrWhiteSpace($keysCsv)) {
                    $delayedMissingDefaultWarehouseKeys = $keysCsv.Split(',') | ForEach-Object { $_.Trim() } | Where-Object { -not [string]::IsNullOrWhiteSpace($_) }
                }
                # Suppress marker line; print the formatted WARNING right after SUCCESS below.
                continue
            }

            $line | Out-File -FilePath $log_file -Append

            $matched = $false
            foreach ($tag in $tagColors.Keys) {
                if ($line -match "(.*)($tag)(.*)") {
                    [Console]::Write($Matches[1])
                    $oldColor = [Console]::ForegroundColor
                    [Console]::ForegroundColor = $tagColors[$tag]
                    [Console]::Write($Matches[2])
                    [Console]::ForegroundColor = $oldColor
                    [Console]::WriteLine($Matches[3])
                    $matched = $true
                    break
                }
            }

            if (-not $matched) { [Console]::WriteLine($line) }
        }
    }

    if (-not $buildSuccess) {
        $err = "ERROR: bftools failed with exit code $LASTEXITCODE"
        Write-Host $err -ForegroundColor Red
        $err | Out-File -FilePath $log_file -Append
    }
    else {
        $succ = "SUCCESS: $mission_name.miz was created."
        Write-Host $succ -ForegroundColor Green
        $succ | Out-File -FilePath $log_file -Append

        if ($delayedMissingDefaultWarehouseKeys.Count -gt 0) {
            $header = "WARNING: missing default_warehouse_* key(s) in CFG JSON:"
            Write-Host $header -ForegroundColor Red
            $header | Out-File -FilePath $log_file -Append
            foreach ($k in $delayedMissingDefaultWarehouseKeys) {
                Write-Host ("  " + $k) -ForegroundColor Yellow
                ("  " + $k) | Out-File -FilePath $log_file -Append
            }
        }
    }

    # Copy to DCS Saved Games only after a successful mission build
    if ($buildSuccess) {
        $cfg_copy_msg = "Copying $target_cfg_name to $DCS_user_path\$target_cfg_name"
        Write-Host $cfg_copy_msg -ForegroundColor Cyan
        $cfg_copy_msg | Out-File -FilePath $log_file -Append
        Copy-Item -Path "$target_cfg_name" -Destination "$DCS_user_path\$target_cfg_name" -Force -ErrorAction Stop

        $miz_copy_msg = "Copying $mission_name.miz to $DCS_user_path\Missions\$mission_name.miz"
        Write-Host $miz_copy_msg -ForegroundColor Cyan
        $miz_copy_msg | Out-File -FilePath $log_file -Append
        Copy-Item -Path "$mission_name.miz" -Destination "$DCS_user_path\Missions\$mission_name.miz" -Force -ErrorAction Stop
    }
    else {
        $skipMsg = "Mission build failed: skipping copy to DCS Saved Games (CFG and .miz were not updated; any existing files there are unchanged)."
        Write-Host $skipMsg -ForegroundColor Yellow
        $skipMsg | Out-File -FilePath $log_file -Append
    }
}
catch {
    $crit = "AN UNEXPECTED ERROR OCCURRED: $_"
    Write-Host $crit -ForegroundColor White -BackgroundColor DarkRed
    $crit | Out-File -FilePath $log_file -Append
}
finally {
    $endMsg = "--- Process Finished: $(Get-Date) ---"
    Write-Host $endMsg
    $endMsg | Out-File -FilePath $log_file -Append

    # Optional: remove Fowl/bflib persisted state only (same folder DCS uses as writedir — no recurse; Tracks etc. untouched)
    # Matches bflib bg::save: <writedir>\<sortie>, <sortie>.tmp, rotated <sortie><unix_timestamp>
    if (Test-Path -LiteralPath $DCS_user_path) {
        Write-Host "`n--- Fowl persisted state scan ($mission_name) ---" -ForegroundColor Cyan
        "--- Fowl persisted state scan ($mission_name) ---" | Out-File -FilePath $log_file -Append

        $root = $DCS_user_path
        $foundPaths = New-Object 'System.Collections.Generic.HashSet[string]' ([StringComparer]::OrdinalIgnoreCase)
        try {
            foreach ($candidate in @(
                    (Join-Path $root $mission_name),
                    (Join-Path $root "$mission_name.tmp")
                )) {
                if (Test-Path -LiteralPath $candidate -PathType Leaf) {
                    [void]$foundPaths.Add([System.IO.Path]::GetFullPath($candidate))
                }
            }
            $backupNameRegex = '^' + [regex]::Escape($mission_name) + '\d+$'
            Get-ChildItem -LiteralPath $root -File -ErrorAction SilentlyContinue |
                Where-Object { $_.Name -match $backupNameRegex } |
                ForEach-Object { [void]$foundPaths.Add([System.IO.Path]::GetFullPath($_.FullName)) }
        }
        catch { }

        $persistFiles = @($foundPaths | Sort-Object | ForEach-Object { Get-Item -LiteralPath $_ -ErrorAction SilentlyContinue } | Where-Object { $null -ne $_ })

        if ($persistFiles.Count -eq 0) {
            $noneMsg = "No Fowl persisted state files found in '$root' (expected: main save file with no extension, '$mission_name.tmp', or '$mission_name<timestamp>' backups). Subfolders (e.g. Tracks) are not scanned."
            Write-Host $noneMsg -ForegroundColor DarkGray
            $noneMsg | Out-File -FilePath $log_file -Append
        }
        else {
            Write-Host "Found $($persistFiles.Count) Fowl state file(s) in DCS user folder root:" -ForegroundColor Yellow
            foreach ($pf in $persistFiles) {
                Write-Host "  $($pf.FullName)"
            }
            "Candidate Fowl state files:" | Out-File -FilePath $log_file -Append
            $persistFiles | ForEach-Object { $_.FullName } | Out-File -FilePath $log_file -Append

            Write-Host "`nPress Y to DELETE these Fowl state files permanently, or any other key to keep them." -ForegroundColor Yellow
            $key = Read-Host "Your choice"
            $logChoice = "User choice for persistence cleanup: $key"
            $logChoice | Out-File -FilePath $log_file -Append

            if (($null -ne $key) -and ($key.Trim().Equals('y', [StringComparison]::OrdinalIgnoreCase))) {
                $deleted = [System.Collections.Generic.List[string]]::new()
                $failed = [System.Collections.Generic.List[string]]::new()
                foreach ($pf in $persistFiles) {
                    try {
                        Remove-Item -LiteralPath $pf.FullName -Force -ErrorAction Stop
                        $deleted.Add($pf.FullName)
                    }
                    catch {
                        $failed.Add("$($pf.FullName) -> $($_.Exception.Message)")
                    }
                }
                Write-Host "`nDeleted ($($deleted.Count)):" -ForegroundColor Green
                foreach ($d in $deleted) {
                    Write-Host "  $d"
                }
                "Deleted Fowl state files:" | Out-File -FilePath $log_file -Append
                $deleted | Out-File -FilePath $log_file -Append
                if ($failed.Count -gt 0) {
                    Write-Host "`nCould not delete ($($failed.Count)):" -ForegroundColor Red
                    foreach ($f in $failed) { Write-Host "  $f" }
                    "Failed to delete:" | Out-File -FilePath $log_file -Append
                    $failed | Out-File -FilePath $log_file -Append
                }
                $doneMsg = "Script finished: Fowl state file cleanup completed (see list above)."
                Write-Host "`n$doneMsg" -ForegroundColor Green
                $doneMsg | Out-File -FilePath $log_file -Append
            }
            else {
                $keepMsg = "Leaving Fowl state files unchanged. Script finished."
                Write-Host "`n$keepMsg" -ForegroundColor Cyan
                $keepMsg | Out-File -FilePath $log_file -Append
            }
        }
    }
    else {
        $skipScan = "DCS user path not found; skipping Fowl state scan: $DCS_user_path"
        Write-Host "`n$skipScan" -ForegroundColor DarkYellow
        $skipScan | Out-File -FilePath $log_file -Append
    }

    Write-Host ""
    Read-Host -Prompt "Press Enter to exit"
}
