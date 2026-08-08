use std::env;
use std::error::Error;
use std::io;
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use networkcopy_speed::transfer_path::{TransferPath, format_link_speed, inspect_tcp_stream};

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let mut arguments = env::args_os();

    let program = arguments
        .next()
        .unwrap_or_else(|| "networkcopy-path-probe".into());

    let Some(endpoint) = arguments.next() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("usage: {} <ip:port>", program.to_string_lossy(),),
        )
        .into());
    };

    if let Some(extra) = arguments.next() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unexpected extra argument: {}", extra.to_string_lossy(),),
        )
        .into());
    }

    let endpoint = endpoint.to_string_lossy().parse::<SocketAddr>()?;

    let stream = TcpStream::connect_timeout(&endpoint, Duration::from_secs(5))?;

    let path = inspect_tcp_stream(&stream)?;

    println!("NetworkCopy Speed Edition transfer-path probe");
    println!("  Remote endpoint:      {}", stream.peer_addr()?);
    println!("  Local endpoint:       {}", stream.local_addr()?);
    println!("  Transfer path:        {}", path.kind);

    print_optional_details(&path);

    Ok(())
}

fn print_optional_details(path: &TransferPath) {
    if let Some(interface_index) = path.interface_index {
        println!("  Interface index:      {interface_index}");
    }

    if let Some(alias) = &path.interface_alias {
        println!("  Interface alias:      {alias}");
    }

    if let Some(mtu) = path.mtu {
        println!("  Interface MTU:        {mtu} bytes");
    }

    if let Some(speed) = path.transmit_link_speed_bps {
        println!("  Transmit link speed:  {}", format_link_speed(speed),);
    }

    if let Some(speed) = path.receive_link_speed_bps {
        println!("  Receive link speed:   {}", format_link_speed(speed),);
    }
}

#[cfg(test)]
mod tests {
    use super::format_link_speed;

    #[test]
    fn link_speed_uses_readable_decimal_units() {
        assert_eq!(format_link_speed(2_500_000_000), "2.50 Gbit/s");
        assert_eq!(format_link_speed(866_700_000), "866.70 Mbit/s");
        assert_eq!(format_link_speed(10_000), "10.00 kbit/s");
        assert_eq!(format_link_speed(999), "999 bit/s");
    }
}
