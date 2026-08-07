$ErrorActionPreference = "Stop"

$Path = Join-Path $PSScriptRoot "README.md"

if (-not (Test-Path -LiteralPath $Path)) {
    throw "Run this file from the repository root. Missing: $Path"
}

$text = [System.IO.File]::ReadAllText($Path)

function Replace-ExactlyOnce {
    param(
        [string]$Label,
        [string]$Old,
        [string]$New
    )

    $first = $text.IndexOf($Old, [System.StringComparison]::Ordinal)

    if ($first -lt 0) {
        throw "Could not find expected block for: $Label"
    }

    $second = $text.IndexOf(
        $Old,
        $first + $Old.Length,
        [System.StringComparison]::Ordinal
    )

    if ($second -ge 0) {
        throw "Expected exactly one block for '$Label', but found more than one."
    }

    $script:text = $text.Substring(0, $first) + $New + $text.Substring($first + $Old.Length)
}

Replace-ExactlyOnce `
    "development status" `
    @'
Current stable release and source version:

```text
2.5.0
```
'@ `
    @'
## Current status

Current stable release:

```text
2.5.0
```

Current development version:

```text
2.6.0-dev
```
'@

Replace-ExactlyOnce `
    "remove accidental editorial note" `
    @'
The detailed v2.4 history can stay farther down in the README.
'@ `
    @'
Release packaging supports optional certificate-store Authenticode signing,
RFC 3161 SHA-256 timestamping, signature verification before checksums, and
unsigned local development builds. See
[Release Trust and Antivirus Guidance](RELEASE-TRUST.md).
'@

Replace-ExactlyOnce `
    "mark stage profiler complete" `
    @'
- [ ] add stage-level timing for source reads, compression/probing, blocked
      socket writes, socket reads, decompression, and destination writes;
'@ `
    @'
- [x] add aggregate stage profiling for ordinary, striped, and tiny-pack payload
      source reads, compression/probing, underlying socket writes, underlying
      socket reads, decompression, and destination writes;
- [ ] extend source/destination disk-stage attribution through CDC and exact-reuse
      paths before using profiler results to tune those specialized paths;
'@

Replace-ExactlyOnce `
    "document aggregate-worker semantics" `
    @'
Direct Link is always a physical Ethernet candidate. Automatic LAN and Explicit
IP may use Ethernet, Wi-Fi, VPN, or virtual interfaces, so the v2.6 policy must
inspect the selected path rather than treating every LAN address as wired.
'@ `
    @'
Stage timing is reported as aggregate worker time together with processed bytes
and operation counts. Because transfer lanes operate concurrently, summed stage
time can legitimately exceed wall-clock transfer duration. The profiler is
intended to identify where concurrent workers spend time rather than to present
a serial percentage breakdown.

Direct Link is always a physical Ethernet candidate. Automatic LAN and Explicit
IP may use Ethernet, Wi-Fi, VPN, or virtual interfaces, so the v2.6 policy must
inspect the selected path rather than treating every LAN address as wired.
'@

[System.IO.File]::WriteAllText(
    $Path,
    $text,
    [System.Text.UTF8Encoding]::new($false)
)

Write-Host "Updated README.md"