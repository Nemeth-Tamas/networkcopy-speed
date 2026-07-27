param(
    [UInt64]$RandomSizeGiB = 2,
    [UInt64]$MixedSizeGiB = 4,
    [string]$OutputDirectory = ".\TestData"
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$GiB = [UInt64]1GB
$ChunkBytes = 8MB

function New-PatternBuffer {
    param(
        [int]$Length
    )

    $seed = [Text.Encoding]::UTF8.GetBytes(
        "NetworkCopy Speed Edition benchmark payload. " +
        "This section represents structured and highly compressible application data.`r`n"
    )

    $buffer = New-Object byte[] $Length
    [Array]::Copy($seed, 0, $buffer, 0, $seed.Length)

    $filled = $seed.Length

    while ($filled -lt $buffer.Length) {
        $copyLength = [Math]::Min($filled, $buffer.Length - $filled)
        [Array]::Copy($buffer, 0, $buffer, $filled, $copyLength)
        $filled += $copyLength
    }

    return $buffer
}

function Write-RandomFile {
    param(
        [string]$Path,
        [UInt64]$Length
    )

    Write-Host "Creating incompressible file:"
    Write-Host "  $Path"
    Write-Host "  $([Math]::Round($Length / 1GB, 2)) GiB"

    $buffer = New-Object byte[] $ChunkBytes
    $random = [Random]::new(1313022789)

    $stream = [IO.FileStream]::new(
        $Path,
        [IO.FileMode]::Create,
        [IO.FileAccess]::Write,
        [IO.FileShare]::None,
        $ChunkBytes,
        [IO.FileOptions]::SequentialScan
    )

    try {
        [UInt64]$remaining = $Length
        [UInt64]$written = 0

        while ($remaining -gt 0) {
            $random.NextBytes($buffer)

            $count = [int][Math]::Min(
                [UInt64]$buffer.Length,
                $remaining
            )

            $stream.Write($buffer, 0, $count)

            $remaining -= [UInt64]$count
            $written += [UInt64]$count

            $percent = [Math]::Floor(($written * 100.0) / $Length)
            Write-Progress `
                -Activity "Writing incompressible benchmark file" `
                -Status "$percent% complete" `
                -PercentComplete $percent
        }

        $stream.Flush($true)
    }
    finally {
        $stream.Dispose()
        Write-Progress `
            -Activity "Writing incompressible benchmark file" `
            -Completed
    }
}

function Write-MixedFile {
    param(
        [string]$Path,
        [UInt64]$Length
    )

    Write-Host
    Write-Host "Creating mixed-compressibility file:"
    Write-Host "  $Path"
    Write-Host "  $([Math]::Round($Length / 1GB, 2)) GiB"

    $randomBuffer = New-Object byte[] $ChunkBytes
    $zeroBuffer = New-Object byte[] $ChunkBytes
    $patternBuffer = New-PatternBuffer -Length $ChunkBytes
    $random = [Random]::new(1397769541)

    $stream = [IO.FileStream]::new(
        $Path,
        [IO.FileMode]::Create,
        [IO.FileAccess]::Write,
        [IO.FileShare]::None,
        $ChunkBytes,
        [IO.FileOptions]::SequentialScan
    )

    try {
        [UInt64]$remaining = $Length
        [UInt64]$written = 0
        [UInt64]$chunkIndex = 0

        while ($remaining -gt 0) {
            # 50% pseudo-random, 25% zero-filled, 25% repeated structured data.
            switch ($chunkIndex % 4) {
                0 {
                    $random.NextBytes($randomBuffer)
                    $buffer = $randomBuffer
                }

                1 {
                    $buffer = $zeroBuffer
                }

                2 {
                    $buffer = $patternBuffer
                }

                3 {
                    $random.NextBytes($randomBuffer)
                    $buffer = $randomBuffer
                }
            }

            $count = [int][Math]::Min(
                [UInt64]$buffer.Length,
                $remaining
            )

            $stream.Write($buffer, 0, $count)

            $remaining -= [UInt64]$count
            $written += [UInt64]$count
            $chunkIndex++

            $percent = [Math]::Floor(($written * 100.0) / $Length)
            Write-Progress `
                -Activity "Writing mixed benchmark file" `
                -Status "$percent% complete" `
                -PercentComplete $percent
        }

        $stream.Flush($true)
    }
    finally {
        $stream.Dispose()
        Write-Progress `
            -Activity "Writing mixed benchmark file" `
            -Completed
    }
}

New-Item `
    -ItemType Directory `
    -Path $OutputDirectory `
    -Force | Out-Null

$randomPath = Join-Path $OutputDirectory "incompressible-$($RandomSizeGiB)GiB.bin"
$mixedPath = Join-Path $OutputDirectory "mixed-$($MixedSizeGiB)GiB.bin"

Write-RandomFile `
    -Path $randomPath `
    -Length ($RandomSizeGiB * $GiB)

Write-MixedFile `
    -Path $mixedPath `
    -Length ($MixedSizeGiB * $GiB)

Write-Host
Write-Host "Benchmark files created successfully:"
Get-Item $randomPath, $mixedPath |
    Format-Table Name, Length, FullName -AutoSize