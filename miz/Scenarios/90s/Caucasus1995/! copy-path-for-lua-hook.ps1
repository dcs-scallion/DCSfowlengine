$ErrorActionPreference = "Stop"

function Get-QuotedValue {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Text,
        [Parameter(Mandatory = $true)]
        [string]$VarName
    )
    $pattern = '^\s*\$' + [Regex]::Escape($VarName) + '\s*=\s*"([^"]*)"\s*$'
    $match = [Regex]::Match($Text, $pattern, [System.Text.RegularExpressions.RegexOptions]::Multiline)
    if (-not $match.Success) {
        throw "Variable `$$VarName was not found in - EDIT-FILE-LOCATIONS.txt"
    }
    return $match.Groups[1].Value
}

function To-LuaBracketPath {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Path
    )
    $normalized = $Path -replace '/', '\'
    return '[[' + $normalized + ']]'
}

try {
    $scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
    $locationsPath = Join-Path $scriptDir "- EDIT-FILE-LOCATIONS.txt"

    if (-not (Test-Path -LiteralPath $locationsPath)) {
        throw "Missing file: $locationsPath"
    }

    $locations = Get-Content -LiteralPath $locationsPath -Raw
    $workPathEngine = Get-QuotedValue -Text $locations -VarName "work_path_engine"
    $pathEngineMission = Get-QuotedValue -Text $locations -VarName "path_engine_mission"
    $dcsUserPath = Get-QuotedValue -Text $locations -VarName "DCS_user_path"

    $missionPath = Join-Path $workPathEngine ($pathEngineMission.TrimStart('\'))
    $missionPath = [System.IO.Path]::GetFullPath($missionPath)
    $luaOutputDir = To-LuaBracketPath -Path $missionPath

    $universalHookPath = Join-Path $scriptDir "Fowl_engine_export.lua"
    $hookFiles = @($universalHookPath)

    if (-not (Test-Path -LiteralPath $universalHookPath)) {
        throw "Missing universal hook file: $universalHookPath"
    }
    $content = Get-Content -LiteralPath $universalHookPath -Raw
    $pattern = '(?m)^\s*local OUTPUT_DIR = .*$'
    if (-not [Regex]::IsMatch($content, $pattern)) {
        throw "Could not locate OUTPUT_DIR line in $universalHookPath"
    }
    $updated = [Regex]::Replace(
        $content,
        $pattern,
        "local OUTPUT_DIR = $luaOutputDir",
        1
    )
    Set-Content -LiteralPath $universalHookPath -Value $updated -NoNewline

    Write-Host "Lua hooks were updated with paths from - EDIT-FILE-LOCATIONS.txt"
    Write-Host "OUTPUT_DIR path:" -ForegroundColor Yellow
    Write-Host "  $missionPath" -ForegroundColor Yellow
    Write-Host "Updated Lua hooks:" -ForegroundColor Green
    foreach ($file in $hookFiles) {
        Write-Host "  $(Split-Path -Leaf $file)" -ForegroundColor Green
    }

    Write-Host "Press Y to copy Lua hooks to DCS Scripts\Hooks now, or any other key to exit" -ForegroundColor Cyan
    $choice = Read-Host "Your choice"
    if ($choice -notmatch '^[Yy]$') {
        Write-Host "No files were copied."
    }
    else {
        $dcsHooksPath = Join-Path $dcsUserPath "Scripts\Hooks"
        if (-not (Test-Path -LiteralPath $dcsHooksPath)) {
            New-Item -ItemType Directory -Path $dcsHooksPath -Force | Out-Null
        }

        Copy-Item -LiteralPath $universalHookPath -Destination (Join-Path $dcsHooksPath "Fowl_engine_export.lua") -Force

        Write-Host "Universal Lua hook copied to $dcsHooksPath"
        Write-Host ""
        Write-Host "Next steps (export runs only after you enter the 3D world):" -ForegroundColor Yellow
        Write-Host "  1. If DCS was already running, exit fully and restart it (hooks load at DCS startup)."
        Write-Host "  2. Start any mission in MULTIPLAYER or single player."
        Write-Host "  3. Enter the 3D world and slot/join into a flyable aircraft (observer may not expose the mission state in time)."
        Write-Host "  4. Status lines are sent to in-game CHAT (message list), e.g. ""[All] admin: [FOWL EXPORT] ..."" — open the chat panel if you do not see them. They are also appended to:"
        Write-Host "      Saved Games\DCS\Logs\fowl_engine_export.log"
        Write-Host "     (HUD trigger outText from hooks is unreliable; do not rely on the top-right message area.)"
        Write-Host "  5. After fowl_weapon_bridge*.json appears in OUTPUT_DIR, remove Fowl_engine_export.lua from Scripts\Hooks."
        Write-Host "     Airbase warehouse IDs: use bflib admin airbaseexport (writes JSON next to the mission CFG in Saved Games)."
    }
}
catch {
    Write-Host "ERROR: $($_.Exception.Message)" -ForegroundColor Red
}
finally {
    Write-Host ""
    [void](Read-Host "Press Enter to close")
}
