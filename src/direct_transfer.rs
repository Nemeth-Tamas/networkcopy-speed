use crate::calibrated_transfer::{self, CalibratedReceiveReport, CalibratedSendReport};
use crate::direct_discovery;
use crate::tcp_connect;
use std::io;
use std::net::TcpListener;
use std::path::Path;

pub(crate) fn receive_once(destination_root: &Path) -> io::Result<CalibratedReceiveReport> {
    let path = direct_discovery::receive_one()?;

    let listener = TcpListener::bind(path.local_endpoint)?;

    println!();
    println!("NetworkCopy Speed Edition direct-link receiver");

    println!("  Local interface: {}", path.interface_index,);

    println!("  Listening:       {}", listener.local_addr()?,);

    println!("  Destination:     {}", destination_root.display(),);

    println!("  Sequence:        calibration matrix, then folder transfer");

    println!();

    calibrated_transfer::receive_once(listener, destination_root)
}

pub(crate) fn send(
    source_root: &Path,
    worker_count: usize,
    calibration_bytes: u64,
) -> io::Result<CalibratedSendReport> {
    let path = direct_discovery::discover_one()?;

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

    calibrated_transfer::send(
        receiver_address,
        source_root,
        worker_count,
        calibration_bytes,
    )
}
