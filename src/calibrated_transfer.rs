use crate::console_progress::{ConsoleProgress, ProgressCounter};
use crate::copy_bench::{decimal_megabytes_per_second, format_bytes};
use crate::desktop_layout::DesktopLayoutSnapshot;
use crate::destination_layout::DestinationLayout;
use crate::multistream_copy::{self, DestinationMode, MultistreamCopyReport, ReceiveReport};
use crate::network_calibration::{self, NetworkCalibrationMatrixReport, NetworkCalibrationReport};
use std::io;
use std::net::{SocketAddr, TcpListener};
use std::path::Path;

pub const DEFAULT_CALIBRATION_MIB: u64 = 256;

#[derive(Debug)]
pub struct CalibratedSendReport {
    pub calibration: NetworkCalibrationMatrixReport,

    pub transfer: MultistreamCopyReport,

    pub calibrated_megabytes_per_second: f64,

    pub logical_megabytes_per_second: f64,

    pub wire_megabytes_per_second: f64,

    pub path_efficiency_percent: f64,
}

impl CalibratedSendReport {
    pub fn print(&self) {
        self.calibration.print("send");

        println!();

        self.transfer.print();

        println!();

        println!("Calibrated transfer comparison");

        println!(
            "  Selected streams:     {}",
            self.transfer.data_stream_count
        );

        println!(
            "  Raw TCP ceiling:      {:.2} MB/s",
            self.calibrated_megabytes_per_second
        );

        println!(
            "  Logical throughput:   {:.2} MB/s",
            self.logical_megabytes_per_second
        );

        println!(
            "  App wire throughput:  {:.2} MB/s",
            self.wire_megabytes_per_second
        );

        println!(
            "  Path efficiency:      {:.2}%",
            self.path_efficiency_percent
        );

        println!(
            "  Wire payload:         {} bytes",
            format_bytes(self.transfer.data_wire_bytes,)
        );
    }
}

#[derive(Debug)]
pub struct CalibratedReceiveReport {
    pub calibration: NetworkCalibrationMatrixReport,

    pub transfer: ReceiveReport,
}

impl CalibratedReceiveReport {
    pub fn print(&self) {
        self.calibration.print("receive");

        println!();

        self.transfer.print();

        println!();

        println!("Calibrated receiver summary");

        println!(
            "  Transfer streams:     {}",
            self.transfer.data_stream_count
        );

        println!("  Matrix included count: yes");
    }
}

pub fn send(
    receiver_address: SocketAddr,
    source_root: &Path,
    worker_count: usize,
    calibration_bytes: u64,
) -> io::Result<CalibratedSendReport> {
    let calibration_progress = ConsoleProgress::start("Connecting calibration", 0)?;

    let calibration = network_calibration::send_matrix_with_progress(
        receiver_address,
        calibration_bytes,
        calibration_progress.counter(),
    )?;

    calibration_progress.finish()?;

    println!();

    let data_stream_count = calibration.recommended.data_stream_count;

    let compression_lane_megabytes_per_second =
        compression_lane_megabytes_per_second(&calibration, data_stream_count)?;

    let calibrated_report = calibration.best;

    let transfer_progress = ConsoleProgress::start("Scanning source", 0)?;

    let transfer = multistream_copy::send_with_progress_calibrated(
        receiver_address,
        source_root,
        worker_count,
        data_stream_count,
        transfer_progress.counter(),
        None,
        compression_lane_megabytes_per_second,
    )?;

    transfer_progress.finish()?;

    println!();

    let calibrated_megabytes_per_second = report_megabytes_per_second(&calibrated_report);

    let logical_megabytes_per_second =
        decimal_megabytes_per_second(transfer.bytes_copied, transfer.data_elapsed);

    let wire_megabytes_per_second =
        decimal_megabytes_per_second(transfer.data_wire_bytes, transfer.data_elapsed);

    let path_efficiency_percent =
        percentage_of(wire_megabytes_per_second, calibrated_megabytes_per_second);

    Ok(CalibratedSendReport {
        calibration,
        transfer,
        calibrated_megabytes_per_second,
        logical_megabytes_per_second,
        wire_megabytes_per_second,
        path_efficiency_percent,
    })
}

