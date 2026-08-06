[CmdletBinding()]
param(
    [switch]$SkipChecks,

    [switch]$AllowDevelopmentVersion,

    [ValidateRange(0, 100)]
    [int]$TortureRounds = 0,

    [string]$SignCertificateThumbprint = "",

    [ValidateSet("CurrentUser", "LocalMachine")]
    [string]$SignCertificateStore = "CurrentUser",

    [string]$TimestampUrl = "",

    [string]$SignToolPath = ""
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

function Normalize-CertificateThumbprint {
    param(
        [Parameter(Mandatory)]
        [string]$Thumbprint
    )

    $normalized = (
        $Thumbprint -replace "\s", ""
    ).ToUpperInvariant()

    if ($normalized -notmatch "^[0-9A-F]{40}$") {
        throw (
            "The signing certificate thumbprint must contain exactly " +
            "40 hexadecimal characters."
        )
    }

    return $normalized
}

function Resolve-SignTool {
    param(
        [string]$RequestedPath
    )

    if (-not [string]::IsNullOrWhiteSpace($RequestedPath)) {
        if (
            -not (
                Test-Path `
                    -LiteralPath $RequestedPath `
                    -PathType Leaf
            )
        ) {
            throw "SignTool was not found at '$RequestedPath'."
        }

        return (Resolve-Path -LiteralPath $RequestedPath).Path
    }

    $command = Get-Command `
        "signtool.exe" `
        -CommandType Application `
        -ErrorAction SilentlyContinue |
        Select-Object -First 1

    if (-not $command) {
        throw (
            "Signing was requested, but signtool.exe was not found. " +
            "Install the Windows SDK or provide -SignToolPath."
        )
    }

    return $command.Source
}

function Invoke-SignTool {
    param(
        [Parameter(Mandatory)]
        [string]$ToolPath,

        [Parameter(Mandatory)]
        [string[]]$Arguments
    )

    Write-Host
    Write-Host (
        "signtool " + ($Arguments -join " ")
    ) -ForegroundColor Cyan

    & $ToolPath @Arguments

    $exitCode = $LASTEXITCODE

    if ($exitCode -ne 0) {
        throw "SignTool failed with exit code $exitCode."
    }
}

function Assert-AuthenticodeSignature {
    param(
        [Parameter(Mandatory)]
        [System.IO.FileInfo]$Artifact,

        [Parameter(Mandatory)]
        [string]$ExpectedThumbprint
    )

    $signature = Get-AuthenticodeSignature `
        -FilePath $Artifact.FullName

    if (
        $signature.Status -ne
        [System.Management.Automation.SignatureStatus]::Valid
    ) {
        throw (
            "Authenticode verification failed for '$($Artifact.Name)': " +
            "$($signature.Status) — $($signature.StatusMessage)"
        )
    }

    $signer = $signature.SignerCertificate

    if (-not $signer) {
        throw (
            "Authenticode verification found no signer certificate for " +
            "'$($Artifact.Name)'."
        )
    }

    $actualThumbprint =
        Normalize-CertificateThumbprint $signer.Thumbprint

    if ($actualThumbprint -ne $ExpectedThumbprint) {
        throw (
            "Artifact '$($Artifact.Name)' was signed by certificate " +
            "$actualThumbprint instead of requested certificate " +
            "$ExpectedThumbprint."
        )
    }

    $timeStamper = $signature.TimeStamperCertificate

    if (-not $timeStamper) {
        throw (
            "Artifact '$($Artifact.Name)' has no verified Authenticode " +
            "timestamp certificate."
        )
    }

    return [pscustomobject]@{
        Artifact           = $Artifact.Name
        Signer             = $signer.Subject
        Thumbprint         = $actualThumbprint
        TimestampAuthority = $timeStamper.Subject
        Status             = $signature.Status
    }
}

function Invoke-AuthenticodeSigning {
    param(
        [Parameter(Mandatory)]
        [System.IO.FileInfo[]]$Artifacts,

        [Parameter(Mandatory)]
        [string]$CertificateThumbprint,

        [Parameter(Mandatory)]
        [ValidateSet("CurrentUser", "LocalMachine")]
        [string]$CertificateStore,

        [Parameter(Mandatory)]
        [string]$TimestampUrl,

        [Parameter(Mandatory)]
        [string]$ToolPath
    )

    $signArguments = @(
        "sign",
        "/sha1",
        $CertificateThumbprint,
        "/s",
        "My"
    )

    if ($CertificateStore -eq "LocalMachine") {
        $signArguments += "/sm"
    }

    $signArguments += @(
        "/fd",
        "SHA256",
        "/tr",
        $TimestampUrl,
        "/td",
        "SHA256",
        "/v"
    )

    foreach ($artifact in $Artifacts) {
        Invoke-SignTool `
            -ToolPath $ToolPath `
            -Arguments (
                $signArguments + @($artifact.FullName)
            )
    }

    Write-Host
    Write-Host (
        "Verifying Authenticode signatures..."
    ) -ForegroundColor Yellow

    $reports = foreach ($artifact in $Artifacts) {
        Invoke-SignTool `
            -ToolPath $ToolPath `
            -Arguments @(
                "verify",
                "/pa",
                "/all",
                "/v",
                $artifact.FullName
            )

        Assert-AuthenticodeSignature `
            -Artifact $artifact `
            -ExpectedThumbprint $CertificateThumbprint
    }

    Write-Host
    Write-Host (
        "Authenticode verification complete."
    ) -ForegroundColor Green

    $reports |
        Format-Table `
            Artifact,
            Signer,
            Thumbprint,
            TimestampAuthority,
            Status `
            -AutoSize
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

$signingRequested =
    -not [string]::IsNullOrWhiteSpace(
        $SignCertificateThumbprint
    )

$normalizedSigningThumbprint = $null
$resolvedSignTool = $null

if ($signingRequested) {
    $normalizedSigningThumbprint =
        Normalize-CertificateThumbprint `
            $SignCertificateThumbprint

    $timestampUri = $null

    $timestampValid = [Uri]::TryCreate(
        $TimestampUrl,
        [UriKind]::Absolute,
        [ref]$timestampUri
    )

    if (
        -not $timestampValid -or
        $timestampUri.Scheme -ne [Uri]::UriSchemeHttps
    ) {
        throw (
            "Signing requires an absolute HTTPS RFC 3161 timestamp URL."
        )
    }

    $resolvedSignTool =
        Resolve-SignTool $SignToolPath
}
elseif (
    -not [string]::IsNullOrWhiteSpace($TimestampUrl) -or
    -not [string]::IsNullOrWhiteSpace($SignToolPath) -or
    $SignCertificateStore -ne "CurrentUser"
) {
    throw (
        "Signing options were supplied without " +
        "-SignCertificateThumbprint."
    )
}

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

    if ($signingRequested) {
        Write-Host (
            "Signing:    certificate " +
            "$normalizedSigningThumbprint " +
            "from $SignCertificateStore\My"
        )

        Write-Host "Timestamp:  $TimestampUrl"
        Write-Host "SignTool:   $resolvedSignTool"
    }
    else {
        Write-Host "Signing:    disabled"
    }

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

    if ($signingRequested) {
        Write-Host
        Write-Host (
            "Signing all release executables..."
        ) -ForegroundColor Yellow

        Invoke-AuthenticodeSigning `
            -Artifacts $artifacts `
            -CertificateThumbprint $normalizedSigningThumbprint `
            -CertificateStore $SignCertificateStore `
            -TimestampUrl $TimestampUrl `
            -ToolPath $resolvedSignTool
    }
    else {
        Write-Host
        Write-Warning (
            "Release executables are unsigned. " +
            "Unsigned development builds remain supported, but public " +
            "release artifacts should be signed when a trusted " +
            "code-signing certificate is available."
        )
    }

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