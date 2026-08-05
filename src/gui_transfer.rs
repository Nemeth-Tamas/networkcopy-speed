use crate::calibrated_transfer::{self, CalibratedReceiveReport, CalibratedSendReport};
use crate::console_progress::{ProgressCounter, ProgressSnapshot};
use crate::destination_layout::DestinationLayout;
use crate::direct_address::DIRECT_TRANSFER_PORT;
use crate::direct_discovery;
use crate::direct_transfer;
use crate::manifest_scan;
use crate::multistream_copy::DestinationMode;
use crate::network_calibration;
use crate::windows_setup;
use std::io;
use std::net::{Ipv6Addr, SocketAddr, SocketAddrV6, TcpListener};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuiConnectionMode {
    Direct,
    Address(SocketAddr),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GuiTransferRequest {
    Send {
        connection: GuiConnectionMode,

        source_root: PathBuf,

        worker_count: usize,

        calibration_mib: u64,
    },

    Receive {
        connection: GuiConnectionMode,

        destination_root: PathBuf,

        destination_layout: DestinationLayout,

        update_existing: bool,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuiTransferDirection {
    Send,
    Receive,
}

#[derive(Clone, Debug)]
pub struct GuiTransferControl {
    progress: ProgressCounter,
}

#[derive(Clone, Debug)]
pub struct GuiTransferProgress {
    pub phase: String,
    pub completed: u64,
    pub total: u64,
    pub cancel_requested: bool,
}

impl GuiTransferControl {
    pub fn new() -> Self {
        Self {
            progress: ProgressCounter::new("Preparing transfer", 0),
        }
    }

    pub fn cancel(&self) {
        self.progress.cancel();
    }

    pub fn progress(&self) -> GuiTransferProgress {
        let ProgressSnapshot {
            label,
            completed,
            total,
        } = self.progress.snapshot();

        GuiTransferProgress {
            phase: label,
            completed,
            total,
            cancel_requested: self.progress.is_cancelled(),
        }
    }
}

impl Default for GuiTransferControl {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuiTransferDiagnostic {
    AllFilesSkipped,
    TinyFileHeavy,
    ExactReuseEffective,
    CdcEffective,
    CompressionEffective,
    CompressionBypassed,
    Balanced,
}

#[derive(Clone, Copy, Debug)]
struct DiagnosticInput {
    files: u64,
    logical_bytes: u64,
    wire_bytes: u64,

    exact_reused_files: u64,

    cdc_files: u64,
    cdc_reused_bytes: u64,

    compressed_records: u64,
    skipped_files: u64,
    skipped_bytes: u64,
    tiny_files_packed: u64,
    raw_tiny_pack_count: u64,
}

#[derive(Clone, Debug)]
pub struct GuiTransferSummary {
    pub direction: GuiTransferDirection,

    pub files: u64,

    pub logical_bytes: u64,

    pub wire_bytes: u64,

    pub compressed_records: u64,

    pub resumed_stripes: u64,

    pub resumed_bytes: u64,

    pub skipped_files: u64,

    pub skipped_bytes: u64,

    pub data_stream_count: usize,

    pub tiny_materialization_workers: usize,

    pub elapsed: Duration,

    pub logical_megabytes_per_second: f64,

    pub wire_megabytes_per_second: f64,

    pub wire_savings_percent: f64,

    pub exact_reused_files: u64,
    pub exact_reused_bytes: u64,
    pub exact_reuse_plan_wire_bytes: u64,
    pub exact_reuse_wire_savings_percent: f64,

    pub cdc_offered_files: u64,
    pub cdc_files: u64,
    pub cdc_fallback_files: u64,

    pub cdc_logical_bytes: u64,
    pub cdc_reused_bytes: u64,
    pub cdc_literal_bytes: u64,

    pub cdc_index_wire_bytes: u64,
    pub cdc_plan_wire_bytes: u64,
    pub cdc_wire_bytes: u64,
    pub cdc_wire_savings_percent: f64,

    pub tiny_pack_count: u64,

    pub compressed_tiny_pack_count: u64,

    pub raw_tiny_pack_count: u64,

    pub tiny_files_packed: u64,

    pub tiny_bytes_packed: u64,

    pub tiny_pack_wire_bytes: u64,

    pub tiny_pack_wire_savings_percent: f64,
}

impl GuiTransferSummary {
    pub fn diagnostic(&self) -> GuiTransferDiagnostic {
        diagnose_transfer(DiagnosticInput {
            files: self.files,

            logical_bytes: self.logical_bytes,

            wire_bytes: self.wire_bytes,

            exact_reused_files: self.exact_reused_files,

            cdc_files: self.cdc_files,
            cdc_reused_bytes: self.cdc_reused_bytes,

            compressed_records: self.compressed_records,

            skipped_files: self.skipped_files,

            skipped_bytes: self.skipped_bytes,

            tiny_files_packed: self.tiny_files_packed,

            raw_tiny_pack_count: self.raw_tiny_pack_count,
        })
    }
}

fn diagnose_transfer(input: DiagnosticInput) -> GuiTransferDiagnostic {
    let transferred_files = input.files.saturating_sub(input.skipped_files);

    let transferred_bytes = input.logical_bytes.saturating_sub(input.skipped_bytes);

    if transferred_files == 0 || transferred_bytes == 0 {
        return GuiTransferDiagnostic::AllFilesSkipped;
    }

    let average_file_bytes = transferred_bytes / transferred_files;

    if input.tiny_files_packed >= 1_000 && average_file_bytes <= 64 * 1024 {
        return GuiTransferDiagnostic::TinyFileHeavy;
    }

    if input.exact_reused_files > 0 {
        return GuiTransferDiagnostic::ExactReuseEffective;
    }

    if input.cdc_files > 0 && input.cdc_reused_bytes > 0 {
        return GuiTransferDiagnostic::CdcEffective;
    }

    let wire_savings_percent = 100.0 - input.wire_bytes as f64 / transferred_bytes as f64 * 100.0;

    if input.compressed_records > 0 && wire_savings_percent >= 5.0 {
        return GuiTransferDiagnostic::CompressionEffective;
    }

    if input.raw_tiny_pack_count > 0 && input.compressed_records == 0 {
        return GuiTransferDiagnostic::CompressionBypassed;
    }

    GuiTransferDiagnostic::Balanced
}

pub fn run_gui_transfer(request: GuiTransferRequest) -> io::Result<GuiTransferSummary> {
    run_gui_transfer_with_control(request, GuiTransferControl::new())
}

pub fn run_gui_transfer_with_control(
    request: GuiTransferRequest,
    control: GuiTransferControl,
) -> io::Result<GuiTransferSummary> {
    control.progress.check_cancelled()?;

    match request {
        GuiTransferRequest::Send {
            connection,
            source_root,
            worker_count,
            calibration_mib,
        } => {
            manifest_scan::validate_worker_count(worker_count)?;

            let calibration_bytes = network_calibration::bytes_from_mib(calibration_mib)?;

            let report = match connection {
                GuiConnectionMode::Direct => direct_transfer::send_with_progress(
                    &source_root,
                    worker_count,
                    calibration_bytes,
                    control.progress.clone(),
                )?,

                GuiConnectionMode::Address(receiver_address) => {
                    calibrated_transfer::send_with_progress(
                        receiver_address,
                        &source_root,
                        worker_count,
                        calibration_bytes,
                        control.progress.clone(),
                    )?
                }
            };

            Ok(send_summary(report))
        }

        GuiTransferRequest::Receive {
            connection,
            destination_root,
            destination_layout,
            update_existing,
        } => {
            let destination_mode = if update_existing {
                DestinationMode::UpdateVerified
            } else {
                DestinationMode::Fresh
            };

            let report = match connection {
                GuiConnectionMode::Direct => {
                    prepare_direct_receiver()?;

                    match destination_layout {
                        DestinationLayout::Exact => {
                            direct_transfer::receive_once_with_progress_and_mode(
                                &destination_root,
                                control.progress.clone(),
                                destination_mode,
                            )
                        }

                        DestinationLayout::SourceNameUnderRoot => {
                            direct_transfer::receive_once_with_progress_mode_and_layout(
                                &destination_root,
                                control.progress.clone(),
                                destination_mode,
                                destination_layout,
                            )
                        }
                    }
                }

                GuiConnectionMode::Address(bind_address) => {
                    prepare_address_receiver(bind_address)?;

                    let listener = TcpListener::bind(bind_address)?;

                    match destination_layout {
                        DestinationLayout::Exact => {
                            calibrated_transfer::receive_once_with_progress_and_mode(
                                listener,
                                &destination_root,
                                control.progress.clone(),
                                destination_mode,
                            )
                        }

                        DestinationLayout::SourceNameUnderRoot => {
                            calibrated_transfer::receive_once_with_progress_mode_and_layout(
                                listener,
                                &destination_root,
                                control.progress.clone(),
                                destination_mode,
                                destination_layout,
                            )
                        }
                    }
                }
            }?;

            Ok(receive_summary(report))
        }
    }
}

fn prepare_direct_receiver() -> io::Result<()> {
    let firewall_address = SocketAddr::V6(SocketAddrV6::new(
        Ipv6Addr::UNSPECIFIED,
        DIRECT_TRANSFER_PORT,
        0,
        0,
    ));

    windows_setup::prepare_receiver(firewall_address)?;

    windows_setup::prepare_discovery_receiver(direct_discovery::DISCOVERY_PORT)
}

fn prepare_address_receiver(bind_address: SocketAddr) -> io::Result<()> {
    if bind_address.ip().is_loopback() {
        return Ok(());
    }

    windows_setup::prepare_receiver(bind_address)
}

fn send_summary(report: CalibratedSendReport) -> GuiTransferSummary {
    let logical_megabytes_per_second = report.logical_megabytes_per_second;

    let wire_megabytes_per_second = report.wire_megabytes_per_second;

    let transfer = report.transfer;

    let cdc_wire_bytes = transfer
        .cdc_index_wire_bytes
        .saturating_add(transfer.cdc_plan_wire_bytes);

    GuiTransferSummary {
        direction: GuiTransferDirection::Send,

        files: transfer.files_copied,

        logical_bytes: transfer.bytes_copied,

        wire_bytes: transfer.data_wire_bytes,

        compressed_records: transfer.compressed_records,

        resumed_stripes: transfer.resumed_stripes,

        resumed_bytes: transfer.resumed_bytes,

        skipped_files: transfer.skipped_files,

        skipped_bytes: transfer.skipped_bytes,

        data_stream_count: transfer.data_stream_count,

        tiny_materialization_workers: transfer.tiny_materialization_workers,

        elapsed: transfer.data_elapsed,

        logical_megabytes_per_second,

        wire_megabytes_per_second,

        wire_savings_percent: wire_savings_percent(transfer.bytes_copied, transfer.data_wire_bytes),

        exact_reused_files: transfer.exact_reused_files,

        exact_reused_bytes: transfer.exact_reused_bytes,

        exact_reuse_plan_wire_bytes: transfer.exact_reuse_plan_wire_bytes,

        exact_reuse_wire_savings_percent: wire_savings_percent(
            transfer.exact_reused_bytes,
            transfer.exact_reuse_plan_wire_bytes,
        ),

        cdc_offered_files: transfer.cdc_offered_files,

        cdc_files: transfer.cdc_files,

        cdc_fallback_files: transfer.cdc_fallback_files,

        cdc_logical_bytes: transfer.cdc_logical_bytes,

        cdc_reused_bytes: transfer.cdc_reused_bytes,

        cdc_literal_bytes: transfer.cdc_literal_bytes,

        cdc_index_wire_bytes: transfer.cdc_index_wire_bytes,

        cdc_plan_wire_bytes: transfer.cdc_plan_wire_bytes,

        cdc_wire_bytes,

        cdc_wire_savings_percent: wire_savings_percent(transfer.cdc_logical_bytes, cdc_wire_bytes),

        tiny_pack_count: transfer.tiny_pack_count,

        compressed_tiny_pack_count: transfer.compressed_tiny_pack_count,

        raw_tiny_pack_count: transfer.raw_tiny_pack_count,

        tiny_files_packed: transfer.tiny_files_packed,

        tiny_bytes_packed: transfer.tiny_bytes_packed,

        tiny_pack_wire_bytes: transfer.tiny_pack_wire_bytes,

        tiny_pack_wire_savings_percent: signed_wire_savings_percent(
            transfer.tiny_bytes_packed,
            transfer.tiny_pack_wire_bytes,
        ),
    }
}

fn receive_summary(report: CalibratedReceiveReport) -> GuiTransferSummary {
    let transfer = report.transfer;

    let cdc_wire_bytes = transfer
        .cdc_index_wire_bytes
        .saturating_add(transfer.cdc_plan_wire_bytes);

    GuiTransferSummary {
        direction: GuiTransferDirection::Receive,

        files: transfer.files_received,

        logical_bytes: transfer.bytes_received,

        wire_bytes: transfer.data_wire_bytes,

        compressed_records: transfer.compressed_records,

        resumed_stripes: transfer.resumed_stripes,

        resumed_bytes: transfer.resumed_bytes,

        skipped_files: transfer.skipped_files,

        skipped_bytes: transfer.skipped_bytes,

        data_stream_count: transfer.data_stream_count,

        tiny_materialization_workers: transfer.tiny_materialization_workers,

        elapsed: transfer.elapsed,

        logical_megabytes_per_second: megabytes_per_second(
            transfer.bytes_received,
            transfer.elapsed,
        ),

        wire_megabytes_per_second: megabytes_per_second(transfer.data_wire_bytes, transfer.elapsed),

        wire_savings_percent: wire_savings_percent(
            transfer.bytes_received,
            transfer.data_wire_bytes,
        ),

        exact_reused_files: transfer.exact_reused_files,

        exact_reused_bytes: transfer.exact_reused_bytes,

        exact_reuse_plan_wire_bytes: transfer.exact_reuse_plan_wire_bytes,

        exact_reuse_wire_savings_percent: wire_savings_percent(
            transfer.exact_reused_bytes,
            transfer.exact_reuse_plan_wire_bytes,
        ),

        cdc_offered_files: transfer.cdc_offered_files,

        cdc_files: transfer.cdc_files,

        cdc_fallback_files: transfer.cdc_fallback_files,

        cdc_logical_bytes: transfer.cdc_logical_bytes,

        cdc_reused_bytes: transfer.cdc_reused_bytes,

        cdc_literal_bytes: transfer.cdc_literal_bytes,

        cdc_index_wire_bytes: transfer.cdc_index_wire_bytes,

        cdc_plan_wire_bytes: transfer.cdc_plan_wire_bytes,

        cdc_wire_bytes,

        cdc_wire_savings_percent: wire_savings_percent(transfer.cdc_logical_bytes, cdc_wire_bytes),

        tiny_pack_count: transfer.tiny_pack_count,

        compressed_tiny_pack_count: transfer.compressed_tiny_pack_count,

        raw_tiny_pack_count: transfer.raw_tiny_pack_count,

        tiny_files_packed: transfer.tiny_files_packed,

        tiny_bytes_packed: transfer.tiny_bytes_packed,

        tiny_pack_wire_bytes: transfer.tiny_pack_wire_bytes,

        tiny_pack_wire_savings_percent: signed_wire_savings_percent(
            transfer.tiny_bytes_packed,
            transfer.tiny_pack_wire_bytes,
        ),
    }
}

fn megabytes_per_second(bytes: u64, elapsed: Duration) -> f64 {
    if elapsed.is_zero() {
        return 0.0;
    }

    bytes as f64 / elapsed.as_secs_f64() / 1_000_000.0
}

fn wire_savings_percent(logical_bytes: u64, wire_bytes: u64) -> f64 {
    if logical_bytes == 0 || wire_bytes >= logical_bytes {
        return 0.0;
    }

    (logical_bytes - wire_bytes) as f64 / logical_bytes as f64 * 100.0
}

fn signed_wire_savings_percent(logical_bytes: u64, wire_bytes: u64) -> f64 {
    if logical_bytes == 0 {
        return 0.0;
    }

    100.0 - wire_bytes as f64 / logical_bytes as f64 * 100.0
}

#[cfg(test)]
mod tests {
    use super::{
        DiagnosticInput, GuiConnectionMode, GuiTransferControl, GuiTransferDiagnostic,
        GuiTransferDirection, GuiTransferRequest, diagnose_transfer, run_gui_transfer,
    };
    use crate::destination_layout::DestinationLayout;
    use std::env;

    #[test]
    fn diagnostic_detects_fully_skipped_transfer() {
        assert_eq!(
            diagnose_transfer(DiagnosticInput {
                files: 10_000,
                logical_bytes: 2_000_000,

                wire_bytes: 0,

                exact_reused_files: 0,

                cdc_files: 0,
                cdc_reused_bytes: 0,

                compressed_records: 0,

                skipped_files: 10_000,

                skipped_bytes: 2_000_000,

                tiny_files_packed: 0,

                raw_tiny_pack_count: 0,
            }),
            GuiTransferDiagnostic::AllFilesSkipped,
        );
    }

    #[test]
    fn diagnostic_detects_tiny_file_overhead() {
        assert_eq!(
            diagnose_transfer(DiagnosticInput {
                files: 5_000,
                logical_bytes: 960_000,

                wire_bytes: 1_200_000,

                exact_reused_files: 0,

                cdc_files: 0,
                cdc_reused_bytes: 0,

                compressed_records: 0,

                skipped_files: 0,

                skipped_bytes: 0,

                tiny_files_packed: 5_000,

                raw_tiny_pack_count: 2,
            }),
            GuiTransferDiagnostic::TinyFileHeavy,
        );
    }

    #[test]
    fn diagnostic_detects_exact_file_reuse() {
        assert_eq!(
            diagnose_transfer(DiagnosticInput {
                files: 4,
                logical_bytes: 8 * 1024 * 1024,

                wire_bytes: 4 * 1024 * 1024,

                exact_reused_files: 2,

                cdc_files: 0,
                cdc_reused_bytes: 0,

                compressed_records: 0,

                skipped_files: 0,
                skipped_bytes: 0,

                tiny_files_packed: 0,
                raw_tiny_pack_count: 0,
            },),
            GuiTransferDiagnostic::ExactReuseEffective,
        );
    }

    #[test]
    fn diagnostic_detects_effective_cdc() {
        assert_eq!(
            diagnose_transfer(DiagnosticInput {
                files: 3,
                logical_bytes: 20_990_976,
                wire_bytes: 140_722,

                exact_reused_files: 0,

                cdc_files: 3,
                cdc_reused_bytes: 20_864_841,

                compressed_records: 0,

                skipped_files: 0,
                skipped_bytes: 0,

                tiny_files_packed: 0,
                raw_tiny_pack_count: 0,
            },),
            GuiTransferDiagnostic::CdcEffective,
        );
    }

    #[test]
    fn diagnostic_detects_effective_compression() {
        assert_eq!(
            diagnose_transfer(DiagnosticInput {
                files: 4,
                logical_bytes: 100_000_000,

                wire_bytes: 25_000_000,

                exact_reused_files: 0,

                cdc_files: 0,
                cdc_reused_bytes: 0,

                compressed_records: 4,

                skipped_files: 0,

                skipped_bytes: 0,

                tiny_files_packed: 0,

                raw_tiny_pack_count: 0,
            }),
            GuiTransferDiagnostic::CompressionEffective,
        );
    }

    #[test]
    fn diagnostic_detects_raw_compression_fallback() {
        assert_eq!(
            diagnose_transfer(DiagnosticInput {
                files: 20,
                logical_bytes: 20_000_000,

                wire_bytes: 20_100_000,

                exact_reused_files: 0,

                cdc_files: 0,
                cdc_reused_bytes: 0,

                compressed_records: 0,

                skipped_files: 0,

                skipped_bytes: 0,

                tiny_files_packed: 20,

                raw_tiny_pack_count: 1,
            }),
            GuiTransferDiagnostic::CompressionBypassed,
        );
    }
    use std::fs;
    use std::net::{SocketAddr, TcpListener};
    use std::process;
    use std::thread;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn gui_ip_transfer_round_trips_over_loopback() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();

        let parent = env::temp_dir().join(format!("networkcopy-gui-{}-{unique}", process::id(),));

        let source = parent.join("source");

        let destination = parent.join("destination");

        fs::create_dir_all(&source).unwrap();

        fs::write(source.join("hello.txt"), b"hello from the NetworkCopy GUI").unwrap();

        fs::write(source.join("compressible.bin"), vec![0x5A_u8; 384 * 1024]).unwrap();

        let duplicate_bytes = vec![0xA7_u8; 2 * 1024 * 1024];

        fs::write(source.join("duplicate-a.bin"), &duplicate_bytes).unwrap();

        fs::write(source.join("duplicate-b.bin"), &duplicate_bytes).unwrap();

        let port_probe = TcpListener::bind(("127.0.0.1", 0)).unwrap();

        let address = port_probe.local_addr().unwrap();

        drop(port_probe);

        let receiver_destination = destination.clone();

        let receiver = thread::spawn(move || {
            run_gui_transfer(GuiTransferRequest::Receive {
                connection: GuiConnectionMode::Address(address),

                destination_root: receiver_destination,

                destination_layout: DestinationLayout::Exact,

                update_existing: false,
            })
        });

        let sender_summary = run_gui_transfer(GuiTransferRequest::Send {
            connection: GuiConnectionMode::Address(address),

            source_root: source,

            worker_count: 2,

            calibration_mib: 1,
        })
        .unwrap();

        let receiver_summary = receiver.join().unwrap().unwrap();

        assert_eq!(sender_summary.direction, GuiTransferDirection::Send,);

        assert_eq!(receiver_summary.direction, GuiTransferDirection::Receive,);

        assert_eq!(sender_summary.files, 4,);

        assert_eq!(receiver_summary.files, 4,);

        assert_eq!(sender_summary.exact_reused_files, 1,);

        assert_eq!(receiver_summary.exact_reused_files, 1,);

        assert_eq!(
            sender_summary.exact_reused_bytes,
            duplicate_bytes.len() as u64,
        );

        assert_eq!(
            receiver_summary.exact_reused_bytes,
            duplicate_bytes.len() as u64,
        );

        assert_eq!(
            sender_summary.exact_reuse_plan_wire_bytes,
            receiver_summary.exact_reuse_plan_wire_bytes,
        );

        assert!(sender_summary.exact_reuse_plan_wire_bytes > 0,);

        assert_eq!(
            sender_summary.diagnostic(),
            GuiTransferDiagnostic::ExactReuseEffective,
        );

        assert_eq!(
            fs::read(destination.join("duplicate-a.bin",),).unwrap(),
            duplicate_bytes,
        );

        assert_eq!(
            fs::read(destination.join("duplicate-b.bin",),).unwrap(),
            duplicate_bytes,
        );

        assert_eq!(
            fs::read(destination.join("hello.txt")).unwrap(),
            b"hello from the NetworkCopy GUI",
        );

        assert_eq!(
            fs::metadata(destination.join("compressible.bin"))
                .unwrap()
                .len(),
            384 * 1024,
        );

        assert_eq!(
            sender_summary.tiny_pack_count,
            receiver_summary.tiny_pack_count,
        );

        assert_eq!(
            sender_summary.compressed_tiny_pack_count,
            receiver_summary.compressed_tiny_pack_count,
        );

        assert_eq!(
            sender_summary.raw_tiny_pack_count,
            receiver_summary.raw_tiny_pack_count,
        );

        assert_eq!(
            sender_summary.tiny_files_packed,
            receiver_summary.tiny_files_packed,
        );

        assert_eq!(
            sender_summary.tiny_pack_wire_bytes,
            receiver_summary.tiny_pack_wire_bytes,
        );

        assert!(sender_summary.tiny_pack_count > 0,);

        fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn gui_control_exposes_cancellation() {
        let control = GuiTransferControl::new();

        let before = control.progress();

        assert!(!before.cancel_requested,);

        control.cancel();

        let after = control.progress();

        assert!(after.cancel_requested,);
    }

    #[test]
    fn gui_update_transfer_skips_unchanged_files_and_replaces_changed_file() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();

        let parent =
            env::temp_dir().join(format!("networkcopy-gui-update-{}-{unique}", process::id(),));

        let source = parent.join("source");
        let destination = parent.join("destination");

        fs::create_dir_all(&source).unwrap();

        fs::write(
            source.join("unchanged.txt"),
            b"this file stays exactly the same",
        )
        .unwrap();

        fs::write(source.join("changed.txt"), b"original contents").unwrap();

        let first_address = available_address();

        let first_destination = destination.clone();

        let first_receiver = thread::spawn(move || {
            run_gui_transfer(GuiTransferRequest::Receive {
                connection: GuiConnectionMode::Address(first_address),

                destination_root: first_destination,

                destination_layout: DestinationLayout::Exact,

                update_existing: false,
            })
        });

        let _first_sender = run_gui_transfer(GuiTransferRequest::Send {
            connection: GuiConnectionMode::Address(first_address),

            source_root: source.clone(),

            worker_count: 2,

            calibration_mib: 1,
        })
        .unwrap();

        let _first_receiver = first_receiver.join().unwrap().unwrap();

        let unchanged_bytes = fs::metadata(source.join("unchanged.txt")).unwrap().len();

        fs::write(
            source.join("changed.txt"),
            b"replacement contents are different",
        )
        .unwrap();

        let second_address = available_address();

        let second_destination = destination.clone();

        let second_receiver = thread::spawn(move || {
            run_gui_transfer(GuiTransferRequest::Receive {
                connection: GuiConnectionMode::Address(second_address),

                destination_root: second_destination,

                destination_layout: DestinationLayout::Exact,

                update_existing: true,
            })
        });

        let sender_summary = run_gui_transfer(GuiTransferRequest::Send {
            connection: GuiConnectionMode::Address(second_address),

            source_root: source,

            worker_count: 2,

            calibration_mib: 1,
        })
        .unwrap();

        let receiver_summary = second_receiver.join().unwrap().unwrap();

        assert_eq!(sender_summary.skipped_files, 1,);

        assert_eq!(sender_summary.skipped_bytes, unchanged_bytes,);

        assert_eq!(receiver_summary.skipped_files, 1,);

        assert_eq!(receiver_summary.skipped_bytes, unchanged_bytes,);

        assert_eq!(
            fs::read(destination.join("unchanged.txt")).unwrap(),
            b"this file stays exactly the same",
        );

        assert_eq!(
            fs::read(destination.join("changed.txt")).unwrap(),
            b"replacement contents are different",
        );

        fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn gui_update_transfer_reports_medium_file_cdc() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();

        let parent =
            env::temp_dir().join(format!("networkcopy-gui-cdc-{}-{unique}", process::id(),));

        let source = parent.join("source");

        let destination = parent.join("destination");

        fs::create_dir_all(&source).unwrap();

        let basis = deterministic_bytes(2 * 1024 * 1024, 0x1234_5678_90AB_CDEF);

        let source_path = source.join("medium.bin");

        fs::write(&source_path, &basis).unwrap();

        let first_address = available_address();

        let first_destination = destination.clone();

        let first_receiver = thread::spawn(move || {
            run_gui_transfer(GuiTransferRequest::Receive {
                connection: GuiConnectionMode::Address(first_address),

                destination_root: first_destination,

                destination_layout: DestinationLayout::Exact,

                update_existing: false,
            })
        });

        run_gui_transfer(GuiTransferRequest::Send {
            connection: GuiConnectionMode::Address(first_address),

            source_root: source.clone(),

            worker_count: 2,
            calibration_mib: 1,
        })
        .unwrap();

        first_receiver.join().unwrap().unwrap();

        let insertion = deterministic_bytes(4097, 0xCAFE_BABE_DEAD_BEEF);

        let insertion_offset = 1024 * 1024 + 123;

        let mut candidate = Vec::with_capacity(basis.len() + insertion.len());

        candidate.extend_from_slice(&basis[..insertion_offset]);

        candidate.extend_from_slice(&insertion);

        candidate.extend_from_slice(&basis[insertion_offset..]);

        fs::write(&source_path, &candidate).unwrap();

        let second_address = available_address();

        let second_destination = destination.clone();

        let second_receiver = thread::spawn(move || {
            run_gui_transfer(GuiTransferRequest::Receive {
                connection: GuiConnectionMode::Address(second_address),

                destination_root: second_destination,

                destination_layout: DestinationLayout::Exact,

                update_existing: true,
            })
        });

        let sender_summary = run_gui_transfer(GuiTransferRequest::Send {
            connection: GuiConnectionMode::Address(second_address),

            source_root: source.clone(),

            worker_count: 2,
            calibration_mib: 1,
        })
        .unwrap();

        let receiver_summary = second_receiver.join().unwrap().unwrap();

        assert_eq!(sender_summary.cdc_offered_files, 1,);

        assert_eq!(sender_summary.cdc_files, 1,);

        assert_eq!(sender_summary.cdc_fallback_files, 0,);

        assert!(sender_summary.cdc_reused_bytes > sender_summary.cdc_logical_bytes * 90 / 100,);

        assert!(sender_summary.cdc_literal_bytes < 512 * 1024,);

        assert!(sender_summary.cdc_wire_savings_percent > 90.0,);

        assert_eq!(
            sender_summary.cdc_offered_files,
            receiver_summary.cdc_offered_files,
        );

        assert_eq!(sender_summary.cdc_files, receiver_summary.cdc_files,);

        assert_eq!(
            sender_summary.cdc_reused_bytes,
            receiver_summary.cdc_reused_bytes,
        );

        assert_eq!(
            sender_summary.cdc_literal_bytes,
            receiver_summary.cdc_literal_bytes,
        );

        assert_eq!(
            sender_summary.cdc_wire_bytes,
            receiver_summary.cdc_wire_bytes,
        );

        assert_eq!(
            sender_summary.diagnostic(),
            GuiTransferDiagnostic::CdcEffective,
        );

        assert_eq!(
            fs::read(destination.join("medium.bin",),).unwrap(),
            candidate,
        );

        fs::remove_dir_all(parent).unwrap();
    }

    fn deterministic_bytes(length: usize, seed: u64) -> Vec<u8> {
        let mut state = seed;

        let mut bytes = Vec::with_capacity(length);

        for _ in 0..length {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;

            state = state.wrapping_mul(0x2545_F491_4F6C_DD1D_u64);

            bytes.push(state as u8);
        }

        bytes
    }

    fn available_address() -> SocketAddr {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();

        listener.local_addr().unwrap()
    }
}
