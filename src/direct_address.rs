use std::io;
use std::net::{Ipv6Addr, SocketAddrV6};
use std::ptr;
use std::slice;

use windows_sys::Win32::NetworkManagement::IpHelper::{
    FreeMibTable, GetUnicastIpAddressTable, MIB_UNICASTIPADDRESS_ROW, MIB_UNICASTIPADDRESS_TABLE,
};
use windows_sys::Win32::Networking::WinSock::AF_INET6;

const NO_ERROR: u32 = 0;

pub(crate) const DIRECT_TRANSFER_PORT: u16 = 7337;

struct UnicastAddressTable(*mut MIB_UNICASTIPADDRESS_TABLE);

impl Drop for UnicastAddressTable {
    fn drop(&mut self) {
        unsafe {
            FreeMibTable(self.0.cast());
        }
    }
}

pub(crate) fn print_link_local(interface_index: u32) -> io::Result<()> {
    let endpoint = link_local_endpoint(interface_index, DIRECT_TRANSFER_PORT)?;

    println!("NetworkCopy Speed Edition direct-link IPv6 address");

    println!("  Interface index: {}", interface_index,);

    println!(
        "  IPv6 link-local: {}%{}",
        endpoint.ip(),
        endpoint.scope_id(),
    );

    println!("  TCP endpoint:    {}", endpoint,);

    Ok(())
}

pub(crate) fn link_local_endpoint(interface_index: u32, port: u16) -> io::Result<SocketAddrV6> {
    if interface_index == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "interface index must not be zero",
        ));
    }

    let mut raw_table = ptr::null_mut();

    let status = unsafe { GetUnicastIpAddressTable(AF_INET6, &mut raw_table) };

    if status != NO_ERROR {
        return Err(io::Error::from_raw_os_error(status as i32));
    }

    if raw_table.is_null() {
        return Err(io::Error::other(
            "GetUnicastIpAddressTable returned a null table",
        ));
    }

    let table = UnicastAddressTable(raw_table);

    let entry_count = unsafe { (*table.0).NumEntries as usize };

    let first_entry = unsafe { (*table.0).Table.as_ptr() };

    let rows = unsafe { slice::from_raw_parts(first_entry, entry_count) };

    rows.iter()
        .filter(|row| row.InterfaceIndex == interface_index)
        .filter_map(ipv6_address)
        .find(|address| is_link_local(*address))
        .map(|address| SocketAddrV6::new(address, port, 0, interface_index))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::AddrNotAvailable,
                format!("interface {interface_index} has no IPv6 link-local address"),
            )
        })
}

fn ipv6_address(row: &MIB_UNICASTIPADDRESS_ROW) -> Option<Ipv6Addr> {
    let family = unsafe { row.Address.si_family };

    if family != AF_INET6 {
        return None;
    }

    let socket_address = unsafe { row.Address.Ipv6 };

    let octets = unsafe { socket_address.sin6_addr.u.Byte };

    Some(Ipv6Addr::from(octets))
}

fn is_link_local(address: Ipv6Addr) -> bool {
    address.segments()[0] & 0xFFC0 == 0xFE80
}

#[cfg(test)]
mod tests {
    use super::is_link_local;
    use std::net::Ipv6Addr;

    #[test]
    fn fe80_address_is_link_local() {
        assert!(is_link_local("fe80::1234".parse::<Ipv6Addr>().unwrap(),),);
    }

    #[test]
    fn global_address_is_not_link_local() {
        assert!(!is_link_local(
            "2001:db8::1234".parse::<Ipv6Addr>().unwrap(),
        ),);
    }
}
