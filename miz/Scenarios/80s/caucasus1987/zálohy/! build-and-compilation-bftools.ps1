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
Start-Transcript -Path $LogFile -Append

try {
    Write-Host "--- Process started: $(Get-Date) ---" -ForegroundColor Cyan

    foreach ($name in @("work_path_engine", "bftools")) {
        $v = (Get-Variable -Name $name -ErrorAction SilentlyContinue).Value
        if ([string]::IsNullOrWhiteSpace($v)) {
            throw "Configuration variable '$name' is missing or empty in '- EDIT-FILE-LOCATIONS.txt'."
        }
    }

    $bftoolsDir = Join-Path $work_path_engine ($bftools.TrimStart('\'))
    Set-Location -Path $bftoolsDir -ErrorAction Stop

    Write-Host "Running cargo clean..."
    cargo clean
    if ($LASTEXITCODE -ne 0) {
        Write-Host "WARNING: cargo clean failed (files under target\ may be locked). Continuing with release build without clean." -ForegroundColor Yellow
    }

    Write-Host "Running cargo build --release..."
    cargo build --release
    $buildSuccess = ($LASTEXITCODE -eq 0)

    if ($buildSuccess) {
        $exeSrc = Join-Path $bftoolsDir "target\release\bftools.exe"
        Write-Host "Copying bftools.exe to: $bftoolsDir"
        Copy-Item -Path $exeSrc -Destination $bftoolsDir -Force -ErrorAction Stop
        Write-Host "`nBuild and copy completed successfully." -ForegroundColor Green
    }
    else {
        Write-Host "`nBuild failed. Check errors above." -ForegroundColor Red
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
