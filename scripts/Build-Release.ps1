[CmdletBinding()]
param(
    [switch]$SkipChecks,

    [switch]$AllowDevelopmentVersion,

    [ValidateRange(0, 100)]
    [int]$TortureRounds = 0
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

function Invoke-Cargo {
    param(
        [Parameter(Mandatory)]
        [string[]]$Arguments
    )

    Write-Host
    Write-Host ("cargo " + ($Arguments -join " ")) -ForegroundColor Cyan

    & cargo @Arguments

    if ($LASTEXITCODE -ne 0) {
        throw "Cargo command failed with exit code $LASTEXITCODE."
    }
}

function Resolve-RepositoryRoot {
    $candidates = @(
        $PSScriptRoot,
        (Split-Path -Parent $PSScriptRoot)
    )

    foreach ($candidate in $candidates) {
        if (
            $candidate -and
            (Test-Path -LiteralPath (Join-Path $candidate "Cargo.toml"))
        ) {
            return (Resolve-Path -LiteralPath $candidate).Path
        }
    }

    throw "Could not find Cargo.toml beside this script or in its parent directory."
}

$repoRoot = Resolve-RepositoryRoot
$distRoot = Join-Path $repoRoot "dist"
$releaseRoot = Join-Path $repoRoot "target\release"

$hadPreviousTortureRounds =
    Test-Path Env:NETWORKCOPY_TORTURE_ROUNDS

$previousTortureRounds =
    $env:NETWORKCOPY_TORTURE_ROUNDS

Push-Location $repoRoot

try {
    Write-Host "NetworkCopy Speed Edition release builder" -ForegroundColor Green
    Write-Host "Repository: $repoRoot"

    $metadataJson = & cargo metadata --no-deps --format-version 1

    if ($LASTEXITCODE -ne 0) {
        throw "cargo metadata failed with exit code $LASTEXITCODE."
    }

    $metadata = $metadataJson | ConvertFrom-Json
    $package = $metadata.packages |
        Where-Object { $_.name -eq "networkcopy-speed" } |
        Select-Object -First 1

    if (-not $package) {
        throw "Could not find the networkcopy-speed package in cargo metadata."
    }

    $version = [string]$package.version

    if (
        -not $AllowDevelopmentVersion -and
        $version -match "-"
    ) {
        throw (
            "Refusing to create release artifacts for development version " +
            "'$version'. Set Cargo.toml to the stable version or run with " +
            "-AllowDevelopmentVersion."
        )
    }

    Write-Host "Version:    $version"

    if (-not $SkipChecks) {
        Invoke-Cargo @(
            "fmt",
            "--all",
            "--",
            "--check"
        )

        Invoke-Cargo @(
            "test",
            "--locked"
        )

        Invoke-Cargo @(
            "clippy",
            "--locked",
            "--all-targets",
            "--all-features",
            "--",
            "-D",
            "warnings"
        )

        if ($TortureRounds -gt 0) {
            $env:NETWORKCOPY_TORTURE_ROUNDS =
                [string]$TortureRounds

            Write-Host
            Write-Host (
                "Running recovery torture: " +
                "$TortureRounds round(s) per matrix"
            ) -ForegroundColor Yellow

            Invoke-Cargo @(
                "test",
                "--locked",
                "--release",
                "torture_matrix",
                "--",
                "--ignored",
                "--nocapture",
                "--test-threads=1"
            )
        }
        else {
            Write-Host
            Write-Host (
                "Recovery torture skipped. " +
                "Use -TortureRounds N to enable it."
            ) -ForegroundColor DarkYellow
        }
    }
    else {
        Write-Warning (
            "Formatting, tests, Clippy, and recovery " +
            "torture were skipped."
        )
    }

    if (Test-Path -LiteralPath $distRoot) {
        Write-Host
        Write-Host "Removing existing dist directory..."
        Remove-Item -LiteralPath $distRoot -Recurse -Force
    }

    New-Item -ItemType Directory -Path $distRoot -Force | Out-Null

    $cliName = "NetworkCopy-Speed-v$version-CLI-Windows-x64.exe"
    $guiHuName = "NetworkCopy-Speed-v$version-GUI-HU-Windows-x64.exe"
    $guiEnName = "NetworkCopy-Speed-v$version-GUI-EN-Windows-x64.exe"
    $managerName =
        "NetworkCopy-Speed-v$version-Manager-Windows-x64.exe"
    $agentName =
        "NetworkCopy-Speed-v$version-Agent-Windows-x64.exe"

    $cliOutput = Join-Path $distRoot $cliName
    $guiHuOutput = Join-Path $distRoot $guiHuName
    $guiEnOutput = Join-Path $distRoot $guiEnName
    $managerOutput =
        Join-Path $distRoot $managerName
    $agentOutput =
        Join-Path $distRoot $agentName

    Invoke-Cargo @(
        "build",
        "--locked",
        "--release",
        "--bin",
        "networkcopy-speed"
    )

    $builtCli = Join-Path $releaseRoot "networkcopy-speed.exe"

    if (-not (Test-Path -LiteralPath $builtCli)) {
        throw "CLI build completed, but $builtCli was not found."
    }

    Copy-Item -LiteralPath $builtCli -Destination $cliOutput -Force

    Write-Host
    Write-Host "Building dedicated endpoint agent..." -ForegroundColor Yellow

    Invoke-Cargo @(
        "build",
        "--locked",
        "--release",
        "--bin",
        "networkcopy-agent"
    )

    $builtAgent =
        Join-Path $releaseRoot "networkcopy-agent.exe"

    if (-not (Test-Path -LiteralPath $builtAgent)) {
        throw "Agent build completed, but $builtAgent was not found."
    }

    Copy-Item `
        -LiteralPath $builtAgent `
        -Destination $agentOutput `
        -Force

    Write-Host
    Write-Host "Building management application..." -ForegroundColor Yellow

    Invoke-Cargo @(
        "build",
        "--locked",
        "--release",
        "--bin",
        "networkcopy-manager"
    )

    $builtManager =
        Join-Path $releaseRoot "networkcopy-manager.exe"

    if (-not (Test-Path -LiteralPath $builtManager)) {
        throw "Manager build completed, but $builtManager was not found."
    }

    Copy-Item `
        -LiteralPath $builtManager `
        -Destination $managerOutput `
        -Force

    Write-Host
    Write-Host "Building Hungarian-default GUI..." -ForegroundColor Yellow

    Invoke-Cargo @(
        "build",
        "--locked",
        "--release",
        "--bin",
        "networkcopy-gui",
        "--no-default-features"
    )

    $builtGui = Join-Path $releaseRoot "networkcopy-gui.exe"

    if (-not (Test-Path -LiteralPath $builtGui)) {
        throw "GUI build completed, but $builtGui was not found."
    }

    Copy-Item -LiteralPath $builtGui -Destination $guiHuOutput -Force

    Write-Host
    Write-Host "Building English-default GUI..." -ForegroundColor Yellow

    Invoke-Cargo @(
        "build",
        "--locked",
        "--release",
        "--bin",
        "networkcopy-gui",
        "--no-default-features",
        "--features",
        "default-language-en"
    )

    if (-not (Test-Path -LiteralPath $builtGui)) {
        throw "English GUI build completed, but $builtGui was not found."
    }

    Copy-Item -LiteralPath $builtGui -Destination $guiEnOutput -Force

    $versionOutput = & $cliOutput version

    if ($LASTEXITCODE -ne 0) {
        throw "The built CLI failed its version smoke test."
    }

    $expectedVersionOutput = "NetworkCopy Speed Edition $version"

    if (($versionOutput | Out-String).Trim() -ne $expectedVersionOutput) {
        throw (
            "Unexpected CLI version output. Expected '$expectedVersionOutput', " +
            "received '$(($versionOutput | Out-String).Trim())'."
        )
    }

    $artifacts = @(
        Get-Item -LiteralPath $managerOutput
        Get-Item -LiteralPath $agentOutput
        Get-Item -LiteralPath $guiHuOutput
        Get-Item -LiteralPath $guiEnOutput
        Get-Item -LiteralPath $cliOutput
    )

    $checksumPath = Join-Path $distRoot "SHA256SUMS.txt"

    $checksumLines = foreach ($artifact in $artifacts) {
        $hash = (
            Get-FileHash -LiteralPath $artifact.FullName -Algorithm SHA256
        ).Hash

        "$hash  $($artifact.Name)"
    }

    $checksumLines |
        Set-Content -LiteralPath $checksumPath -Encoding ASCII

    Write-Host
    Write-Host "Release artifacts created successfully." -ForegroundColor Green
    Write-Host

    $artifacts |
        Select-Object Name, Length |
        Format-Table -AutoSize

    Write-Host "Checksums:"
    Get-Content -LiteralPath $checksumPath |
        ForEach-Object { Write-Host "  $_" }

    Write-Host
    Write-Host "Output directory:"
    Write-Host "  $distRoot"
}
finally {
    if ($hadPreviousTortureRounds) {
        $env:NETWORKCOPY_TORTURE_ROUNDS =
            $previousTortureRounds
    }
    else {
        Remove-Item `
            Env:NETWORKCOPY_TORTURE_ROUNDS `
            -ErrorAction SilentlyContinue
    }

    Pop-Location
}