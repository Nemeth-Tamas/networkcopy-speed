param(
    [string]$OutputRoot = (
        Join-Path `
            $PSScriptRoot `
            "..\TinyPackData"
    ),

    [int]$FileCount = 5000,

    [int]$RandomBytesPerFile = 192
)

$ErrorActionPreference = "Stop"

$OutputRoot = [IO.Path]::GetFullPath(
    $OutputRoot
)

$compressible = Join-Path `
    $OutputRoot `
    "compressible\source"

$incompressible = Join-Path `
    $OutputRoot `
    "incompressible\source"

Remove-Item `
    $OutputRoot `
    -Recurse `
    -Force `
    -ErrorAction SilentlyContinue

New-Item `
    -ItemType Directory `
    -Path $compressible `
    -Force |
    Out-Null

New-Item `
    -ItemType Directory `
    -Path $incompressible `
    -Force |
    Out-Null

$utf8 = [Text.UTF8Encoding]::new(
    $false
)

for (
    $index = 1;
    $index -le $FileCount;
    $index++
) {
    $contents = @"
{
  "record": $index,
  "application": "NetworkCopy Speed Edition",
  "status": "ready",
  "category": "tiny-pack-benchmark",
  "message": "Repeated structured text should compress extremely well."
}
"@

    $path = Join-Path `
        $compressible `
        ("file-{0:D5}.json" -f $index)

    [IO.File]::WriteAllText(
        $path,
        $contents,
        $utf8
    )
}

$rng = [Security.Cryptography.RandomNumberGenerator]::Create()

try {
    for (
        $index = 1;
        $index -le $FileCount;
        $index++
    ) {
        $contents = New-Object `
            byte[] `
            $RandomBytesPerFile

        $rng.GetBytes(
            $contents
        )

        $path = Join-Path `
            $incompressible `
            ("file-{0:D5}.bin" -f $index)

        [IO.File]::WriteAllBytes(
            $path,
            $contents
        )
    }
}
finally {
    $rng.Dispose()
}

Write-Host
Write-Host "Tiny-pack benchmark datasets created"
Write-Host "  Compressible:   $compressible"
Write-Host "  Incompressible: $incompressible"
Write-Host "  Files each:     $FileCount"
