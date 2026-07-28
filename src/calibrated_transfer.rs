use crate::copy_bench::{decimal_megabytes_per_second, format_bytes};
use crate::multistream_copy::{self, MultistreamCopyReport, ReceiveReport};
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
    let calibration = network_calibration::send_matrix(receiver_address, calibration_bytes)?;

    let data_stream_count = calibration.recommended.data_stream_count;

    let calibrated_report = calibration.recommended;

    let transfer = multistream_copy::send(
        receiver_address,
        source_root,
        worker_count,
        data_stream_count,
    )?;

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

pub fn receive_once(
    listener: TcpListener,
    destination_root: &Path,
) -> io::Result<CalibratedReceiveReport> {
    let calibration = network_calibration::receive_matrix_on_listener(&listener)?;

    let transfer = multistream_copy::receive_on_listener(&listener, destination_root)?;

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
    use super::{receive_once, send};
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
