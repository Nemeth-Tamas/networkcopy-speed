use crate::direct_discovery;
use crate::multistream_copy;
use crate::tcp_connect;
use std::io::{self, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener};

const PROBE_MAGIC: [u8; 8] = *b"NCTP1REQ";

const ACK_MAGIC: [u8; 8] = *b"NCTP1ACK";

pub(crate) fn receive_once() -> io::Result<()> {
    let path = direct_discovery::receive_one()?;

    let listener = TcpListener::bind(path.local_endpoint)?;

    println!();
    println!("Direct-link TCP receiver ready");

    println!("  Local interface: {}", path.interface_index,);

    println!("  Listening:       {}", listener.local_addr()?,);

    println!();
    println!("Waiting for the discovered peer...");

    let (mut stream, peer_address) = listener.accept()?;

    stream.set_nodelay(true)?;

    let actual_local = stream.local_addr()?;

    validate_local_address(actual_local, path.local_endpoint.ip())?;

    let mut request = [0_u8; PROBE_MAGIC.len()];

    stream.read_exact(&mut request)?;

    if request != PROBE_MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "direct-link TCP probe had invalid request magic",
        ));
    }

    stream.write_all(&ACK_MAGIC)?;

    stream.flush()?;

    println!();
    println!("Direct-link TCP path validated");

    println!("  Local interface: {}", path.interface_index,);

    println!("  Local endpoint:  {}", actual_local,);

    println!("  Peer endpoint:   {}", peer_address,);

    Ok(())
}

pub(crate) fn send_once() -> io::Result<()> {
    let path = direct_discovery::discover_one()?;

    let expected_local = path.local_endpoint;

    let receiver_address = path.endpoint;

    let _binding = tcp_connect::begin_direct_binding(expected_local.ip())?;

    let mut stream = multistream_copy::connect_with_retry(receiver_address)?;

    stream.set_nodelay(true)?;

    let actual_local = stream.local_addr()?;

    validate_local_address(actual_local, expected_local.ip())?;

    stream.write_all(&PROBE_MAGIC)?;

    stream.flush()?;

    let mut acknowledgement = [0_u8; ACK_MAGIC.len()];

    stream.read_exact(&mut acknowledgement)?;

    if acknowledgement != ACK_MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "direct-link TCP probe had invalid acknowledgement magic",
        ));
    }

    println!();
    println!("Direct-link TCP path validated");

    println!("  Local interface: {}", path.interface_index,);

    println!("  Local endpoint:  {}", actual_local,);

    println!("  Peer endpoint:   {}", stream.peer_addr()?,);

    Ok(())
}

fn validate_local_address(actual: SocketAddr, expected_ip: IpAddr) -> io::Result<()> {
    if actual.ip() != expected_ip {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "direct-link socket used local address {}, expected {}",
                actual.ip(),
                expected_ip,
            ),
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::validate_local_address;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};

    #[test]
    fn matching_ipv6_local_address_is_valid() {
        let expected = "fe80::1234".parse::<Ipv6Addr>().unwrap();

        let actual = SocketAddr::V6(SocketAddrV6::new(expected, 7337, 0, 10));

        validate_local_address(actual, IpAddr::V6(expected)).unwrap();
    }

    #[test]
    fn wrong_local_address_is_rejected() {
        let actual = SocketAddr::V6(SocketAddrV6::new(
            "fe80::5678".parse().unwrap(),
            7337,
            0,
            10,
        ));

        let error =
            validate_local_address(actual, IpAddr::V6("fe80::1234".parse().unwrap())).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData,);
    }

    #[test]
    fn matching_ipv4_local_address_is_valid() {
        let expected = Ipv4Addr::new(169, 254, 132, 227);

        let actual = SocketAddr::V4(SocketAddrV4::new(expected, 7337));

        validate_local_address(actual, IpAddr::V4(expected)).unwrap();
    }
}
