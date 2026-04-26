param(
    # Full `cargo clean` before release build can hit locked files or stress link; skip for faster iteration.
    [switch]$SkipClean
)

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


##############################
# Build and copy bftools.exe #
##############################

$LogFile = Join-Path -Path $PSScriptRoot -ChildPath "! build-and-compilation-bftools-LOG.txt"
$CargoLogFile = Join-Path -Path $PSScriptRoot -ChildPath "! build-and-compilation-bftools-CARGO-LOG.txt"
Start-Transcript -Path $LogFile -Append

try {
    Write-Host "--- Process started: $(Get-Date) ---" -ForegroundColor Cyan

    foreach ($name in @("work_path_engine", "bftools")) {
        $v = (Get-Variable -Name $name -ErrorAction SilentlyContinue).Value
        if ([string]::IsNullOrWhiteSpace($v)) {
            throw "Configuration variable '$name' is missing or empty in '- EDIT-FILE-LOCATIONS.txt'."
        }
    }

    $bftoolsRel = if ($null -eq $bftools) { '' } else { $bftools.TrimStart('\') }
    $bftoolsDir = Join-Path $work_path_engine $bftoolsRel
    Set-Location -Path $bftoolsDir -ErrorAction Stop

    @(
        "# bftools cargo log (stdout+stderr, not truncated like transcript)"
        "# started: $(Get-Date -Format o)"
        "# bftoolsDir: $bftoolsDir"
        ""
    ) | Set-Content -Path $CargoLogFile -Encoding utf8

    if ($SkipClean) {
        Write-Host "Skipping cargo clean (-SkipClean)." -ForegroundColor Yellow
    }
    else {
        Write-Host "Running cargo clean..."
        # Cargo writes progress to stderr; `2>&1` yields ErrorRecord in PS7 and paints the whole stream red in the host.
        cargo clean 2>&1 | ForEach-Object { "$_" } | Tee-Object -FilePath $CargoLogFile -Append -Encoding utf8
        if ($LASTEXITCODE -ne 0) {
            Write-Host "WARNING: cargo clean failed (files under target\ may be locked). Continuing with release build without clean." -ForegroundColor Yellow
        }
    }

    "`n===== cargo build --release $(Get-Date -Format o) =====`n" | Out-File -FilePath $CargoLogFile -Append -Encoding utf8
    Write-Host "Running cargo build --release (full compiler/link output: $CargoLogFile)..."
    cargo build --release 2>&1 | ForEach-Object { "$_" } | Tee-Object -FilePath $CargoLogFile -Append -Encoding utf8
    $buildSuccess = ($LASTEXITCODE -eq 0)

    if ($buildSuccess) {
        $exeSrc = Join-Path $bftoolsDir "target\release\bftools.exe"
        Write-Host "Copying bftools.exe to: $bftoolsDir"
        Copy-Item -Path $exeSrc -Destination $bftoolsDir -Force -ErrorAction Stop
        Write-Host "`nBuild and copy completed successfully." -ForegroundColor Green
    }
    else {
        Write-Host "`nBuild failed." -ForegroundColor Red
        Write-Host "Transcript lines can be truncated (long linker commands). Full cargo output:" -ForegroundColor Yellow
        Write-Host "  $CargoLogFile" -ForegroundColor Yellow
        Write-Host "(Open that file and search for error / LNK / failed.)" -ForegroundColor Yellow
        $skip = "Build failed: skipping copy of bftools.exe (repo bftools folder was not updated)."
        Write-Host $skip -ForegroundColor Yellow
    }
}
catch {
    Write-Host "`nERROR: $($_.Exception.Message)" -ForegroundColor Red
    Write-Host "Script stopped early." -ForegroundColor Red
    exit 1
}
finally {
    Write-Host "`n--- Process finished: $(Get-Date) ---"
    Stop-Transcript
    Read-Host "Press Enter to close"
}
