# PATCHED VERSION: byte arrays are created locally inside mutation functions.
# This file intentionally contains no New-SeededBytes function.

[CmdletBinding()]
param(
    [string]$Root = (Join-Path $env:TEMP "NetworkCopy-CdcLoopback"),
    [switch]$VerifyOnly
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$SourceRoot = Join-Path $Root "SenderNew"
$DestinationRoot = Join-Path $Root "ReceiverOld"

function Ensure-ParentDirectory {
    param([Parameter(Mandatory)][string]$Path)

    $parent = Split-Path -Parent $Path
    if ($parent) {
        New-Item -ItemType Directory -Path $parent -Force | Out-Null
    }
}

function Write-SeededFile {
    param(
        [Parameter(Mandatory)][string]$Path,
        [Parameter(Mandatory)][long]$Length,
        [Parameter(Mandatory)][int]$Seed
    )

    Ensure-ParentDirectory -Path $Path

    $random = [System.Random]::new($Seed)
    $buffer = [byte[]]::new(1MB)
    $remaining = $Length

    $stream = [System.IO.File]::Open(
        $Path,
        [System.IO.FileMode]::Create,
        [System.IO.FileAccess]::Write,
        [System.IO.FileShare]::None
    )

    try {
        while ($remaining -gt 0) {
            $random.NextBytes($buffer)
            $count = [int][Math]::Min([long]$buffer.Length, $remaining)
            $stream.Write($buffer, 0, $count)
            $remaining -= $count
        }
    }
    finally {
        $stream.Dispose()
    }
}

function New-BasisPair {
    param(
        [Parameter(Mandatory)][string]$RelativePath,
        [Parameter(Mandatory)][long]$Length,
        [Parameter(Mandatory)][int]$Seed
    )

    $destinationPath = Join-Path $DestinationRoot $RelativePath
    $sourcePath = Join-Path $SourceRoot $RelativePath

    Write-SeededFile -Path $destinationPath -Length $Length -Seed $Seed
    Ensure-ParentDirectory -Path $sourcePath
    Copy-Item -LiteralPath $destinationPath -Destination $sourcePath -Force
}

function Insert-SeededBytes {
    param(
        [Parameter(Mandatory)][string]$Path,
        [Parameter(Mandatory)][int]$Offset,
        [Parameter(Mandatory)][int]$Length,
        [Parameter(Mandatory)][int]$Seed
    )

    $original = [System.IO.File]::ReadAllBytes($Path)
    if ($Offset -lt 0 -or $Offset -gt $original.Length) {
        throw "Insertion offset $Offset is outside $Path."
    }

    $insertion = [byte[]]::new($Length)
    $random = [System.Random]::new($Seed)
    $random.NextBytes($insertion)

    $result = [byte[]]::new($original.Length + $insertion.Length)

    [System.Buffer]::BlockCopy($original, 0, $result, 0, $Offset)
    [System.Buffer]::BlockCopy($insertion, 0, $result, $Offset, $insertion.Length)
    [System.Buffer]::BlockCopy(
        $original,
        $Offset,
        $result,
        $Offset + $insertion.Length,
        $original.Length - $Offset
    )

    [System.IO.File]::WriteAllBytes($Path, $result)
}

function Remove-ByteRange {
    param(
        [Parameter(Mandatory)][string]$Path,
        [Parameter(Mandatory)][int]$Offset,
        [Parameter(Mandatory)][int]$Length
    )

    $original = [System.IO.File]::ReadAllBytes($Path)

    if (
        $Offset -lt 0 -or
        $Length -lt 1 -or
        ($Offset + $Length) -gt $original.Length
    ) {
        throw "Deletion range $Offset..$($Offset + $Length) is outside $Path."
    }

    $result = [byte[]]::new($original.Length - $Length)

    [System.Buffer]::BlockCopy($original, 0, $result, 0, $Offset)
    [System.Buffer]::BlockCopy(
        $original,
        $Offset + $Length,
        $result,
        $Offset,
        $original.Length - ($Offset + $Length)
    )

    [System.IO.File]::WriteAllBytes($Path, $result)
}

function Overwrite-SeededBytes {
    param(
        [Parameter(Mandatory)][string]$Path,
        [Parameter(Mandatory)][long]$Offset,
        [Parameter(Mandatory)][int]$Length,
        [Parameter(Mandatory)][int]$Seed
    )

    $patch = [byte[]]::new($Length)
    $random = [System.Random]::new($Seed)
    $random.NextBytes($patch)

    $stream = [System.IO.File]::Open(
        $Path,
        [System.IO.FileMode]::Open,
        [System.IO.FileAccess]::ReadWrite,
        [System.IO.FileShare]::None
    )

    try {
        if (($Offset + $Length) -gt $stream.Length) {
            throw "Overwrite range exceeds $Path."
        }

        $stream.Position = $Offset
        $stream.Write($patch, 0, $patch.Length)
        $stream.Flush()
    }
    finally {
        $stream.Dispose()
    }
}

function Append-SeededBytes {
    param(
        [Parameter(Mandatory)][string]$Path,
        [Parameter(Mandatory)][int]$Length,
        [Parameter(Mandatory)][int]$Seed
    )

    $append = [byte[]]::new($Length)
    $random = [System.Random]::new($Seed)
    $random.NextBytes($append)

    $stream = [System.IO.File]::Open(
        $Path,
        [System.IO.FileMode]::Append,
        [System.IO.FileAccess]::Write,
        [System.IO.FileShare]::None
    )

    try {
        $stream.Write($append, 0, $append.Length)
        $stream.Flush()
    }
    finally {
        $stream.Dispose()
    }
}

function Test-Corpus {
    if (-not (Test-Path -LiteralPath $SourceRoot -PathType Container)) {
        throw "Source corpus does not exist: $SourceRoot"
    }

    if (-not (Test-Path -LiteralPath $DestinationRoot -PathType Container)) {
        throw "Destination corpus does not exist: $DestinationRoot"
    }

    $sourceFiles = @(
        Get-ChildItem -LiteralPath $SourceRoot -File -Recurse |
            Sort-Object FullName
    )

    $missing = [System.Collections.Generic.List[string]]::new()
    $mismatched = [System.Collections.Generic.List[string]]::new()

    foreach ($sourceFile in $sourceFiles) {
        $relative = [System.IO.Path]::GetRelativePath(
            $SourceRoot,
            $sourceFile.FullName
        )

        $destinationPath = Join-Path $DestinationRoot $relative

        if (-not (Test-Path -LiteralPath $destinationPath -PathType Leaf)) {
            $missing.Add($relative)
            continue
        }

        $sourceHash = (
            Get-FileHash -LiteralPath $sourceFile.FullName -Algorithm SHA256
        ).Hash

        $destinationHash = (
            Get-FileHash -LiteralPath $destinationPath -Algorithm SHA256
        ).Hash

        if ($sourceHash -ne $destinationHash) {
            $mismatched.Add($relative)
        }
    }

    $destinationFiles = @(
        Get-ChildItem -LiteralPath $DestinationRoot -File -Recurse
    )

    $sourceRelative = [System.Collections.Generic.HashSet[string]]::new(
        [System.StringComparer]::OrdinalIgnoreCase
    )

    foreach ($sourceFile in $sourceFiles) {
        [void]$sourceRelative.Add(
            [System.IO.Path]::GetRelativePath($SourceRoot, $sourceFile.FullName)
        )
    }

    $extra = [System.Collections.Generic.List[string]]::new()

    foreach ($destinationFile in $destinationFiles) {
        $relative = [System.IO.Path]::GetRelativePath(
            $DestinationRoot,
            $destinationFile.FullName
        )

        if (
            -not $sourceRelative.Contains($relative) -and
            -not $destinationFile.Name.StartsWith(".networkcopy", [System.StringComparison]::OrdinalIgnoreCase)
        ) {
            $extra.Add($relative)
        }
    }

    [PSCustomObject]@{
        SourceFiles = $sourceFiles.Count
        DestinationFiles = $destinationFiles.Count
        Missing = $missing.Count
        Mismatched = $mismatched.Count
        Extra = $extra.Count
        AllMatch = (
            $missing.Count -eq 0 -and
            $mismatched.Count -eq 0 -and
            $extra.Count -eq 0
        )
    } | Format-List

    if ($missing.Count -gt 0) {
        Write-Host "`nMissing files:" -ForegroundColor Red
        $missing | Select-Object -First 20 | ForEach-Object { Write-Host "  $_" }
    }

    if ($mismatched.Count -gt 0) {
        Write-Host "`nMismatched files:" -ForegroundColor Red
        $mismatched | Select-Object -First 20 | ForEach-Object { Write-Host "  $_" }
    }

    if ($extra.Count -gt 0) {
        Write-Host "`nExtra destination files:" -ForegroundColor Yellow
        $extra | Select-Object -First 20 | ForEach-Object { Write-Host "  $_" }
    }

    if (
        $missing.Count -gt 0 -or
        $mismatched.Count -gt 0 -or
        $extra.Count -gt 0
    ) {
        throw "Loopback corpus verification failed."
    }

    Write-Host "All source and destination files match." -ForegroundColor Green
}

if ($VerifyOnly) {
    Test-Corpus
    return
}

if (Test-Path -LiteralPath $Root) {
    Remove-Item -LiteralPath $Root -Recurse -Force
}

New-Item -ItemType Directory -Path $SourceRoot -Force | Out-Null
New-Item -ItemType Directory -Path $DestinationRoot -Force | Out-Null

Write-Host "Creating NetworkCopy loopback acceptance corpus..." -ForegroundColor Cyan

# Unchanged medium file: should be verified and skipped.
New-BasisPair `
    -RelativePath "unchanged\medium-unchanged.bin" `
    -Length (4MB) `
    -Seed 1001

# Four profitable medium-file CDC updates.
New-BasisPair `
    -RelativePath "cdc\insert-4097.bin" `
    -Length (8MB) `
    -Seed 2001

New-BasisPair `
    -RelativePath "cdc\delete-4097.bin" `
    -Length (8MB) `
    -Seed 2002

New-BasisPair `
    -RelativePath "cdc\overwrite-64k.bin" `
    -Length (8MB) `
    -Seed 2003

New-BasisPair `
    -RelativePath "unicode\árvíztűrő-tükörfúrógép.bin" `
    -Length (2MB) `
    -Seed 2004

# Same-path but unrelated medium file: receiver should offer CDC,
# sender should reject it as unprofitable and fall back to full transfer.
New-BasisPair `
    -RelativePath "fallback\unrelated-medium.bin" `
    -Length (4MB) `
    -Seed 3001

# Small changed file: below the 1 MiB CDC threshold.
New-BasisPair `
    -RelativePath "small\changed-small.bin" `
    -Length (512KB) `
    -Seed 4001

# Large changed file: should remain on the existing striped path.
New-BasisPair `
    -RelativePath "large\striped-large.bin" `
    -Length (72MB) `
    -Seed 5001

# 1,024 tiny files. Every eighth file is changed, leaving 896 unchanged
# files to verify/skip and 128 files for tiny-pack transfer.
for ($index = 0; $index -lt 1024; $index++) {
    $group = [int]($index / 64)
    $relative = "tiny\group-{0:D2}\tiny-{1:D4}.bin" -f $group, $index
    $length = 1024 + (($index * 7919) % 7168)

    New-BasisPair `
        -RelativePath $relative `
        -Length $length `
        -Seed (10000 + $index)
}

# Ensure mutated files receive visibly newer timestamps than their bases.
Start-Sleep -Milliseconds 1200

Insert-SeededBytes `
    -Path (Join-Path $SourceRoot "cdc\insert-4097.bin") `
    -Offset ((3MB) + 123) `
    -Length 4097 `
    -Seed 2101

Remove-ByteRange `
    -Path (Join-Path $SourceRoot "cdc\delete-4097.bin") `
    -Offset ((5MB) + 321) `
    -Length 4097

Overwrite-SeededBytes `
    -Path (Join-Path $SourceRoot "cdc\overwrite-64k.bin") `
    -Offset ((4MB) + 777) `
    -Length (64KB) `
    -Seed 2103

Append-SeededBytes `
    -Path (Join-Path $SourceRoot "unicode\árvíztűrő-tükörfúrógép.bin") `
    -Length 12345 `
    -Seed 2104

Write-SeededFile `
    -Path (Join-Path $SourceRoot "fallback\unrelated-medium.bin") `
    -Length (4MB) `
    -Seed 3999

Write-SeededFile `
    -Path (Join-Path $SourceRoot "fallback\new-medium.bin") `
    -Length (4MB) `
    -Seed 3002

Overwrite-SeededBytes `
    -Path (Join-Path $SourceRoot "small\changed-small.bin") `
    -Offset (128KB) `
    -Length (16KB) `
    -Seed 4101

Overwrite-SeededBytes `
    -Path (Join-Path $SourceRoot "large\striped-large.bin") `
    -Offset (24MB) `
    -Length (1MB) `
    -Seed 5101

for ($index = 0; $index -lt 1024; $index += 8) {
    $group = [int]($index / 64)
    $relative = "tiny\group-{0:D2}\tiny-{1:D4}.bin" -f $group, $index
    $path = Join-Path $SourceRoot $relative
    $length = [int][Math]::Min(2048, (Get-Item -LiteralPath $path).Length)

    Overwrite-SeededBytes `
        -Path $path `
        -Offset 0 `
        -Length $length `
        -Seed (20000 + $index)
}

$sourceBytes = (
    Get-ChildItem -LiteralPath $SourceRoot -File -Recurse |
        Measure-Object -Property Length -Sum
).Sum

$destinationBytes = (
    Get-ChildItem -LiteralPath $DestinationRoot -File -Recurse |
        Measure-Object -Property Length -Sum
).Sum

Write-Host ""
Write-Host "Corpus ready." -ForegroundColor Green
Write-Host "  Sender source:   $SourceRoot"
Write-Host "  Receiver basis:  $DestinationRoot"
Write-Host "  Files:           1,033"
Write-Host "  Sender bytes:    $sourceBytes"
Write-Host "  Receiver bytes:  $destinationBytes"
Write-Host ""
Write-Host "Expected update behavior:"
Write-Host "  CDC offers:                 5"
Write-Host "  Successful CDC updates:     4"
Write-Host "  CDC whole-file fallbacks:   1"
Write-Host "  Tiny files changed:         128"
Write-Host "  Unchanged files skipped:    at least 897"
Write-Host "  Large striped files:        1"
Write-Host ""
Write-Host "After the GUI transfer, verify with:"
Write-Host "  & `"$PSCommandPath`" -Root `"$Root`" -VerifyOnly"