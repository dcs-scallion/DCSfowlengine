# WeatherVOTE.ps1
# Simulates bflib setmission neighbor weather votes (vote_neighbor_index).
# Prefer double-click WeatherVOTE.BAT (keeps the window open).
# Or:  powershell -File WeatherVOTE.ps1
# Optional:  -Seed 42   -NoPause

param(
    [Nullable[int]]$Seed = $null,
    [switch]$NoPause
)

# =============================================================================
# CONFIG — edit these
# =============================================================================

# Relative weights for presets 0 .. N-1 (any length >= 1; same order as CFG profiles)
$Weights = @(45, 43, 41, 38, 35, 30)

# Optional labels (same length as Weights). Empty = "preset 0", "preset 1", ...
$Labels = @(
    "light_scattered",
    "scattered",
    "broken",
    "overcast",
    "light_rain",
    "heavy_rain"
)

# Starting profile index (CFG weather_start_index / persisted weather_index)
$StartIndex = 1

# Number of successive votes (mission-end rounds)
$VoteCount = 360

# How many indices per printed sequence line
$SequenceRowLength = 40

# Console colors per preset index (good -> bad). Extra presets cycle this list.
# Valid: Black, DarkBlue, DarkGreen, DarkCyan, DarkRed, DarkMagenta, DarkYellow,
#        Gray, DarkGray, Blue, Green, Cyan, Red, Magenta, Yellow, White
$Colors = @(
    "Cyan",        # 0 clear / light scattered
    "Green",       # 1 scattered
    "Yellow",      # 2 broken
    "DarkYellow",  # 3 overcast
    "Blue",        # 4 light rain
    "Red"          # 5 heavy rain
)

# =============================================================================
# Engine (matches bflib/src/setmission.rs vote_neighbor_index)
# =============================================================================

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

function Assert-Config {
    if ($Weights.Count -lt 1) {
        throw "Weights must contain at least one value."
    }
    foreach ($w in $Weights) {
        if ($null -eq $w -or [double]$w -lt 0) {
            throw "Each weight must be >= 0 (got: $w)."
        }
    }
    if (($Weights | Measure-Object -Sum).Sum -le 0) {
        throw "Sum of Weights must be > 0."
    }
    if ($StartIndex -lt 0 -or $StartIndex -ge $Weights.Count) {
        throw "StartIndex $StartIndex out of range 0..$($Weights.Count - 1)."
    }
    if ($VoteCount -lt 1) {
        throw "VoteCount must be >= 1."
    }
    if ($Labels.Count -gt 0 -and $Labels.Count -ne $Weights.Count) {
        throw "Labels length ($($Labels.Count)) must match Weights ($($Weights.Count)), or leave Labels empty."
    }
    if ($SequenceRowLength -lt 1) {
        throw "SequenceRowLength must be >= 1."
    }
    if ($Colors.Count -lt 1) {
        throw "Colors must contain at least one ConsoleColor name."
    }
    foreach ($c in $Colors) {
        try {
            [void][Enum]::Parse([type][ConsoleColor], [string]$c, $true)
        } catch {
            throw "Invalid color '$c'. Use a System.ConsoleColor name."
        }
    }
}

function Get-Label([int]$Index) {
    if ($Labels.Count -eq $Weights.Count) {
        return [string]$Labels[$Index]
    }
    return "preset $Index"
}

function Get-IndexColor([int]$Index) {
    $name = [string]$Colors[$Index % $Colors.Count]
    return [Enum]::Parse([type][ConsoleColor], $name, $true)
}

function Write-IndexText {
    param(
        [int]$Index,
        [string]$Text,
        [switch]$NoNewline
    )
    $color = Get-IndexColor $Index
    if ($NoNewline) {
        Write-Host -NoNewline $Text -ForegroundColor $color
    } else {
        Write-Host $Text -ForegroundColor $color
    }
}

function Invoke-NeighborVote {
    param(
        [int]$Current,
        [double[]]$W,
        [System.Random]$Rng
    )
    $n = $W.Count
    $cands = [System.Collections.Generic.List[object]]::new()
    [void]$cands.Add([pscustomobject]@{ Idx = $Current; W = [double]$W[$Current] })
    if ($Current -gt 0) {
        [void]$cands.Add([pscustomobject]@{ Idx = ($Current - 1); W = [double]$W[$Current - 1] })
    }
    if (($Current + 1) -lt $n) {
        [void]$cands.Add([pscustomobject]@{ Idx = ($Current + 1); W = [double]$W[$Current + 1] })
    }
    $total = 0.0
    foreach ($c in $cands) { $total += $c.W }
    if ($total -le 0.0) { return $Current }

    # Same as Rust: pick in [0, total)
    $pick = $Rng.NextDouble() * $total
    foreach ($c in $cands) {
        if ($pick -lt $c.W) { return [int]$c.Idx }
        $pick -= $c.W
    }
    return $Current
}

function Wait-IfNeeded {
    if (-not $NoPause) {
        Write-Host ""
        Write-Host "Press Enter to close..."
        try {
            [void][System.Console]::ReadLine()
        } catch {
            pause
        }
    }
}

