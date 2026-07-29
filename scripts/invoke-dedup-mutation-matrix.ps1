[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string] $BasisPath,

    [string] $OutputRoot = ".\DedupMutationCorpus",

    [string] $Executable =
        ".\target\release\networkcopy-speed.exe",

    [int[]] $BlockKiB = @(4, 16, 64, 256)
)

$ErrorActionPreference = "Stop"

function New-DeterministicPattern {
    param(
        [Parameter(Mandatory = $true)]
        [int] $Length,

        [Parameter(Mandatory = $true)]
        [byte] $Seed
    )

    $Buffer = [byte[]]::new($Length)

    for ($Index = 0; $Index -lt $Buffer.Length; $Index++) {
        $Buffer[$Index] = [byte](
            (
                [int]$Seed +
                ($Index * 31) +
                (($Index -shr 8) * 17)
            ) -band 0xFF
        )
    }

    return $Buffer
}

function Copy-ExactBytes {
    param(
        [Parameter(Mandatory = $true)]
        [System.IO.Stream] $InputStream,

        [Parameter(Mandatory = $true)]
        [System.IO.Stream] $OutputStream,

        [Parameter(Mandatory = $true)]
        [long] $Count
    )

    $Buffer = [byte[]]::new(1024 * 1024)
    $Remaining = $Count

    while ($Remaining -gt 0) {
        $Requested = [int][Math]::Min(
            [long]$Buffer.Length,
            $Remaining
        )

        $Read = $InputStream.Read(
            $Buffer,
            0,
            $Requested
        )

        if ($Read -eq 0) {
            throw (
                "Input ended with {0} bytes still expected" -f
                $Remaining
            )
        }

        $OutputStream.Write(
            $Buffer,
            0,
            $Read
        )

        $Remaining -= $Read
    }
}

function Write-DeterministicBytes {
    param(
        [Parameter(Mandatory = $true)]
        [System.IO.Stream] $OutputStream,

        [Parameter(Mandatory = $true)]
        [long] $Count,

        [Parameter(Mandatory = $true)]
        [byte] $Seed
    )

    $Buffer = New-DeterministicPattern `
        -Length (64 * 1024) `
        -Seed $Seed

    $Remaining = $Count

    while ($Remaining -gt 0) {
        $WriteLength = [int][Math]::Min(
            [long]$Buffer.Length,
            $Remaining
        )

        $OutputStream.Write(
            $Buffer,
            0,
            $WriteLength
        )

        $Remaining -= $WriteLength
    }
}

