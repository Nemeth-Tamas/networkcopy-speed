use crate::calibrated_transfer::{self, CalibratedReceiveReport, CalibratedSendReport};
use crate::direct_address::{self, DIRECT_TRANSFER_PORT};
use crate::direct_discovery;
use std::io;
use std::net::{SocketAddr, TcpListener};
use std::path::Path;

pub(crate) fn receive_once(destination_root: &Path) -> io::Result<CalibratedReceiveReport> {
    let interface_index = direct_discovery::receive_one()?;

    let bind_endpoint = direct_address::link_local_endpoint(interface_index, DIRECT_TRANSFER_PORT)?;

    let listener = TcpListener::bind(bind_endpoint)?;

    println!();
    println!("NetworkCopy Speed Edition direct-link receiver");

    println!("  Local interface: {}", interface_index,);

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

    let local_address = direct_address::link_local_endpoint(path.interface_index, 0)?;

    let receiver_address = SocketAddr::V6(path.endpoint);

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