pub(crate) fn send_with_progress_and_desktop_layout(
    receiver_address: SocketAddr,
    source_root: &Path,
    worker_count: usize,
    calibration_bytes: u64,
    progress: ProgressCounter,
    desktop_layout: Option<DesktopLayoutSnapshot>,
) -> io::Result<CalibratedSendReport> {
    send_with_progress_and_stream_count(
        receiver_address,
        source_root,
        worker_count,
        calibration_bytes,
        progress,
        None,
        desktop_layout,
    )
}

pub(crate) fn send_with_progress_and_stream_count(
    receiver_address: SocketAddr,
    source_root: &Path,
    worker_count: usize,
    calibration_bytes: u64,
    progress: ProgressCounter,
    forced_data_stream_count: Option<usize>,
    desktop_layout: Option<DesktopLayoutSnapshot>,
) -> io::Result<CalibratedSendReport> {
    if let Some(data_stream_count) = forced_data_stream_count {
        network_calibration::validate_matrix_stream_count(data_stream_count)?;
    }

    progress.set_label("Connecting calibration");

    progress.set_completed(0);

    progress.set_total(0);

    let calibration = network_calibration::send_matrix_with_progress(
        receiver_address,
        calibration_bytes,
        progress.clone(),
    )?;

    progress.check_cancelled()?;

    let data_stream_count =
        forced_data_stream_count.unwrap_or(calibration.recommended.data_stream_count);

    let compression_lane_megabytes_per_second =
        compression_lane_megabytes_per_second(&calibration, data_stream_count)?;

    let calibrated_report = calibration.best;

    match forced_data_stream_count {
        Some(data_stream_count) => {
            progress.set_label(format!(
                "Scanning source - resuming with {data_stream_count} streams"
            ));
        }

        None => {
            progress.set_label("Scanning source");
        }
    }

    progress.set_completed(0);

    progress.set_total(0);

    let transfer = multistream_copy::send_with_progress_calibrated(
        receiver_address,
        source_root,
        worker_count,
        data_stream_count,
        progress.clone(),
        desktop_layout,
        compression_lane_megabytes_per_second,
    )?;

    progress.check_cancelled()?;

    let calibrated_megabytes_per_second = report_megabytes_per_second(&calibrated_report);

    let logical_megabytes_per_second =
        decimal_megabytes_per_second(transfer.bytes_copied, transfer.data_elapsed);

    let wire_megabytes_per_second =
        decimal_megabytes_per_second(transfer.data_wire_bytes, transfer.data_elapsed);

    let path_efficiency_percent =
        percentage_of(wire_megabytes_per_second, calibrated_megabytes_per_second);

    let total = progress.total();

    progress.set_completed(total);

    progress.set_label("Complete");

    Ok(CalibratedSendReport {
        calibration,

        transfer,

        calibrated_megabytes_per_second,

        logical_megabytes_per_second,

        wire_megabytes_per_second,

        path_efficiency_percent,
    })
}

pub fn receive_once(
    listener: TcpListener,
    destination_root: &Path,
) -> io::Result<CalibratedReceiveReport> {
    let calibration_progress = ConsoleProgress::start("Waiting for calibration", 0)?;

    let calibration = network_calibration::receive_matrix_on_listener_with_progress(
        &listener,
        calibration_progress.counter(),
    )?;

    calibration_progress.finish()?;

    println!();

    let transfer_progress = ConsoleProgress::start("Waiting for transfer", 0)?;

    let transfer = multistream_copy::receive_on_listener_with_progress(
        &listener,
        destination_root,
        transfer_progress.counter(),
    )?;

    transfer_progress.finish()?;

    println!();

    if !calibration
        .reports
        .iter()
        .any(|report| report.data_stream_count == transfer.data_stream_count)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "transfer used {} streams, which was not included in the calibration matrix",
                transfer.data_stream_count
            ),
        ));
    }

    Ok(CalibratedReceiveReport {
        calibration,
        transfer,
    })
}

pub(crate) fn receive_once_with_progress_and_mode(
    listener: TcpListener,
    destination_root: &Path,
    progress: ProgressCounter,
    destination_mode: DestinationMode,
) -> io::Result<CalibratedReceiveReport> {
    receive_once_with_progress_mode_and_layout(
        listener,
        destination_root,
        progress,
        destination_mode,
        DestinationLayout::Exact,
    )
}

