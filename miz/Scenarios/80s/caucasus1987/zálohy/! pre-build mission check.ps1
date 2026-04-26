#############################
#  pre-build mission check  #
#############################



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

$log_file = Join-Path -Path $PSScriptRoot -ChildPath "! pre-build mission check-LOG.txt"

# Remove previous log
if (Test-Path $log_file) { Remove-Item $log_file -Force }

# Skip Start-Transcript (it duplicates lines); log manually instead.

try {
    Write-Host "--- Process Started: $(Get-Date) ---" -ForegroundColor Cyan
    "--- Process Started: $(Get-Date) ---" | Out-File -FilePath $log_file -Append

    foreach ($name in @("DCS_user_path", "mission_name", "target_cfg_name")) {
        $v = (Get-Variable -Name $name -ErrorAction SilentlyContinue).Value
        if ([string]::IsNullOrWhiteSpace($v)) {
            throw "Configuration variable '$name' is missing or empty in '- EDIT-FILE-LOCATIONS.txt'."
        }
    }

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

        $nextStep = "Mission is buildable. To assemble and copy mission files, run '! build-and-copy-mission.ps1'."
        Write-Host $nextStep -ForegroundColor Cyan
        $nextStep | Out-File -FilePath $log_file -Append
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

    Write-Host ""
    Read-Host -Prompt "Press Enter to exit"
}
