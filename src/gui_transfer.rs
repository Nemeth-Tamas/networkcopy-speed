use crate::calibrated_transfer::{self, CalibratedReceiveReport, CalibratedSendReport};
use crate::console_progress::{ProgressCounter, ProgressSnapshot};
use crate::direct_address::DIRECT_TRANSFER_PORT;
use crate::direct_discovery;
use crate::direct_transfer;
use crate::manifest_scan;
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

#[derive(Clone, Debug)]
pub struct GuiTransferSummary {
    pub direction: GuiTransferDirection,

    pub files: u64,

    pub logical_bytes: u64,

    pub wire_bytes: u64,

    pub compressed_records: u64,

    pub resumed_stripes: u64,

    pub resumed_bytes: u64,

    pub data_stream_count: usize,

    pub elapsed: Duration,

    pub logical_megabytes_per_second: f64,

    pub wire_megabytes_per_second: f64,

    pub wire_savings_percent: f64,

    pub tiny_pack_count: u64,

    pub compressed_tiny_pack_count: u64,

    pub raw_tiny_pack_count: u64,

    pub tiny_files_packed: u64,

    pub tiny_bytes_packed: u64,

    pub tiny_pack_wire_bytes: u64,

    pub tiny_pack_wire_savings_percent: f64,
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
        } => {
            let report = match connection {
                GuiConnectionMode::Direct => {
                    prepare_direct_receiver()?;

                    direct_transfer::receive_once_with_progress(
                        &destination_root,
                        control.progress.clone(),
                    )?
                }

                GuiConnectionMode::Address(bind_address) => {
                    prepare_address_receiver(bind_address)?;

                    let listener = TcpListener::bind(bind_address)?;

                    calibrated_transfer::receive_once_with_progress(
                        listener,
                        &destination_root,
                        control.progress.clone(),
                    )?
                }
            };

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

    GuiTransferSummary {
        direction: GuiTransferDirection::Send,

        files: transfer.files_copied,

        logical_bytes: transfer.bytes_copied,

        wire_bytes: transfer.data_wire_bytes,

        compressed_records: transfer.compressed_records,

        resumed_stripes: transfer.resumed_stripes,

        resumed_bytes: transfer.resumed_bytes,

        data_stream_count: transfer.data_stream_count,

        elapsed: transfer.data_elapsed,

        logical_megabytes_per_second,

        wire_megabytes_per_second,

        wire_savings_percent: wire_savings_percent(transfer.bytes_copied, transfer.data_wire_bytes),

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

    GuiTransferSummary {
        direction: GuiTransferDirection::Receive,

        files: transfer.files_received,

        logical_bytes: transfer.bytes_received,

        wire_bytes: transfer.data_wire_bytes,

        compressed_records: transfer.compressed_records,

        resumed_stripes: transfer.resumed_stripes,

        resumed_bytes: transfer.resumed_bytes,

        data_stream_count: transfer.data_stream_count,

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
        GuiConnectionMode, GuiTransferControl, GuiTransferDirection, GuiTransferRequest,
        run_gui_transfer,
    };
    use std::env;
    use std::fs;
    use std::net::TcpListener;
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

        let port_probe = TcpListener::bind(("127.0.0.1", 0)).unwrap();

        let address = port_probe.local_addr().unwrap();

        drop(port_probe);

        let receiver_destination = destination.clone();

        let receiver = thread::spawn(move || {
            run_gui_transfer(GuiTransferRequest::Receive {
                connection: GuiConnectionMode::Address(address),

                destination_root: receiver_destination,
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

        assert_eq!(sender_summary.files, 2,);

        assert_eq!(receiver_summary.files, 2,);

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
}
