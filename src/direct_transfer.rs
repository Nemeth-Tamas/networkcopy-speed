use crate::calibrated_transfer::{self, CalibratedReceiveReport, CalibratedSendReport};
use crate::console_progress::ProgressCounter;
use crate::direct_discovery;
use crate::tcp_connect;
use std::io;
use std::net::TcpListener;
use std::path::Path;

pub(crate) fn receive_once(destination_root: &Path) -> io::Result<CalibratedReceiveReport> {
    receive_configured(destination_root, None)
}

pub(crate) fn receive_once_with_progress(
    destination_root: &Path,
    progress: ProgressCounter,
) -> io::Result<CalibratedReceiveReport> {
    receive_configured(destination_root, Some(progress))
}

fn receive_configured(
    destination_root: &Path,
    progress: Option<ProgressCounter>,
) -> io::Result<CalibratedReceiveReport> {
    if let Some(progress) = &progress {
        progress.set_label("Waiting for direct sender");

        progress.set_completed(0);

        progress.set_total(0);

        progress.check_cancelled()?;
    }

    let path = match &progress {
        Some(progress) => direct_discovery::receive_one_with_progress(progress)?,

        None => direct_discovery::receive_one()?,
    };

    if let Some(progress) = &progress {
        progress.check_cancelled()?;
    }

    let listener = TcpListener::bind(path.local_endpoint)?;

    println!();
    println!("NetworkCopy Speed Edition direct-link receiver");

    println!("  Local interface: {}", path.interface_index,);

    println!("  Listening:       {}", listener.local_addr()?,);

    println!("  Destination:     {}", destination_root.display(),);

    println!("  Sequence:        calibration matrix, then folder transfer");

    println!();

    match progress {
        Some(progress) => {
            calibrated_transfer::receive_once_with_progress(listener, destination_root, progress)
        }

        None => calibrated_transfer::receive_once(listener, destination_root),
    }
}

pub(crate) fn send(
    source_root: &Path,
    worker_count: usize,
    calibration_bytes: u64,
) -> io::Result<CalibratedSendReport> {
    send_configured(source_root, worker_count, calibration_bytes, None)
}

pub(crate) fn send_with_progress(
    source_root: &Path,
    worker_count: usize,
    calibration_bytes: u64,
    progress: ProgressCounter,
) -> io::Result<CalibratedSendReport> {
    send_configured(source_root, worker_count, calibration_bytes, Some(progress))
}

fn send_configured(
    source_root: &Path,
    worker_count: usize,
    calibration_bytes: u64,
    progress: Option<ProgressCounter>,
) -> io::Result<CalibratedSendReport> {
    if let Some(progress) = &progress {
        progress.set_label("Discovering direct receiver");

        progress.set_completed(0);

        progress.set_total(0);

        progress.check_cancelled()?;
    }

    let path = match &progress {
        Some(progress) => direct_discovery::discover_one_with_progress(progress)?,

        None => direct_discovery::discover_one()?,
    };

    if let Some(progress) = &progress {
        progress.check_cancelled()?;
    }

    let local_address = path.local_endpoint;

    let receiver_address = path.endpoint;

    let _binding = tcp_connect::begin_direct_binding(local_address.ip())?;

    println!();
    println!("NetworkCopy Speed Edition direct-link sender");

    println!("  Local interface: {}", path.interface_index,);

    println!("  Source binding:  {}", local_address,);

    println!("  Receiver:        {}", receiver_address,);

    println!("  Source:          {}", source_root.display(),);

    println!("  Scan workers:    {}", worker_count,);

    println!("  Sequence:        calibration matrix, then folder transfer");

    println!();

    match progress {
        Some(progress) => calibrated_transfer::send_with_progress(
            receiver_address,
            source_root,
            worker_count,
            calibration_bytes,
            progress,
        ),

        None => calibrated_transfer::send(
            receiver_address,
            source_root,
            worker_count,
            calibration_bytes,
        ),
    }
}
