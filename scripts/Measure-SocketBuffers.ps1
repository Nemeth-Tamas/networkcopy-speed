[CmdletBinding()]
param(
    [ValidateRange(1, 100)]
    [int]$Repeats = 10,

    [ValidateRange(1, 1048576)]
    [int]$PayloadMiB = 16384,

    [ValidateSet(1, 2, 4, 8)]
    [int[]]$Streams = @(4),

    # 0 means "leave Windows default untouched".
    [int[]]$SendBuffersKiB = @(0, 256, 1024, 4096),

    # 0 means "leave Windows default untouched".
    [int[]]$ReceiveBuffersKiB = @(0, 256, 1024, 4096),

    [ValidateRange(1, 65535)]
    [int]$Port = 7337,

    [ValidateRange(10, 3600)]
    [int]$TimeoutSeconds = 300,

    [ValidateRange(0, 20)]
    [int]$WarmupRuns = 2
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

function Test-IsAdministrator {
    $identity =
        [Security.Principal.WindowsIdentity]::GetCurrent()

    $principal =
        [Security.Principal.WindowsPrincipal]::new(
            $identity
        )

    return $principal.IsInRole(
        [Security.Principal.WindowsBuiltInRole]::Administrator
    )
}

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

function Format-BufferName {
    param(
        [int]$BufferKiB
    )

    if ($BufferKiB -eq 0) {
        return "default"
    }

    return "${BufferKiB}KiB"
}

function Start-CapturedProcess {
    param(
        [string]$FilePath,
        [string[]]$Arguments,
        [string]$StdOutPath,
        [string]$StdErrPath
    )

    Remove-Item $StdOutPath -Force -ErrorAction SilentlyContinue
    Remove-Item $StdErrPath -Force -ErrorAction SilentlyContinue

    return Start-Process `
        -FilePath $FilePath `
        -ArgumentList $Arguments `
        -RedirectStandardOutput $StdOutPath `
        -RedirectStandardError $StdErrPath `
        -NoNewWindow `
        -PassThru
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

    return @{
        StdOut = $stdout
        StdErr = $stderr
    }
}

function Wait-ReceiverReady {
    param(
        [System.Diagnostics.Process]$Process,
        [string]$StdOutPath,
        [string]$StdErrPath,
        [int]$TimeoutSeconds
    )

    $timer =
        [System.Diagnostics.Stopwatch]::StartNew()

    while ($timer.Elapsed.TotalSeconds -lt $TimeoutSeconds) {
        if ($Process.HasExited) {
            $output = Read-ProcessOutput `
                -StdOutPath $StdOutPath `
                -StdErrPath $StdErrPath

            throw @"
Receiver exited before becoming ready.

STDOUT:
$($output.StdOut)

STDERR:
$($output.StdErr)
"@
        }

        if (Test-Path $StdOutPath) {
            $text = Get-Content `
                -LiteralPath $StdOutPath `
                -Raw `
                -ErrorAction SilentlyContinue

            if ($text -match "(?m)^\s*Listening:\s+") {
                return
            }
        }

        Start-Sleep -Milliseconds 50
    }

    try {
        $Process.Kill()
    }
    catch {
    }

    throw "Receiver did not become ready within $TimeoutSeconds seconds."
}

function Wait-ProcessWithTimeout {
    param(
        [System.Diagnostics.Process]$Process,
        [int]$TimeoutSeconds,
        [string]$Description
    )

    $milliseconds =
        [Math]::Min(
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

    # Ensure redirected output is completely flushed.
    $Process.WaitForExit()
}

function Parse-CalibrationOutput {
    param(
        [string]$Text,
        [string]$Description
    )

    $elapsed =
        [regex]::Match(
            $Text,
            "(?m)^\s*Elapsed:\s+([0-9.]+)\s+s\s*$"
        )

    $throughput =
        [regex]::Match(
            $Text,
            "(?m)^\s*Raw throughput:\s+([0-9.]+)\s+MB/s"
        )

    $sendBuffer =
        [regex]::Match(
            $Text,
            "(?m)^\s*Socket send buffer:\s*([0-9,]+)\s+bytes\s*$"
        )

    $receiveBuffer =
        [regex]::Match(
            $Text,
            "(?m)^\s*Socket receive buffer:\s*([0-9,]+)\s+bytes\s*$"
        )

    foreach ($match in @(
        $elapsed,
        $throughput,
        $sendBuffer,
        $receiveBuffer
    )) {
        if (-not $match.Success) {
            throw @"
Could not parse $Description output:

$Text
"@
        }
    }

    $culture =
        [Globalization.CultureInfo]::InvariantCulture

    return [pscustomobject]@{
        ElapsedSeconds =
            [double]::Parse(
                $elapsed.Groups[1].Value,
                $culture
            )

        MegabytesPerSecond =
            [double]::Parse(
                $throughput.Groups[1].Value,
                $culture
            )

        SendBufferBytes =
            [Convert]::ToUInt64(
                $sendBuffer.Groups[1].Value.Replace(",", "")
            )

        ReceiveBufferBytes =
            [Convert]::ToUInt64(
                $receiveBuffer.Groups[1].Value.Replace(",", "")
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

    $average =
        ($Values | Measure-Object -Average).Average

    $minimum =
        ($Values | Measure-Object -Minimum).Minimum

    $maximum =
        ($Values | Measure-Object -Maximum).Maximum

    $variance = 0.0

    if ($Values.Count -gt 1) {
        foreach ($value in $Values) {
            $difference = $value - $average

            $variance +=
                $difference * $difference
        }

        $variance /= ($Values.Count - 1)
    }

    $standardDeviation =
        [Math]::Sqrt($variance)

    $coefficientOfVariation =
        if ($average -eq 0.0) {
            0.0
        }
        else {
            $standardDeviation / $average * 100.0
        }

    return [pscustomobject]@{
        Average = $average
        StdDev = $standardDeviation
        CvPercent = $coefficientOfVariation
        Minimum = $minimum
        Maximum = $maximum
    }
}

if (-not (Test-IsAdministrator)) {
    throw @"
This benchmark harness must be run from an elevated PowerShell.

Right-click PowerShell / Windows Terminal and choose:
Run as administrator
"@
}

foreach ($buffer in $SendBuffersKiB + $ReceiveBuffersKiB) {
    if ($buffer -lt 0) {
        throw "Socket buffer values must be zero or greater."
    }
}

$repoRoot = Find-RepoRoot

$exe =
    Join-Path `
        $repoRoot `
        "target\release\networkcopy-speed.exe"

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

if (-not (Test-Path $exe)) {
    throw "Release executable was not produced: $exe"
}

$timestamp =
    Get-Date -Format "yyyyMMdd-HHmmss"

$resultRoot =
    Join-Path `
        $repoRoot `
        "bench-results\socket-buffers-$timestamp"

New-Item `
    -ItemType Directory `
    -Path $resultRoot `
    -Force |
    Out-Null

$rawCsv =
    Join-Path $resultRoot "raw-results.csv"

$summaryCsv =
    Join-Path $resultRoot "summary.csv"

$configurations = @()

foreach ($streamCount in $Streams) {
    foreach ($sendBuffer in $SendBuffersKiB) {
        foreach ($receiveBuffer in $ReceiveBuffersKiB) {
            $configurations += [pscustomobject]@{
                Streams = $streamCount
                SendKiB = $sendBuffer
                ReceiveKiB = $receiveBuffer

                Name = (
                    "streams-{0}__send-{1}__recv-{2}" -f
                    $streamCount,
                    (Format-BufferName $sendBuffer),
                    (Format-BufferName $receiveBuffer)
                )
            }
        }
    }
}

Write-Host
Write-Host "NetworkCopy socket-buffer benchmark"
Write-Host "  EXE:            $exe"
Write-Host "  Payload/run:    $PayloadMiB MiB"
Write-Host "  Repeats/config: $Repeats"
Write-Host "  Configurations: $($configurations.Count)"
Write-Host "  Warm-up runs:   $WarmupRuns"
Write-Host "  Results:        $resultRoot"
Write-Host

#
# Warm up the loopback path before measured runs.
#
for ($warmup = 1; $warmup -le $WarmupRuns; $warmup++) {
    Write-Host "Warm-up $warmup / $WarmupRuns..."

    $receiverOut =
        Join-Path $resultRoot "warmup-receiver-$warmup.out"

    $receiverErr =
        Join-Path $resultRoot "warmup-receiver-$warmup.err"

    $senderOut =
        Join-Path $resultRoot "warmup-sender-$warmup.out"

    $senderErr =
        Join-Path $resultRoot "warmup-sender-$warmup.err"

    $receiver = $null
    $sender = $null

    try {
        $receiver = Start-CapturedProcess `
            -FilePath $exe `
            -Arguments @(
                "bench-network-receive",
                "127.0.0.1:$Port"
            ) `
            -StdOutPath $receiverOut `
            -StdErrPath $receiverErr

        Wait-ReceiverReady `
            -Process $receiver `
            -StdOutPath $receiverOut `
            -StdErrPath $receiverErr `
            -TimeoutSeconds $TimeoutSeconds

        $sender = Start-CapturedProcess `
            -FilePath $exe `
            -Arguments @(
                "bench-network-send",
                "127.0.0.1:$Port",
                "$PayloadMiB",
                "4"
            ) `
            -StdOutPath $senderOut `
            -StdErrPath $senderErr

        Wait-ProcessWithTimeout `
            -Process $sender `
            -TimeoutSeconds $TimeoutSeconds `
            -Description "Warm-up sender"

        Wait-ProcessWithTimeout `
            -Process $receiver `
            -TimeoutSeconds $TimeoutSeconds `
            -Description "Warm-up receiver"

        if ($sender.ExitCode -ne 0 -or $receiver.ExitCode -ne 0) {
            throw "Warm-up process returned a nonzero exit code."
        }
    }
    finally {
        if ($sender -and -not $sender.HasExited) {
            try {
                $sender.Kill()
            }
            catch {
            }
        }

        if ($receiver -and -not $receiver.HasExited) {
            try {
                $receiver.Kill()
            }
            catch {
            }
        }
    }
}

$results = @()

$totalRuns =
    $Repeats * $configurations.Count

$currentRun = 0

for ($repeat = 1; $repeat -le $Repeats; $repeat++) {
    #
    # Randomize each round so thermal/load/time drift does not
    # consistently favor one configuration.
    #
    $roundConfigurations =
        $configurations |
        Sort-Object { Get-Random }

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

        $prefix =
            "{0:D4}-r{1:D2}-{2}" -f
            $currentRun,
            $repeat,
            $config.Name

        $receiverOut =
            Join-Path $resultRoot "$prefix-receiver.out"

        $receiverErr =
            Join-Path $resultRoot "$prefix-receiver.err"

        $senderOut =
            Join-Path $resultRoot "$prefix-sender.out"

        $senderErr =
            Join-Path $resultRoot "$prefix-sender.err"

        $receiverArgs = @(
            "bench-network-receive",
            "127.0.0.1:$Port"
        )

        if ($config.ReceiveKiB -ne 0) {
            $receiverArgs += "$($config.ReceiveKiB)"
        }

        $senderArgs = @(
            "bench-network-send",
            "127.0.0.1:$Port",
            "$PayloadMiB",
            "$($config.Streams)"
        )

        if ($config.SendKiB -ne 0) {
            $senderArgs += "$($config.SendKiB)"
        }

        $receiver = $null
        $sender = $null

        try {
            $receiver = Start-CapturedProcess `
                -FilePath $exe `
                -Arguments $receiverArgs `
                -StdOutPath $receiverOut `
                -StdErrPath $receiverErr

            Wait-ReceiverReady `
                -Process $receiver `
                -StdOutPath $receiverOut `
                -StdErrPath $receiverErr `
                -TimeoutSeconds $TimeoutSeconds

            $sender = Start-CapturedProcess `
                -FilePath $exe `
                -Arguments $senderArgs `
                -StdOutPath $senderOut `
                -StdErrPath $senderErr

            Wait-ProcessWithTimeout `
                -Process $sender `
                -TimeoutSeconds $TimeoutSeconds `
                -Description "Sender"

            Wait-ProcessWithTimeout `
                -Process $receiver `
                -TimeoutSeconds $TimeoutSeconds `
                -Description "Receiver"

            $senderOutput =
                Read-ProcessOutput `
                    -StdOutPath $senderOut `
                    -StdErrPath $senderErr

            $receiverOutput =
                Read-ProcessOutput `
                    -StdOutPath $receiverOut `
                    -StdErrPath $receiverErr

            if ($sender.ExitCode -ne 0) {
                throw @"
Sender failed with exit code $($sender.ExitCode).

STDOUT:
$($senderOutput.StdOut)

STDERR:
$($senderOutput.StdErr)
"@
            }

            if ($receiver.ExitCode -ne 0) {
                throw @"
Receiver failed with exit code $($receiver.ExitCode).

STDOUT:
$($receiverOutput.StdOut)

STDERR:
$($receiverOutput.StdErr)
"@
            }

            $senderMetrics =
                Parse-CalibrationOutput `
                    -Text $senderOutput.StdOut `
                    -Description "sender"

            $receiverMetrics =
                Parse-CalibrationOutput `
                    -Text $receiverOutput.StdOut `
                    -Description "receiver"

            $result = [pscustomobject]@{
                Run = $currentRun
                Repeat = $repeat
                Configuration = $config.Name

                Streams = $config.Streams

                RequestedSendKiB =
                    if ($config.SendKiB -eq 0) {
                        "default"
                    }
                    else {
                        $config.SendKiB
                    }

                RequestedReceiveKiB =
                    if ($config.ReceiveKiB -eq 0) {
                        "default"
                    }
                    else {
                        $config.ReceiveKiB
                    }

                SenderElapsedSeconds =
                    $senderMetrics.ElapsedSeconds

                SenderMBps =
                    $senderMetrics.MegabytesPerSecond

                ReceiverElapsedSeconds =
                    $receiverMetrics.ElapsedSeconds

                ReceiverMBps =
                    $receiverMetrics.MegabytesPerSecond

                ActualSenderSendBufferBytes =
                    $senderMetrics.SendBufferBytes

                ActualSenderReceiveBufferBytes =
                    $senderMetrics.ReceiveBufferBytes

                ActualReceiverSendBufferBytes =
                    $receiverMetrics.SendBufferBytes

                ActualReceiverReceiveBufferBytes =
                    $receiverMetrics.ReceiveBufferBytes
            }

            $results += $result

            #
            # Save after every successful run so a later interruption
            # does not throw away earlier measurements.
            #
            $results |
                Export-Csv `
                    -LiteralPath $rawCsv `
                    -NoTypeInformation

            Write-Host (
                "  sender:   {0:N2} MB/s ({1:N3} s)" -f
                $result.SenderMBps,
                $result.SenderElapsedSeconds
            )

            Write-Host (
                "  receiver: {0:N2} MB/s ({1:N3} s)" -f
                $result.ReceiverMBps,
                $result.ReceiverElapsedSeconds
            )
        }
        finally {
            if ($sender -and -not $sender.HasExited) {
                try {
                    $sender.Kill()
                }
                catch {
                }
            }

            if ($receiver -and -not $receiver.HasExited) {
                try {
                    $receiver.Kill()
                }
                catch {
                }
            }
        }
    }
}

$summary = foreach (
    $group in (
        $results |
        Group-Object Configuration
    )
) {
    $senderStats =
        Measure-Values @(
            $group.Group |
            ForEach-Object {
                [double]$_.SenderMBps
            }
        )

    $receiverStats =
        Measure-Values @(
            $group.Group |
            ForEach-Object {
                [double]$_.ReceiverMBps
            }
        )

    $first = $group.Group[0]

    [pscustomobject]@{
        Configuration =
            $group.Name

        Streams =
            $first.Streams

        RequestedSendKiB =
            $first.RequestedSendKiB

        RequestedReceiveKiB =
            $first.RequestedReceiveKiB

        Runs =
            $group.Count

        SenderMeanMBps =
            [Math]::Round(
                $senderStats.Average,
                2
            )

        SenderStdDevMBps =
            [Math]::Round(
                $senderStats.StdDev,
                2
            )

        SenderCvPercent =
            [Math]::Round(
                $senderStats.CvPercent,
                2
            )

        SenderMinMBps =
            [Math]::Round(
                $senderStats.Minimum,
                2
            )

        SenderMaxMBps =
            [Math]::Round(
                $senderStats.Maximum,
                2
            )

        ReceiverMeanMBps =
            [Math]::Round(
                $receiverStats.Average,
                2
            )

        ReceiverStdDevMBps =
            [Math]::Round(
                $receiverStats.StdDev,
                2
            )

        ReceiverCvPercent =
            [Math]::Round(
                $receiverStats.CvPercent,
                2
            )

        ReceiverMinMBps =
            [Math]::Round(
                $receiverStats.Minimum,
                2
            )

        ReceiverMaxMBps =
            [Math]::Round(
                $receiverStats.Maximum,
                2
            )

        ActualSenderSendBufferBytes =
            $first.ActualSenderSendBufferBytes

        ActualReceiverReceiveBufferBytes =
            $first.ActualReceiverReceiveBufferBytes
    }
}

$summary =
    $summary |
    Sort-Object SenderMeanMBps -Descending

$summary |
    Export-Csv `
        -LiteralPath $summaryCsv `
        -NoTypeInformation

Write-Host
Write-Host "============================================================"
Write-Host "Socket-buffer benchmark complete"
Write-Host "============================================================"
Write-Host

$summary |
    Format-Table `
        Configuration,
        Runs,
        SenderMeanMBps,
        SenderStdDevMBps,
        SenderCvPercent,
        ReceiverMeanMBps,
        ReceiverStdDevMBps,
        ReceiverCvPercent `
        -AutoSize

Write-Host
Write-Host "Raw results:"
Write-Host "  $rawCsv"
Write-Host
Write-Host "Summary:"
Write-Host "  $summaryCsv"
Write-Host

if ($summary.Count -gt 0) {
    Write-Host "Highest loopback sender mean:"
    Write-Host (
        "  {0}: {1:N2} MB/s" -f
        $summary[0].Configuration,
        $summary[0].SenderMeanMBps
    )
    Write-Host
}

Write-Host (
    "These are loopback results only; do not treat the winner as " +
    "the production LAN setting until physical-link testing."
)