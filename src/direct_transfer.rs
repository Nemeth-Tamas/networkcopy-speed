use crate::calibrated_transfer::{self, CalibratedReceiveReport, CalibratedSendReport};
use crate::console_progress::ProgressCounter;
use crate::desktop_layout::DesktopLayoutSnapshot;
use crate::destination_layout::DestinationLayout;
use crate::direct_discovery;
use crate::multistream_copy::DestinationMode;
use crate::tcp_connect;
use std::io;
use std::net::TcpListener;
use std::path::Path;

pub(crate) fn receive_once(destination_root: &Path) -> io::Result<CalibratedReceiveReport> {
    receive_configured(
        destination_root,
        None,
        DestinationMode::Fresh,
        DestinationLayout::Exact,
    )
}

pub(crate) fn receive_once_with_progress_and_mode(
    destination_root: &Path,
    progress: ProgressCounter,
    destination_mode: DestinationMode,
) -> io::Result<CalibratedReceiveReport> {
    receive_once_with_progress_mode_and_layout(
        destination_root,
        progress,
        destination_mode,
        DestinationLayout::Exact,
    )
}

pub(crate) fn receive_once_with_progress_mode_and_layout(
    destination_root: &Path,
    progress: ProgressCounter,
    destination_mode: DestinationMode,
    destination_layout: DestinationLayout,
) -> io::Result<CalibratedReceiveReport> {
    receive_configured(
        destination_root,
        Some(progress),
        destination_mode,
        destination_layout,
    )
}

fn receive_configured(
    destination_root: &Path,
    progress: Option<ProgressCounter>,
    destination_mode: DestinationMode,
    destination_layout: DestinationLayout,
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
        Some(progress) => calibrated_transfer::receive_once_with_progress_mode_and_layout(
            listener,
            destination_root,
            progress,
            destination_mode,
            destination_layout,
        ),

        None => {
            if destination_mode != DestinationMode::Fresh
                || destination_layout != DestinationLayout::Exact
            {
                return Err(io::Error::other(
                    "update or destination-root mode requires progress-aware receiver execution",
                ));
            }

            calibrated_transfer::receive_once(listener, destination_root)
        }
    }
}

pub(crate) fn send(
    source_root: &Path,
    worker_count: usize,
    calibration_bytes: u64,
) -> io::Result<CalibratedSendReport> {
    send_configured(source_root, worker_count, calibration_bytes, None, None)
}

pub(crate) fn send_with_progress(
    source_root: &Path,
    worker_count: usize,
    calibration_bytes: u64,
    progress: ProgressCounter,
) -> io::Result<CalibratedSendReport> {
    send_with_progress_and_desktop_layout(
        source_root,
        worker_count,
        calibration_bytes,
        progress,
        None,
    )
}

pub(crate) fn send_with_progress_and_desktop_layout(
    source_root: &Path,
    worker_count: usize,
    calibration_bytes: u64,
    progress: ProgressCounter,
    desktop_layout: Option<DesktopLayoutSnapshot>,
) -> io::Result<CalibratedSendReport> {
    send_configured(
        source_root,
        worker_count,
        calibration_bytes,
        Some(progress),
        desktop_layout,
    )
}

fn send_configured(
    source_root: &Path,
    worker_count: usize,
    calibration_bytes: u64,
    progress: Option<ProgressCounter>,
    desktop_layout: Option<DesktopLayoutSnapshot>,
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

    match (progress, desktop_layout) {
        (Some(progress), desktop_layout) => {
            calibrated_transfer::send_with_progress_and_desktop_layout(
                receiver_address,
                source_root,
                worker_count,
                calibration_bytes,
                progress,
                desktop_layout,
            )
        }

        (None, None) => calibrated_transfer::send(
            receiver_address,
            source_root,
            worker_count,
            calibration_bytes,
        ),

        (None, Some(_)) => Err(io::Error::other(
            "desktop layout metadata requires progress-aware direct sender execution",
        )),
    }
}
