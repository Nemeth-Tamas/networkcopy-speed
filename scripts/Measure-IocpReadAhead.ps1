[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$Source,

    [ValidateRange(1, 100)]
    [int]$Repeats = 10,

    [int[]]$ChunkMiB = @(8),

    [int[]]$Operations = @(1, 2, 4, 8, 16),

    [ValidateRange(0, 20)]
    [int]$WarmupRuns = 2,

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

    throw "Could not find repository root containing Cargo.toml."
}


function Start-CapturedProcess {
    param(
        [string]$FilePath,
        [string[]]$Arguments,
        [string]$StdOutPath,
        [string]$StdErrPath,
        [string]$WorkingDirectory
    )

    Remove-Item `
        -LiteralPath $StdOutPath `
        -Force `
        -ErrorAction SilentlyContinue

    Remove-Item `
        -LiteralPath $StdErrPath `
        -Force `
        -ErrorAction SilentlyContinue

    Start-Process `
        -FilePath $FilePath `
        -ArgumentList $Arguments `
        -RedirectStandardOutput $StdOutPath `
        -RedirectStandardError $StdErrPath `
        -WorkingDirectory $WorkingDirectory `
        -NoNewWindow `
        -PassThru
}


function Wait-ProcessWithTimeout {
    param(
        [System.Diagnostics.Process]$Process,
        [int]$TimeoutSeconds,
        [string]$Description
    )

    $milliseconds = [Math]::Min(
        [int]::MaxValue,
        $TimeoutSeconds * 1000
    )

    if (-not $Process.WaitForExit($milliseconds)) {
        try {
            $Process.Kill()
        }
        catch {
        }

        throw "$Description timed out after $TimeoutSeconds seconds."
    }

    # Make sure redirected stdout/stderr have completely flushed.
    $Process.WaitForExit()
}


function Read-ProcessOutput {
    param(
        [string]$StdOutPath,
        [string]$StdErrPath
    )

    $stdout = if (Test-Path $StdOutPath) {
        Get-Content `
            -LiteralPath $StdOutPath `
            -Raw `
            -ErrorAction SilentlyContinue
    }
    else {
        ""
    }

    $stderr = if (Test-Path $StdErrPath) {
        Get-Content `
            -LiteralPath $StdErrPath `
            -Raw `
            -ErrorAction SilentlyContinue
    }
    else {
        ""
    }

    @{
        StdOut = $stdout
        StdErr = $stderr
    }
}


function Parse-IocpReadAheadOutput {
    param(
        [string]$Text
    )

    $bytesRead = [regex]::Match(
        $Text,
        "(?m)^\s*Bytes read:\s*([0-9,]+)\s*$"
    )

    $chunkSize = [regex]::Match(
        $Text,
        "(?m)^\s*Chunk size:\s*([0-9]+)\s+MiB\s*$"
    )

    $operations = [regex]::Match(
        $Text,
        "(?m)^\s*Operations:\s*([0-9]+)\s*$"
    )

    $poolSize = [regex]::Match(
        $Text,
        "(?m)^\s*Buffer pool:\s*([0-9]+)\s+MiB\s*$"
    )

    $submissions = [regex]::Match(
        $Text,
        "(?m)^\s*Read submissions:\s*([0-9,]+)\s*$"
    )

    $immediateReads = [regex]::Match(
        $Text,
        "(?m)^\s*Immediate reads:\s*([0-9,]+)\s*$"
    )

    $readTime = [regex]::Match(
        $Text,
        "(?m)^\s*Read time:\s*([0-9.]+)\s+s\s*$"
    )

    $totalTime = [regex]::Match(
        $Text,
        "(?m)^\s*Total time:\s*([0-9.]+)\s+s\s*$"
    )

    $throughput = [regex]::Match(
        $Text,
        "(?m)^\s*Read throughput:\s*([0-9.]+)\s+MB/s\s+\(([0-9.]+)\s+MiB/s\)\s*$"
    )

    foreach ($match in @(
        $bytesRead,
        $chunkSize,
        $operations,
        $poolSize,
        $submissions,
        $immediateReads,
        $readTime,
        $totalTime,
        $throughput
    )) {
        if (-not $match.Success) {
            throw @"
Could not parse IOCP read-ahead output:

$Text
"@
        }
    }

    $culture = [Globalization.CultureInfo]::InvariantCulture

    [pscustomobject]@{
        BytesRead = [Convert]::ToUInt64(
            $bytesRead.Groups[1].Value.Replace(",", "")
        )

        ChunkMiB = [int]$chunkSize.Groups[1].Value

        Operations = [int]$operations.Groups[1].Value

        PoolMiB = [int]$poolSize.Groups[1].Value

        ReadSubmissions = [Convert]::ToUInt64(
            $submissions.Groups[1].Value.Replace(",", "")
        )

        ImmediateReads = [Convert]::ToUInt64(
            $immediateReads.Groups[1].Value.Replace(",", "")
        )

        ReadSeconds = [double]::Parse(
            $readTime.Groups[1].Value,
            $culture
        )

        TotalSeconds = [double]::Parse(
            $totalTime.Groups[1].Value,
            $culture
        )

        MBps = [double]::Parse(
            $throughput.Groups[1].Value,
            $culture
        )

        MiBps = [double]::Parse(
            $throughput.Groups[2].Value,
            $culture
        )
    }
}


function Measure-Values {
    param(
        [double[]]$Values
    )

    if ($Values.Count -eq 0) {
        throw "Cannot calculate statistics for an empty set."
    }

    $average = (
        $Values |
        Measure-Object -Average
    ).Average

    $minimum = (
        $Values |
        Measure-Object -Minimum
    ).Minimum

    $maximum = (
        $Values |
        Measure-Object -Maximum
    ).Maximum

    $variance = 0.0

    if ($Values.Count -gt 1) {
        foreach ($value in $Values) {
            $difference = $value - $average

            $variance +=
                $difference * $difference
        }

        $variance /= ($Values.Count - 1)
    }

    $standardDeviation = [Math]::Sqrt($variance)

    $cvPercent = if ($average -eq 0.0) {
        0.0
    }
    else {
        $standardDeviation / $average * 100.0
    }

    [pscustomobject]@{
        Average = $average
        StdDev = $standardDeviation
        CvPercent = $cvPercent
        Minimum = $minimum
        Maximum = $maximum
    }
}


function Invoke-ReadAhead {
    param(
        [string]$Exe,
        [string]$RepoRoot,
        [string]$SourcePath,
        [int]$Chunk,
        [int]$OperationCount,
        [string]$StdOutPath,
        [string]$StdErrPath,
        [int]$Timeout
    )

    $process = $null

    try {
        $process = Start-CapturedProcess `
            -FilePath $Exe `
            -Arguments @(
                "bench-iocp-read-ahead",
                $SourcePath,
                "$Chunk",
                "$OperationCount"
            ) `
            -StdOutPath $StdOutPath `
            -StdErrPath $StdErrPath `
            -WorkingDirectory $RepoRoot

        Wait-ProcessWithTimeout `
            -Process $process `
            -TimeoutSeconds $Timeout `
            -Description "IOCP read-ahead benchmark"

        $output = Read-ProcessOutput `
            -StdOutPath $StdOutPath `
            -StdErrPath $StdErrPath

        if ($process.ExitCode -ne 0) {
            throw @"
IOCP read-ahead benchmark failed with exit code $($process.ExitCode).

STDOUT:
$($output.StdOut)

STDERR:
$($output.StdErr)
"@
        }

        Parse-IocpReadAheadOutput `
            -Text $output.StdOut
    }
    finally {
        if ($process -and -not $process.HasExited) {
            try {
                $process.Kill()
            }
            catch {
            }
        }
    }
}


