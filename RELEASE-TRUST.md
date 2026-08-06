# Release Trust and Antivirus Guidance

NetworkCopy Speed Edition produces native Windows executables. Local
development builds are unsigned by default. Public release builds can
optionally be Authenticode-signed by the release builder.

NetworkCopy does not:

- modify antivirus settings;
- create antivirus exclusions;
- restore itself from quarantine;
- disable scanning;
- embed certificates, private keys, passwords, or signing tokens.

Those actions must remain explicit and under the machine owner's control.

## Optional Authenticode release signing

The release builder can sign all five executables:

- Manager;
- Agent;
- CLI;
- Hungarian GUI;
- English GUI.

Signing occurs after each executable is copied into `dist` and before
`SHA256SUMS.txt` is created.

The signing certificate must already be installed in either:

```text
CurrentUser\My
```

or:

```text
LocalMachine\My
```

The repository stores only the public certificate thumbprint supplied on the
command line. It never reads a PFX file or accepts a certificate password.

Example using a certificate in the current user's personal store:

```powershell
.\scripts\Build-Release.ps1 `
    -TortureRounds 10 `
    -SignCertificateThumbprint "0123456789ABCDEF0123456789ABCDEF01234567" `
    -TimestampUrl "https://timestamp.example.com"
```

Example using the local-machine certificate store:

```powershell
.\scripts\Build-Release.ps1 `
    -TortureRounds 10 `
    -SignCertificateThumbprint "0123456789ABCDEF0123456789ABCDEF01234567" `
    -SignCertificateStore LocalMachine `
    -TimestampUrl "https://timestamp.example.com"
```

When signtool.exe is not available through PATH, provide its absolute
Windows SDK path:

```powershell
-SignToolPath "C:\Program Files (x86)\Windows Kits\10\bin\<version>\x64\signtool.exe"
```

When signing is requested, the builder:

1. validates the certificate thumbprint;
2. requires an HTTPS RFC 3161 timestamp URL;
3. signs every executable with SHA-256;
4. verifies every signature using the ordinary Authenticode application policy;
5. verifies the requested signer thumbprint;
6. requires a timestamp certificate;
7. prints signer and timestamp-authority information;
8. fails before checksums are created if any artifact cannot be verified.

Unsigned development and local release-candidate builds remain supported.

## Narrow local antivirus exclusions

Unsigned Rust executables may trigger heuristic antivirus detections,
especially when they:

- open listening sockets;
- request administrator elevation;
- modify Windows Firewall rules;
- replace or relaunch executables during updater testing.

Do not exclude broad locations such as:

- Desktop
- Downloads
- `C:\`
- the entire user profile

When a local exclusion is genuinely necessary, scope it to disposable build
outputs only:

```text
<repository>\target\release
<repository>\dist
```

A dedicated Cargo output directory can make the exclusion even narrower:

```powershell
$env:CARGO_TARGET_DIR = "$PWD\.networkcopy-build"
cargo build --release
```

The exclusion can then target only:

```text
<repository>\.networkcopy-build
```

Do not distribute anything from an excluded directory without rebuilding or
verifying it first.

## Quarantine recovery

Before restoring a quarantined public release artifact:

1. confirm the exact release version and artifact filename;
2. compare its SHA-256 with the release's `SHA256SUMS.txt`;
3. confirm that the release came from the official project repository;
4. restore only that exact file.

For an unsigned local development build, prefer deleting the quarantined
output, adding a narrowly scoped temporary build exclusion, and rebuilding
from the pushed source instead of trusting the quarantined copy.

## False-positive reports

A useful antivirus false-positive report should include:

- NetworkCopy version;
- exact artifact filename;
- artifact SHA-256;
- antivirus product and version;
- detection name;
- whether the artifact is signed;
- release page;
- the exact binary that triggered the detection.

Submit the original unmodified binary. Repacking or renaming it can produce a
different result and makes the report harder to reproduce.

## Signing-key rules

Never commit:

- PFX or P12 files;
- private keys;
- certificate passwords;
- hardware-token PINs;
- timestamp credentials;
- CI signing secrets.

Trusted public signing requires a suitable code-signing certificate. The
unsigned path remains the normal development workflow until one is available.
