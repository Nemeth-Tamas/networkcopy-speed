[CmdletBinding()]
param(
    [ValidateRange(1, 100)]
    [int]$Rounds = 10
)

$ErrorActionPreference = "Stop"

$RepositoryRoot = Split-Path -Parent $PSScriptRoot
$HadPreviousRounds = Test-Path Env:NETWORKCOPY_TORTURE_ROUNDS
$PreviousRounds = $env:NETWORKCOPY_TORTURE_ROUNDS

Push-Location $RepositoryRoot

try {
    Get-Command cargo -ErrorAction Stop | Out-Null

    $env:NETWORKCOPY_TORTURE_ROUNDS = [string]$Rounds

    Write-Host ""
    Write-Host "NetworkCopy v2.1 recovery torture"
    Write-Host "Rounds per matrix: $Rounds"
    Write-Host "Mode: release, ignored tests, single test thread"
    Write-Host ""

    $CargoArguments = @(
        "test"
        "--release"
        "torture_matrix"
        "--"
        "--ignored"
        "--nocapture"
        "--test-threads=1"
    )

    & cargo @CargoArguments

    if ($LASTEXITCODE -ne 0) {
        throw "NetworkCopy recovery torture failed with exit code $LASTEXITCODE."
    }

    Write-Host ""
    Write-Host "All NetworkCopy v2.1 recovery torture matrices passed."
}
finally {
    if ($HadPreviousRounds) {
        $env:NETWORKCOPY_TORTURE_ROUNDS = $PreviousRounds
    }
    else {
        Remove-Item Env:NETWORKCOPY_TORTURE_ROUNDS `
            -ErrorAction SilentlyContinue
    }

    Pop-Location
}