try {
Assert-Config

if ($null -ne $Seed) {
    $rng = [System.Random]::new([int]$Seed)
    $seedInfo = "seed=$Seed"
} else {
    $rng = [System.Random]::new()
    $seedInfo = "seed=random"
}

$n = $Weights.Count
$counts = @(0) * $n
$trans = @{}
for ($i = 0; $i -lt $n; $i++) {
    for ($j = 0; $j -lt $n; $j++) {
        if ([Math]::Abs($i - $j) -le 1) {
            $trans["$i->$j"] = 0
        }
    }
}

$cur = [int]$StartIndex
$chosen = New-Object System.Collections.Generic.List[int]
$stays = 0

for ($v = 0; $v -lt $VoteCount; $v++) {
    $nxt = Invoke-NeighborVote -Current $cur -W $Weights -Rng $rng
    $trans["$cur->$nxt"]++
    if ($nxt -eq $cur) { $stays++ }
    $cur = $nxt
    [void]$chosen.Add($cur)
    $counts[$cur]++
}

$changes = $VoteCount - $stays
$minIdx = ($chosen | Measure-Object -Minimum).Minimum
$maxIdx = ($chosen | Measure-Object -Maximum).Maximum

Write-Host ""
Write-Host "WeatherVOTE  ($seedInfo)"
Write-Host -NoNewline "weights = "
for ($i = 0; $i -lt $n; $i++) {
    if ($i -gt 0) { Write-Host -NoNewline " / " }
    Write-IndexText -Index $i -Text ([string]$Weights[$i]) -NoNewline
}
Write-Host ""
Write-Host -NoNewline "start_index = "
Write-IndexText -Index $StartIndex -Text ("{0} ({1})" -f $StartIndex, (Get-Label $StartIndex)) -NoNewline
Write-Host "   votes = $VoteCount"
Write-Host ""
Write-Host -NoNewline "legend: "
for ($i = 0; $i -lt $n; $i++) {
    if ($i -gt 0) { Write-Host -NoNewline "  " }
    Write-IndexText -Index $i -Text ("{0}:{1}" -f $i, (Get-Label $i)) -NoNewline
}
Write-Host ""
Write-Host ""

Write-Host "=== Frequency after $VoteCount votes ==="
Write-Host ("{0,-6} {1,-22} {2,6} {3,8}" -f "Index", "Profile", "Count", "Share")
Write-Host ("-" * 48)
for ($i = 0; $i -lt $n; $i++) {
    $share = 100.0 * $counts[$i] / $VoteCount
    $line = "{0,-6} {1,-22} {2,6} {3,7:N1} %" -f $i, (Get-Label $i), $counts[$i], $share
    Write-IndexText -Index $i -Text $line
}

Write-Host ""
Write-Host "=== Transitions (from -> to : count) ==="
for ($i = 0; $i -lt $n; $i++) {
    Write-Host -NoNewline "  from "
    Write-IndexText -Index $i -Text ([string]$i) -NoNewline
    Write-Host -NoNewline ":  "
    $js = @()
    if ($i -gt 0) { $js += ($i - 1) }
    $js += $i
    if (($i + 1) -lt $n) { $js += ($i + 1) }
    $first = $true
    foreach ($j in $js) {
        if (-not $first) { Write-Host -NoNewline "  " }
        $first = $false
        $key = "$i->$j"
        Write-IndexText -Index $j -Text ("{0}:{1}" -f $j, $trans[$key]) -NoNewline
    }
    Write-Host ""
}

Write-Host ""
Write-Host "=== Sequence (index after each vote) ==="
$k = 0
foreach ($idx in $chosen) {
    if ($k -gt 0 -and (($k % $SequenceRowLength) -ne 0)) {
        Write-Host -NoNewline " "
    }
    if ($k -gt 0 -and (($k % $SequenceRowLength) -eq 0)) {
        Write-Host ""
    }
    Write-IndexText -Index $idx -Text ([string]$idx) -NoNewline
    $k++
}
Write-Host ""

$final = [int]$chosen[$chosen.Count - 1]
Write-Host ""
Write-Host ("stay={0} ({1:N1} %)  change={2} ({3:N1} %)" -f `
    $stays, (100.0 * $stays / $VoteCount), `
    $changes, (100.0 * $changes / $VoteCount))
Write-Host -NoNewline "final index: "
Write-IndexText -Index $final -Text ("{0} ({1})" -f $final, (Get-Label $final)) -NoNewline
Write-Host ""
Write-Host -NoNewline "max index reached: "
Write-IndexText -Index $maxIdx -Text ([string]$maxIdx) -NoNewline
Write-Host -NoNewline "   min: "
Write-IndexText -Index $minIdx -Text ([string]$minIdx) -NoNewline
Write-Host -NoNewline ("   visits to last preset ({0}): " -f ($n - 1))
Write-IndexText -Index ($n - 1) -Text ([string]$counts[$n - 1]) -NoNewline
Write-Host ""
Write-Host ""
}
catch {
    Write-Host ""
    Write-Host "ERROR: $($_.Exception.Message)" -ForegroundColor Red
    Write-Host ""
    exit 1
}
finally {
    Wait-IfNeeded
}
