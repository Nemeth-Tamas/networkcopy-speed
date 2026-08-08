[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$Source,

    [ValidateRange(1, 100)]
    [int]$Repeats = 20,

    [ValidateRange(1, 1024)]
    [int]$ChunkMiB = 4,

    [ValidateRange(10, 3600)]
    [int]$TimeoutSeconds = 300
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"


function Find-RepoRoot {
    $current = if ($PSScriptRoot) {
        Split-Path -Parent $PSScriptRoot
    }
    else {
        (Get-Location).Path
    }

    while ($current) {
        if (Test-Path (Join-Path $current "Cargo.toml")) {
            return $current
        }

        $parent = Split-Path -Parent $current

        if ($parent -eq $current) {
            break
        }

        $current = $parent
    }

    throw "Could not find repository root."
}


function Measure-Stats {
    param(
        [double[]]$Values
    )

    $mean = (
        $Values |
        Measure-Object -Average
    ).Average

    $min = (
        $Values |
        Measure-Object -Minimum
    ).Minimum

    $max = (
        $Values |
        Measure-Object -Maximum
    ).Maximum

    $sumSquared = 0.0

    foreach ($value in $Values) {
        $delta = $value - $mean
        $sumSquared += $delta * $delta
    }

    $stddev = if ($Values.Count -gt 1) {
        [Math]::Sqrt(
            $sumSquared / ($Values.Count - 1)
        )
    }
    else {
        0.0
    }

    $cv = if ($mean -eq 0.0) {
        0.0
    }
    else {
        $stddev / $mean * 100.0
    }

    [pscustomobject]@{
        Mean = $mean
        StdDev = $stddev
        Cv = $cv
        Min = $min
        Max = $max
    }
}


function Invoke-ReadBench {
    param(
        [string]$Exe,
        [string]$Mode,
        [string]$SourcePath,
        [int]$Chunk
    )

    if ($Mode -eq "blocking") {
        $arguments = @(
            "bench-blocking-read",
            $SourcePath,
            "$Chunk"
        )
    }
    elseif ($Mode -eq "iocp") {
        $arguments = @(
            "bench-iocp-read-ahead",
            $SourcePath,
            "$Chunk",
            "1"
        )
    }
    else {
        throw "Unknown mode: $Mode"
    }

    $output = & $Exe @arguments 2>&1

    if ($LASTEXITCODE -ne 0) {
        throw @"
$Mode benchmark failed:

$($output -join "`n")
"@
    }

    $text = $output -join "`n"

    $match = [regex]::Match(
        $text,
        "(?m)^\s*Read throughput:\s*([0-9.]+)\s+MB/s"
    )

    if (-not $match.Success) {
        throw @"
Could not parse $Mode benchmark:

$text
"@
    }

    [double]::Parse(
        $match.Groups[1].Value,
        [Globalization.CultureInfo]::InvariantCulture
    )
}


$repoRoot = Find-RepoRoot

$exe = Join-Path `
    $repoRoot `
    "target\release\networkcopy-speed.exe"

$sourceCandidate = if (
    [IO.Path]::IsPathRooted($Source)
) {
    $Source
}
else {
    Join-Path $repoRoot $Source
}

$sourcePath = (
    Resolve-Path `
        -LiteralPath $sourceCandidate
).Path


Push-Location $repoRoot

try {
    Write-Host "Building release executable..."

    & cargo build --release

    if ($LASTEXITCODE -ne 0) {
        throw "cargo build --release failed."
    }
}
finally {
    Pop-Location
}


$timestamp = Get-Date -Format "yyyyMMdd-HHmmss"

$resultRoot = Join-Path `
    $repoRoot `
    "bench-results\source-read-modes-$timestamp"

New-Item `
    -ItemType Directory `
    -Path $resultRoot `
    -Force |
    Out-Null

$rawCsv = Join-Path $resultRoot "raw-results.csv"
$summaryCsv = Join-Path $resultRoot "summary.csv"


# Prime the source before measured A/B runs.
Write-Host
Write-Host "Warming source file..."

$null = Invoke-ReadBench `
    -Exe $exe `
    -Mode "blocking" `
    -SourcePath $sourcePath `
    -Chunk $ChunkMiB

$null = Invoke-ReadBench `
    -Exe $exe `
    -Mode "iocp" `
    -SourcePath $sourcePath `
    -Chunk $ChunkMiB


$runs = @()

for ($repeat = 1; $repeat -le $Repeats; $repeat++) {
    $modes = @(
        "blocking",
        "iocp"
    ) |
        Sort-Object {
            Get-Random
        }

    foreach ($mode in $modes) {
        Write-Host (
            "Repeat {0}/{1}: {2}" -f
            $repeat,
            $Repeats,
            $mode
        )

        $throughput = Invoke-ReadBench `
            -Exe $exe `
            -Mode $mode `
            -SourcePath $sourcePath `
            -Chunk $ChunkMiB

        $runs += [pscustomobject]@{
            Repeat = $repeat
            Mode = $mode
            ChunkMiB = $ChunkMiB
            MBps = $throughput
        }

        $runs |
            Export-Csv `
                -LiteralPath $rawCsv `
                -NoTypeInformation

        Write-Host (
            "  {0:N2} MB/s" -f
            $throughput
        )
    }
}


$summary = foreach (
    $group in (
        $runs |
        Group-Object Mode
    )
) {
    $stats = Measure-Stats @(
        $group.Group |
        ForEach-Object {
            [double]$_.MBps
        }
    )

    [pscustomobject]@{
        Mode = $group.Name
        Runs = $group.Count

        MeanMBps = [Math]::Round(
            $stats.Mean,
            2
        )

        StdDevMBps = [Math]::Round(
            $stats.StdDev,
            2
        )

        CvPercent = [Math]::Round(
            $stats.Cv,
            2
        )

        MinMBps = [Math]::Round(
            $stats.Min,
            2
        )

        MaxMBps = [Math]::Round(
            $stats.Max,
            2
        )
    }
}


$summary |
    Export-Csv `
        -LiteralPath $summaryCsv `
        -NoTypeInformation


Write-Host
Write-Host "============================================"
Write-Host "Source read A/B complete"
Write-Host "============================================"
Write-Host

$summary |
    Sort-Object MeanMBps -Descending |
    Format-Table -AutoSize


$blocking = $summary |
    Where-Object Mode -eq "blocking"

$iocp = $summary |
    Where-Object Mode -eq "iocp"

if ($blocking -and $iocp) {
    $advantage = (
        (
            $iocp.MeanMBps /
            $blocking.MeanMBps
        ) - 1.0
    ) * 100.0

    Write-Host
    Write-Host (
        "IOCP mean advantage: {0:N2}%" -f
        $advantage
    )
}


Write-Host
Write-Host "Raw:"
Write-Host "  $rawCsv"
Write-Host
Write-Host "Summary:"
Write-Host "  $summaryCsv"