$repoRoot = Find-RepoRoot

$exe = Join-Path `
    $repoRoot `
    "target\release\networkcopy-speed.exe"


if ([IO.Path]::IsPathRooted($Source)) {
    $sourceCandidate = $Source
}
else {
    $sourceCandidate = Join-Path `
        $repoRoot `
        $Source
}


$resolvedSource = Resolve-Path `
    -LiteralPath $sourceCandidate `
    -ErrorAction Stop

$sourcePath = $resolvedSource.Path

if (-not (Test-Path -LiteralPath $sourcePath -PathType Leaf)) {
    throw "Source is not a regular file: $sourcePath"
}


if ($ChunkMiB.Count -eq 0) {
    throw "At least one chunk size is required."
}

if ($Operations.Count -eq 0) {
    throw "At least one operation count is required."
}


foreach ($chunk in $ChunkMiB) {
    if ($chunk -lt 1) {
        throw "Chunk sizes must be at least 1 MiB."
    }

    foreach ($operationCount in $Operations) {
        if ($operationCount -lt 1 -or $operationCount -gt 256) {
            throw "Operation counts must be between 1 and 256."
        }

        $poolMiB = $chunk * $operationCount

        if ($poolMiB -gt 4096) {
            throw @"
Invalid configuration:
  Chunk:      $chunk MiB
  Operations: $operationCount
  Pool:       $poolMiB MiB

The bounded IOCP pool must not exceed 4096 MiB.
"@
        }
    }
}


