[CmdletBinding()]
param(
    [string]$Root = "C:\NCS-v2-source",
    [ValidateRange(1, 1024)]
    [int]$FileSizeMiB = 60
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$MiB = 1MB
[long]$FileBytes = [long]$FileSizeMiB * $MiB
[long]$InsertionOffset = ([long]$FileSizeMiB * $MiB / 2) + 123
$ExpectedFileCount = 9
[long]$ExpectedTotalBytes = ($FileBytes * 9) + 4097

function Write-DeterministicFile {
    param(
        [Parameter(Mandatory)]
        [string]$Path,

        [Parameter(Mandatory)]
        [int]$Seed,

        [Parameter(Mandatory)]
        [long]$Length
    )

    $random = [System.Random]::new($Seed)
    $buffer = New-Object byte[] $MiB
    $stream = [System.IO.File]::Open(
        $Path,
        [System.IO.FileMode]::Create,
        [System.IO.FileAccess]::Write,
        [System.IO.FileShare]::None
    )

    try {
        [long]$remaining = $Length

        while ($remaining -gt 0) {
            $count = [int][Math]::Min([long]$buffer.Length, $remaining)
            $random.NextBytes($buffer)
            $stream.Write($buffer, 0, $count)
            $remaining -= $count
        }

        $stream.Flush()
    }
    finally {
        $stream.Dispose()
    }
}

Write-Host "Creating NetworkCopy v2 physical acceptance corpus"
Write-Host "Destination: $Root"
Write-Host "Base file size: $FileSizeMiB MiB"
Write-Host

if (Test-Path -LiteralPath $Root) {
    Write-Host "Removing existing corpus..."
    Remove-Item -LiteralPath $Root -Recurse -Force
}

New-Item -ItemType Directory -Path $Root -Force | Out-Null

Write-Host "Creating 00-basis.bin..."
Write-DeterministicFile `
    -Path (Join-Path $Root "00-basis.bin") `
    -Seed 1000 `
    -Length $FileBytes

foreach ($index in 1..7) {
    $name = "{0:D2}-filler.bin" -f $index
    Write-Host "Creating $name..."

    Write-DeterministicFile `
        -Path (Join-Path $Root $name) `
        -Seed (1000 + $index) `
        -Length $FileBytes
}

$BasisPath = Join-Path $Root "00-basis.bin"
$TargetPath = Join-Path $Root "08-target.bin"

Write-Host "Creating 08-target.bin with a 4097-byte middle insertion..."

$sourceStream = [System.IO.File]::OpenRead($BasisPath)
$targetStream = [System.IO.File]::Open(
    $TargetPath,
    [System.IO.FileMode]::Create,
    [System.IO.FileAccess]::Write,
    [System.IO.FileShare]::None
)

$copyBuffer = New-Object byte[] $MiB
$insertion = New-Object byte[] 4097
$insertionRandom = [System.Random]::new(9001)
$insertionRandom.NextBytes($insertion)

try {
    [long]$remaining = $InsertionOffset

    while ($remaining -gt 0) {
        $count = [int][Math]::Min([long]$copyBuffer.Length, $remaining)
        $bytesRead = $sourceStream.Read($copyBuffer, 0, $count)

        if ($bytesRead -eq 0) {
            throw "Basis file ended before the insertion point."
        }

        $targetStream.Write($copyBuffer, 0, $bytesRead)
        $remaining -= $bytesRead
    }

    $targetStream.Write($insertion, 0, $insertion.Length)
    $sourceStream.CopyTo($targetStream)
    $targetStream.Flush()
}
finally {
    $targetStream.Dispose()
    $sourceStream.Dispose()
}

$files = Get-ChildItem -LiteralPath $Root -File | Sort-Object Name
$fileCount = $files.Count
[long]$totalBytes = ($files | Measure-Object Length -Sum).Sum

Write-Host
Write-Host "Generated files:"
$files | Format-Table Name, Length -AutoSize

if ($fileCount -ne $ExpectedFileCount) {
    throw "Corpus contains $fileCount files; expected $ExpectedFileCount."
}

if ($totalBytes -ne $ExpectedTotalBytes) {
    throw "Corpus contains $totalBytes bytes; expected $ExpectedTotalBytes."
}

$targetLength = (Get-Item -LiteralPath $TargetPath).Length
$expectedTargetLength = $FileBytes + 4097

if ($targetLength -ne $expectedTargetLength) {
    throw "08-target.bin contains $targetLength bytes; expected $expectedTargetLength."
}

$manifestPath = Join-Path $Root "SHA256SUMS.txt"

$files |
    ForEach-Object {
        $hash = (Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash
        "{0},{1},{2}" -f $_.Name, $_.Length, $hash
    } |
    Set-Content -LiteralPath $manifestPath -Encoding UTF8

Write-Host
Write-Host "Corpus ready."
Write-Host "Files:       $fileCount"
Write-Host "Total bytes: $totalBytes"
Write-Host "Hash list:   $manifestPath"
Write-Host
Write-Host "Use this folder as the GUI sender source:"
Write-Host "  $Root"