pub(crate) fn receive_once_with_progress_mode_and_layout(
    listener: TcpListener,
    destination_root: &Path,
    progress: ProgressCounter,
    destination_mode: DestinationMode,
    destination_layout: DestinationLayout,
) -> io::Result<CalibratedReceiveReport> {
    progress.set_label("Waiting for calibration");

    progress.set_completed(0);

    progress.set_total(0);

    let calibration =
        network_calibration::receive_matrix_on_listener_with_progress(&listener, progress.clone())?;

    progress.check_cancelled()?;

    progress.set_label("Waiting for transfer");

    progress.set_completed(0);

    progress.set_total(0);

    let transfer = multistream_copy::receive_on_listener_with_progress_mode_and_layout(
        &listener,
        destination_root,
        progress.clone(),
        destination_mode,
        destination_layout,
    )?;

    progress.check_cancelled()?;

    if !calibration
        .reports
        .iter()
        .any(|report| report.data_stream_count == transfer.data_stream_count)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "transfer used {} streams, which was not included in the calibration matrix",
                transfer.data_stream_count,
            ),
        ));
    }

    let total = progress.total();

    progress.set_completed(total);

    progress.set_label("Complete");

    Ok(CalibratedReceiveReport {
        calibration,

        transfer,
    })
}

fn compression_lane_megabytes_per_second(
    calibration: &NetworkCalibrationMatrixReport,
    data_stream_count: usize,
) -> io::Result<f64> {
    let report = calibration
        .reports
        .iter()
        .find(|report| report.data_stream_count == data_stream_count)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "requested transfer stream count {data_stream_count} was not included in the completed calibration matrix",
                ),
            )
        })?;

    let aggregate = report_megabytes_per_second(report);

    let lane = aggregate / data_stream_count as f64;

    if !lane.is_finite() || lane <= 0.0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "selected calibration produced an invalid per-lane throughput",
        ));
    }

    Ok(lane)
}

fn report_megabytes_per_second(report: &NetworkCalibrationReport) -> f64 {
    decimal_megabytes_per_second(report.total_bytes, report.elapsed)
}

fn percentage_of(value: f64, ceiling: f64) -> f64 {
    if ceiling == 0.0 {
        return 0.0;
    }

    value / ceiling * 100.0
}

#[cfg(test)]
mod tests {
    use super::{compression_lane_megabytes_per_second, receive_once, send};
    use std::env;
    use std::fs;
    use std::net::TcpListener;
    use std::process;
    use std::thread;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn calibrated_transfer_round_trips() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();

        let parent =
            env::temp_dir().join(format!("networkcopy-calibrated-{}-{unique}", process::id()));

        let source = parent.join("source");

        let destination = parent.join("destination");

        fs::create_dir_all(&source).unwrap();

        fs::write(source.join("small.txt"), b"automatic calibrated transfer").unwrap();

        let medium_contents = vec![0xC3_u8; 300 * 1024];

        fs::write(source.join("medium.bin"), &medium_contents).unwrap();

        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();

        let receiver_address = listener.local_addr().unwrap();

        let receiver_destination = destination.clone();

        let receiver = thread::spawn(move || receive_once(listener, &receiver_destination));

        let sender_report = send(receiver_address, &source, 2, 8 * 1024 * 1024 + 137).unwrap();

        let receiver_report = receiver.join().unwrap().unwrap();

        assert_eq!(sender_report.transfer.files_copied, 2);

        assert_eq!(receiver_report.transfer.files_received, 2);

        assert_eq!(
            sender_report.transfer.data_stream_count,
            sender_report.calibration.recommended.data_stream_count
        );

        assert_eq!(
            sender_report.transfer.data_stream_count,
            receiver_report.transfer.data_stream_count
        );

        let expected_compression_lane = compression_lane_megabytes_per_second(
            &sender_report.calibration,
            sender_report.transfer.data_stream_count,
        )
        .unwrap();

        let actual_compression_lane = sender_report
            .transfer
            .compression_lane_megabytes_per_second
            .unwrap();

        assert!((actual_compression_lane - expected_compression_lane).abs() < 0.000_001,);

        assert!(sender_report.calibrated_megabytes_per_second > 0.0);

        assert!(sender_report.wire_megabytes_per_second > 0.0);

        assert_eq!(
            fs::read(destination.join("small.txt",),).unwrap(),
            b"automatic calibrated transfer"
        );

        assert_eq!(
            fs::read(destination.join("medium.bin",),).unwrap(),
            medium_contents
        );

        fs::remove_dir_all(parent).unwrap();
    }
}