Push-Location $repoRoot

try {
    Write-Host "Building release benchmark executable..."

    & cargo build --release

    if ($LASTEXITCODE -ne 0) {
        throw "cargo build --release failed."
    }
}
finally {
    Pop-Location
}


if (-not (Test-Path -LiteralPath $exe)) {
    throw "Release executable was not produced: $exe"
}


$timestamp = Get-Date -Format "yyyyMMdd-HHmmss"

$resultRoot = Join-Path `
    $repoRoot `
    "bench-results\iocp-read-ahead-$timestamp"

New-Item `
    -ItemType Directory `
    -Path $resultRoot `
    -Force |
    Out-Null


$rawCsv = Join-Path `
    $resultRoot `
    "raw-results.csv"

$summaryCsv = Join-Path `
    $resultRoot `
    "summary.csv"


$configurations = @()

foreach ($chunk in $ChunkMiB) {
    foreach ($operationCount in $Operations) {
        $configurations += [pscustomobject]@{
            ChunkMiB = $chunk

            Operations = $operationCount

            PoolMiB = $chunk * $operationCount

            Name = (
                "chunk-{0}MiB__ops-{1}" -f
                $chunk,
                $operationCount
            )
        }
    }
}


Write-Host
Write-Host "NetworkCopy IOCP read-ahead benchmark"
Write-Host "  EXE:            $exe"
Write-Host "  Source:         $sourcePath"
Write-Host "  Source size:    $((Get-Item $sourcePath).Length) bytes"
Write-Host "  Repeats/config: $Repeats"
Write-Host "  Configurations: $($configurations.Count)"
Write-Host "  Warm-up runs:   $WarmupRuns"
Write-Host "  Results:        $resultRoot"
Write-Host
Write-Host "NOTE:"
Write-Host "  Warm-up runs intentionally prime the Windows file cache."
Write-Host "  These measurements characterize read-ahead/cache-path"
Write-Host "  throughput, not guaranteed cold-storage throughput."
Write-Host


#
# Intentionally warm the source into the Windows cache.
#
if ($WarmupRuns -gt 0) {
    $warmupChunk = $ChunkMiB[0]
    $warmupOperations = $Operations[
        [Math]::Min(
            $Operations.Count - 1,
            3
        )
    ]

    for ($warmup = 1; $warmup -le $WarmupRuns; $warmup++) {
        Write-Host (
            "Warm-up {0}/{1}: {2} MiB / {3} operations..." -f
            $warmup,
            $WarmupRuns,
            $warmupChunk,
            $warmupOperations
        )

        $warmupOut = Join-Path `
            $resultRoot `
            "warmup-$warmup.out"

        $warmupErr = Join-Path `
            $resultRoot `
            "warmup-$warmup.err"

        $null = Invoke-ReadAhead `
            -Exe $exe `
            -RepoRoot $repoRoot `
            -SourcePath $sourcePath `
            -Chunk $warmupChunk `
            -OperationCount $warmupOperations `
            -StdOutPath $warmupOut `
            -StdErrPath $warmupErr `
            -Timeout $TimeoutSeconds
    }
}


$results = @()

$totalRuns = $Repeats * $configurations.Count
$currentRun = 0


for ($repeat = 1; $repeat -le $Repeats; $repeat++) {
    #
    # Randomize every round so time/thermal/background drift
    # cannot consistently favor one queue depth.
    #
    $roundConfigurations =
        $configurations |
        Sort-Object {
            Get-Random
        }

    foreach ($config in $roundConfigurations) {
        $currentRun++

        Write-Host
        Write-Host (
            "[{0}/{1}] Repeat {2}/{3}: {4}" -f
            $currentRun,
            $totalRuns,
            $repeat,
            $Repeats,
            $config.Name
        )

        $prefix = (
            "{0:D4}-r{1:D2}-{2}" -f
            $currentRun,
            $repeat,
            $config.Name
        )

        $stdoutPath = Join-Path `
            $resultRoot `
            "$prefix.out"

        $stderrPath = Join-Path `
            $resultRoot `
            "$prefix.err"

        $metrics = Invoke-ReadAhead `
            -Exe $exe `
            -RepoRoot $repoRoot `
            -SourcePath $sourcePath `
            -Chunk $config.ChunkMiB `
            -OperationCount $config.Operations `
            -StdOutPath $stdoutPath `
            -StdErrPath $stderrPath `
            -Timeout $TimeoutSeconds

        $immediatePercent =
            if ($metrics.ReadSubmissions -eq 0) {
                0.0
            }
            else {
                (
                    $metrics.ImmediateReads /
                    $metrics.ReadSubmissions
                ) * 100.0
            }

        $result = [pscustomobject]@{
            Run = $currentRun

            Repeat = $repeat

            Configuration = $config.Name

            ChunkMiB = $metrics.ChunkMiB

            Operations = $metrics.Operations

            PoolMiB = $metrics.PoolMiB

            BytesRead = $metrics.BytesRead

            ReadSubmissions =
                $metrics.ReadSubmissions

            ImmediateReads =
                $metrics.ImmediateReads

            ImmediatePercent =
                $immediatePercent

            ReadSeconds =
                $metrics.ReadSeconds

            TotalSeconds =
                $metrics.TotalSeconds

            MBps =
                $metrics.MBps

            MiBps =
                $metrics.MiBps
        }

        $results += $result

        #
        # Preserve every completed result immediately.
        #
        $results |
            Export-Csv `
                -LiteralPath $rawCsv `
                -NoTypeInformation

        Write-Host (
            "  {0:N2} MB/s ({1:N3} s), immediate {2}/{3}" -f
            $result.MBps,
            $result.ReadSeconds,
            $result.ImmediateReads,
            $result.ReadSubmissions
        )
    }
}


$summary = foreach (
    $group in (
        $results |
        Group-Object Configuration
    )
) {
    $throughputStats = Measure-Values @(
        $group.Group |
        ForEach-Object {
            [double]$_.MBps
        }
    )

    $readTimeStats = Measure-Values @(
        $group.Group |
        ForEach-Object {
            [double]$_.ReadSeconds
        }
    )

    $first = $group.Group[0]

    [pscustomobject]@{
        Configuration =
            $group.Name

        ChunkMiB =
            $first.ChunkMiB

        Operations =
            $first.Operations

        PoolMiB =
            $first.PoolMiB

        Runs =
            $group.Count

        MeanMBps =
            [Math]::Round(
                $throughputStats.Average,
                2
            )

        StdDevMBps =
            [Math]::Round(
                $throughputStats.StdDev,
                2
            )

        CvPercent =
            [Math]::Round(
                $throughputStats.CvPercent,
                2
            )

        MinMBps =
            [Math]::Round(
                $throughputStats.Minimum,
                2
            )

        MaxMBps =
            [Math]::Round(
                $throughputStats.Maximum,
                2
            )

        MeanReadSeconds =
            [Math]::Round(
                $readTimeStats.Average,
                6
            )

        ImmediateReads =
            (
                $group.Group |
                Measure-Object `
                    -Property ImmediateReads `
                    -Sum
            ).Sum

        ReadSubmissions =
            (
                $group.Group |
                Measure-Object `
                    -Property ReadSubmissions `
                    -Sum
            ).Sum
    }
}


$summary =
    $summary |
    Sort-Object MeanMBps -Descending


$summary |
    Export-Csv `
        -LiteralPath $summaryCsv `
        -NoTypeInformation


Write-Host
Write-Host "============================================================"
Write-Host "IOCP read-ahead benchmark complete"
Write-Host "============================================================"
Write-Host

$summary |
    Format-Table `
        Configuration,
        Runs,
        MeanMBps,
        StdDevMBps,
        CvPercent,
        MinMBps,
        MaxMBps,
        MeanReadSeconds `
        -AutoSize

Write-Host
Write-Host "Raw results:"
Write-Host "  $rawCsv"
Write-Host
Write-Host "Summary:"
Write-Host "  $summaryCsv"
Write-Host


if ($summary.Count -gt 0) {
    Write-Host "Highest mean read throughput:"
    Write-Host (
        "  {0}: {1:N2} MB/s" -f
        $summary[0].Configuration,
        $summary[0].MeanMBps
    )

    Write-Host
}


Write-Host (
    "Reminder: repeated reads of one source primarily characterize " +
    "the Windows cached read path after warm-up."
)