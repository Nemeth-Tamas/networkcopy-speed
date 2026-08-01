use crate::{direct_address, management_discovery, management_protocol, windows_setup};
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

pub fn run() -> io::Result<()> {
    windows_setup::prepare_receiver(SocketAddr::V4(SocketAddrV4::new(
        Ipv4Addr::UNSPECIFIED,
        management_protocol::MANAGEMENT_CONTROL_PORT,
    )))?;

    windows_setup::prepare_receiver(SocketAddr::V4(SocketAddrV4::new(
        Ipv4Addr::UNSPECIFIED,
        direct_address::DIRECT_TRANSFER_PORT,
    )))?;

    windows_setup::prepare_discovery_receiver(management_protocol::MANAGEMENT_DISCOVERY_PORT)?;

    management_discovery::run_agent()
}