function New-MutatedFile {
    param(
        [Parameter(Mandatory = $true)]
        [string] $SourcePath,

        [Parameter(Mandatory = $true)]
        [string] $DestinationPath,

        [Parameter(Mandatory = $true)]
        [long] $PrefixBytes,

        [long] $SkipBytes = 0,

        [long] $InsertBytes = 0,

        [byte] $Seed = 0xA5
    )

    $SourceLength = (
        Get-Item `
            -LiteralPath $SourcePath
    ).Length

    if ($PrefixBytes -lt 0) {
        throw "Prefix length cannot be negative"
    }

    if ($SkipBytes -lt 0) {
        throw "Skip length cannot be negative"
    }

    if ($InsertBytes -lt 0) {
        throw "Insert length cannot be negative"
    }

    if (($PrefixBytes + $SkipBytes) -gt $SourceLength) {
        throw (
            "Mutation range exceeds the source file: " +
            "$DestinationPath"
        )
    }

    $InputStream = [System.IO.File]::OpenRead(
        $SourcePath
    )

    $OutputStream = [System.IO.File]::Create(
        $DestinationPath
    )

    try {
        Copy-ExactBytes `
            -InputStream $InputStream `
            -OutputStream $OutputStream `
            -Count $PrefixBytes

        if ($SkipBytes -gt 0) {
            $InputStream.Seek(
                $SkipBytes,
                [System.IO.SeekOrigin]::Current
            ) | Out-Null
        }

        if ($InsertBytes -gt 0) {
            Write-DeterministicBytes `
                -OutputStream $OutputStream `
                -Count $InsertBytes `
                -Seed $Seed
        }

        $InputStream.CopyTo(
            $OutputStream
        )

        $OutputStream.Flush()
    }
    finally {
        $OutputStream.Dispose()
        $InputStream.Dispose()
    }
}

$BasisSource = (
    Resolve-Path `
        -LiteralPath $BasisPath
).Path

$ExecutablePath = (
    Resolve-Path `
        -LiteralPath $Executable
).Path

$OutputPath = [System.IO.Path]::GetFullPath(
    $OutputRoot
)

$CurrentPath = [System.IO.Path]::GetFullPath(
    "."
).TrimEnd(
    [System.IO.Path]::DirectorySeparatorChar
)

$NormalizedOutput = $OutputPath.TrimEnd(
    [System.IO.Path]::DirectorySeparatorChar
)

if (
    $NormalizedOutput.Equals(
        $CurrentPath,
        [System.StringComparison]::OrdinalIgnoreCase
    )
) {
    throw "Output root must not be the repository root"
}

$OutputPrefix =
    $NormalizedOutput +
    [System.IO.Path]::DirectorySeparatorChar

if (
    $BasisSource.StartsWith(
        $OutputPrefix,
        [System.StringComparison]::OrdinalIgnoreCase
    )
) {
    throw (
        "The basis file must not be inside the output root"
    )
}

$BasisLength = (
    Get-Item `
        -LiteralPath $BasisSource
).Length

$MinimumBasisBytes = 2 * 1024 * 1024

if ($BasisLength -lt $MinimumBasisBytes) {
    throw (
        "The mutation corpus requires a basis file of at " +
        "least 2 MiB"
    )
}

foreach ($Size in $BlockKiB) {
    if ($Size -lt 4) {
        throw "Block sizes must be at least 4 KiB"
    }

    if (($Size -band ($Size - 1)) -ne 0) {
        throw (
            "Block size must be a power of two: $Size KiB"
        )
    }
}

Remove-Item `
    -LiteralPath $OutputPath `
    -Recurse `
    -Force `
    -ErrorAction SilentlyContinue

New-Item `
    -ItemType Directory `
    -Path $OutputPath |
    Out-Null

$BasisCopy = Join-Path `
    $OutputPath `
    "basis.bin"

Copy-Item `
    -LiteralPath $BasisSource `
    -Destination $BasisCopy

$AlignedOffset = 1024 * 1024
$AlignedInsertBytes = 256 * 1024

$UnalignedOffset =
    $AlignedOffset + 123

$UnalignedMutationBytes = 4097

$OverwriteOffset =
    $AlignedOffset + (32 * 1024)

$OverwriteBytes = 4096

$ExactPath = Join-Path `
    $OutputPath `
    "exact.bin"

$OverwritePath = Join-Path `
    $OutputPath `
    "overwrite-4k.bin"

$AlignedInsertPath = Join-Path `
    $OutputPath `
    "insert-aligned-256k.bin"

$UnalignedInsertPath = Join-Path `
    $OutputPath `
    "insert-unaligned-4097.bin"

$UnalignedDeletePath = Join-Path `
    $OutputPath `
    "delete-unaligned-4097.bin"

$AppendPath = Join-Path `
    $OutputPath `
    "append-4097.bin"

Copy-Item `
    -LiteralPath $BasisCopy `
    -Destination $ExactPath

New-MutatedFile `
    -SourcePath $BasisCopy `
    -DestinationPath $OverwritePath `
    -PrefixBytes $OverwriteOffset `
    -SkipBytes $OverwriteBytes `
    -InsertBytes $OverwriteBytes `
    -Seed 0x11

New-MutatedFile `
    -SourcePath $BasisCopy `
    -DestinationPath $AlignedInsertPath `
    -PrefixBytes $AlignedOffset `
    -InsertBytes $AlignedInsertBytes `
    -Seed 0x22

New-MutatedFile `
    -SourcePath $BasisCopy `
    -DestinationPath $UnalignedInsertPath `
    -PrefixBytes $UnalignedOffset `
    -InsertBytes $UnalignedMutationBytes `
    -Seed 0x33

New-MutatedFile `
    -SourcePath $BasisCopy `
    -DestinationPath $UnalignedDeletePath `
    -PrefixBytes $UnalignedOffset `
    -SkipBytes $UnalignedMutationBytes `
    -Seed 0x44

New-MutatedFile `
    -SourcePath $BasisCopy `
    -DestinationPath $AppendPath `
    -PrefixBytes $BasisLength `
    -InsertBytes $UnalignedMutationBytes `
    -Seed 0x55

$Candidates = @(
    [pscustomobject]@{
        Name = "exact"
        Path = $ExactPath
    }

    [pscustomobject]@{
        Name = "overwrite-4k"
        Path = $OverwritePath
    }

    [pscustomobject]@{
        Name = "insert-aligned-256k"
        Path = $AlignedInsertPath
    }

    [pscustomobject]@{
        Name = "insert-unaligned-4097"
        Path = $UnalignedInsertPath
    }

    [pscustomobject]@{
        Name = "delete-unaligned-4097"
        Path = $UnalignedDeletePath
    }

    [pscustomobject]@{
        Name = "append-4097"
        Path = $AppendPath
    }
)

$ResultsPath = Join-Path `
    $OutputPath `
    "fixed-block-results.txt"

@"
NetworkCopy fixed-block dedup mutation matrix
Basis: $BasisCopy
Basis bytes: $BasisLength
Block sizes: $($BlockKiB -join ", ") KiB

"@ | Set-Content `
    -LiteralPath $ResultsPath `
    -Encoding utf8

Write-Host
Write-Host "Dedup mutation corpus created"
Write-Host "  Basis:             $BasisCopy"
Write-Host "  Basis bytes:       $BasisLength"
Write-Host "  Mutation offset:   $UnalignedOffset"
Write-Host "  Results:           $ResultsPath"
Write-Host

foreach ($Candidate in $Candidates) {
    foreach ($Size in $BlockKiB) {
        $Heading = (
            "===== {0} / {1} KiB =====" -f
            $Candidate.Name,
            $Size
        )

        Write-Host $Heading

        Add-Content `
            -LiteralPath $ResultsPath `
            -Value $Heading

        $Output = & $ExecutablePath `
            bench-fixed-dedup `
            $BasisCopy `
            $Candidate.Path `
            $Size `
            2>&1

        $ExitCode = $LASTEXITCODE

        $Output |
            Tee-Object `
                -FilePath $ResultsPath `
                -Append

        Add-Content `
            -LiteralPath $ResultsPath `
            -Value ""

        if ($ExitCode -ne 0) {
            throw (
                "Fixed-block benchmark failed for " +
                "$($Candidate.Name) at $Size KiB"
            )
        }

        Write-Host
    }
}

Write-Host "Mutation matrix complete"
Write-Host "  Results: $ResultsPath"
