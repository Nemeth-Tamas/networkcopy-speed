<p align="center">
  <img
    src="assets/networkcopy-logo.png"
    alt="NetworkCopy Speed Edition"
    width="900"
  >
</p>

# NetworkCopy Speed Edition

A high-performance Windows folder-transfer tool written in Rust.

NetworkCopy can transfer folders across a normal local network or directly
between two computers connected by an Ethernet cable. Direct Link Mode does
not require a router, switch, DHCP server, or manually assigned IP addresses.

The repository currently contains:

- the released v1.1 command-line transfer engine;
- the v1.2 desktop GUI under active development;
- one shared networking and transfer implementation used by both front ends.

## Current status

Current release:

```text
1.2.0
```

v1.2 is the current stable release.

The GUI includes:

- Hungarian default language;
- English built into the same executable;
- runtime language switching;
- Direct Link and manual IP-address modes;
- Send and Receive operation selection;
- native Windows folder pickers;
- live phase, byte, percentage, and throughput progress;
- immediate cooperative cancellation;
- persistent interrupted-transfer records;
- one-click transfer restart and stripe resume;
- automatic administrator elevation when Receive requires firewall setup;
- success, cancellation, and failure summaries.

## Quick start — graphical interface

Run:

```powershell
networkcopy-gui.exe
```

### Direct Ethernet cable

Connect the two Windows computers with an Ethernet cable.

On the receiving computer:

1. Open **Fogadás / Receive**.
2. Select **Közvetlen kábel / Direct cable**.
3. Choose the destination folder.
4. Press **Indítás / Start**.
5. Approve administrator elevation when Windows requests it.

On the sending computer:

1. Open **Küldés / Send**.
2. Select **Közvetlen kábel / Direct cable**.
3. Choose the source folder.
4. Press **Indítás / Start**.

NetworkCopy automatically:

- identifies dedicated Ethernet interfaces;
- rejects interfaces carrying a default route;
- discovers the receiver over scoped IPv6 link-local multicast;
- falls back to IPv4 APIPA when IPv6 is unavailable;
- binds discovery, calibration, control, and transfer sockets to the selected
  interface;
- measures the path with 1, 2, 4, and 8 TCP streams;
- chooses the smallest stream count reaching at least 90% of the best measured
  result;
- starts the folder transfer.

### Existing LAN or manual address

Select **IP-cím / IP address**.

The receiver enters a local listening address, for example:

```text
0.0.0.0:7337
```

The sender enters the receiver address, for example:

```text
192.168.1.50:7337
```

Loopback development and local testing can use:

```text
127.0.0.1:7337
```

## Cancellation and resume

The GUI's Cancel button interrupts:

- Direct Link discovery;
- receiver waits;
- calibration;
- scanning checkpoints;
- file payload transfer;
- large-file stripe processing.

NetworkCopy stores a tiny transfer-request record beside the executable when
possible. When that directory is not writable, it falls back to:

```text
%LOCALAPPDATA%\NetworkCopy Speed Edition
```

After an interrupted operation, the next launch offers to restore the previous
settings and restart the operation.

The destination-side resume journal remains authoritative for completed
large-file stripes. Restarting the same operation allows those completed
stripes to be skipped.

A successful transfer removes the corresponding GUI session record.

## Transfer engine

The GUI and CLI call the same Rust library. There is no second networking
implementation and the GUI does not launch or scrape the CLI.

The transfer engine includes:

- parallel directory scanning;
- UTF-16 Windows path support;
- one control connection plus calibrated TCP data lanes;
- tiny-file packing;
- large-file striping;
- resumable stripe journals;
- adaptive Zstandard compression;
- BLAKE3 integrity verification;
- sparse-file and metadata handling;
- bounded reusable transfer buffers;
- process-wide memory budgeting;
- explicit source-interface binding for Direct Link Mode.

## Calibration policy

Before a folder transfer, NetworkCopy measures:

```text
1, 2, 4, and 8 TCP streams
```

The selected count is the smallest number of streams that reaches at least
90% of the best measured throughput.

This avoids using eight connections when fewer lanes already saturate the
available path.

## Validated transfer

The v1.1 Direct Link acceptance dataset contained:

```text
2,181 files
6,807,145,167 logical bytes
```

Validation covered:

- scoped IPv6 link-local discovery;
- IPv4 APIPA fallback with IPv6 disabled;
- explicit local source binding;
- multistream calibration;
- compression;
- tiny-file packing;
- BLAKE3 verification;
- cable interruption;
- restart and completed-stripe resume.

One IPv4 APIPA validation run completed at:

```text
282.55 MB/s logical payload throughput
30.35% application-wire savings
```

These numbers describe that specific VMware test environment and dataset.
Actual speed depends on storage, CPU, adapters, drivers, cabling, and the
compressibility of the transferred data.

## Network ports

NetworkCopy uses:

| Protocol | Port | Purpose |
|---|---:|---|
| UDP | 7336 | Direct Link discovery |
| TCP | 7337 | Calibration, control, and file transfer |

The GUI receiver configures the required inbound Windows Firewall rules and
automatically relaunches with administrator privileges when necessary.

## Command-line interface

Direct Link receiver:

```powershell
networkcopy-speed.exe direct-receive "C:\Destination"
```

Direct Link sender:

```powershell
networkcopy-speed.exe direct-send "C:\Source" 4 64
```

Sender arguments:

```text
direct-send <source> [scanner-workers] [calibration-mib]
```

Use the built-in help for benchmark, diagnostic, explicit-address, and
lower-level commands:

```powershell
networkcopy-speed.exe --help
```

## Building

Requirements:

- Windows 11 or a supported Windows 10 installation;
- stable Rust toolchain;
- MSVC Rust target and Visual Studio C++ build tools.

Debug GUI:

```powershell
cargo run --bin networkcopy-gui
```

Release GUI with Hungarian as the initial language:

```powershell
cargo build `
    --release `
    --bin networkcopy-gui
```

Release GUI with English as the initial language:

```powershell
cargo build `
    --release `
    --bin networkcopy-gui `
    --features default-language-en
```

Both variants contain both languages. The feature only changes which language
is selected at startup.

Release CLI:

```powershell
cargo build `
    --release `
    --bin networkcopy-speed
```

## Release naming

The v1.2 release assets will be executable-only:

```text
networkcopy-speed-hu.exe
networkcopy-speed-en.exe
```

No installer or ZIP package is required.

## Quality gate

Before a release:

```powershell
cargo fmt --check

cargo test

cargo clippy `
    --all-targets `
    --all-features `
    -- `
    -D warnings

cargo build --release
```

## Project structure

```text
src/
├── bin/
│   └── networkcopy-gui.rs
├── lib.rs
├── cli_main.rs
├── gui_transfer.rs
├── gui_session.rs
├── windows_elevation.rs
├── direct_discovery.rs
├── direct_discovery_v4.rs
├── direct_transfer.rs
├── calibrated_transfer.rs
├── multistream_copy.rs
├── manifest_scan.rs
├── adaptive_compression.rs
├── resume_state.rs
└── ...
```

Important architectural boundary:

```text
GUI ─┐
     ├── shared transfer library ── networking and storage engine
CLI ─┘
```

## Scope

NetworkCopy is currently Windows-only and optimized for trusted local
machine-to-machine transfers.

The project intentionally focuses on:

- local Ethernet and LAN paths;
- maximum practical throughput;
- restartable transfers;
- correctness and integrity;
- a portable standalone executable.

See `LICENSE` for licensing terms.
