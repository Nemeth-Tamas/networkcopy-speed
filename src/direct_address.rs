use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddrV4, SocketAddrV6};
use std::ptr;
use std::slice;

use windows_sys::Win32::NetworkManagement::IpHelper::{
    FreeMibTable, GetUnicastIpAddressTable, MIB_UNICASTIPADDRESS_ROW, MIB_UNICASTIPADDRESS_TABLE,
};
use windows_sys::Win32::Networking::WinSock::{AF_INET, AF_INET6};

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

pub(crate) fn print_addresses(interface_index: u32) -> io::Result<()> {
    let ipv6 = optional_address(link_local_endpoint(interface_index, DIRECT_TRANSFER_PORT))?;

    let ipv4 = optional_address(apipa_endpoint(interface_index, DIRECT_TRANSFER_PORT))?;

    if ipv6.is_none() && ipv4.is_none() {
        return Err(io::Error::new(
            io::ErrorKind::AddrNotAvailable,
            format!(
                "interface {interface_index} has neither an IPv6 link-local nor an IPv4 APIPA address",
            ),
        ));
    }

    println!("NetworkCopy Speed Edition direct-link addresses");

    println!("  Interface index: {}", interface_index,);

    println!();

    match ipv6 {
        Some(endpoint) => {
            println!(
                "  IPv6 link-local: {}%{}",
                endpoint.ip(),
                endpoint.scope_id(),
            );

            println!("  IPv6 endpoint:   {}", endpoint,);
        }

        None => {
            println!("  IPv6 link-local: unavailable");
        }
    }

    println!();

    match ipv4 {
        Some(endpoint) => {
            println!("  IPv4 APIPA:      {}", endpoint.ip(),);

            println!("  IPv4 endpoint:   {}", endpoint,);
        }

        None => {
            println!("  IPv4 APIPA:      unavailable");
        }
    }

    Ok(())
}

fn optional_address<T>(result: io::Result<T>) -> io::Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),

        Err(error) if error.kind() == io::ErrorKind::AddrNotAvailable => Ok(None),

        Err(error) => Err(error),
    }
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

pub(crate) fn apipa_endpoint(interface_index: u32, port: u16) -> io::Result<SocketAddrV4> {
    if interface_index == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "interface index must not be zero",
        ));
    }

    let mut raw_table = ptr::null_mut();

    let status = unsafe { GetUnicastIpAddressTable(AF_INET, &mut raw_table) };

    if status != NO_ERROR {
        return Err(io::Error::from_raw_os_error(status as i32));
    }

    if raw_table.is_null() {
        return Err(io::Error::other(
            "GetUnicastIpAddressTable returned a null IPv4 table",
        ));
    }

    let table = UnicastAddressTable(raw_table);

    let entry_count = unsafe { (*table.0).NumEntries as usize };

    let first_entry = unsafe { (*table.0).Table.as_ptr() };

    let rows = unsafe { slice::from_raw_parts(first_entry, entry_count) };

    rows.iter()
        .filter(|row| row.InterfaceIndex == interface_index)
        .filter_map(ipv4_address)
        .find(|address| is_apipa(*address))
        .map(|address| SocketAddrV4::new(address, port))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::AddrNotAvailable,
                format!("interface {interface_index} has no IPv4 APIPA address",),
            )
        })
}

fn ipv4_address(row: &MIB_UNICASTIPADDRESS_ROW) -> Option<Ipv4Addr> {
    let family = unsafe { row.Address.si_family };

    if family != AF_INET {
        return None;
    }

    let socket_address = unsafe { row.Address.Ipv4 };

    let address_bytes = unsafe { socket_address.sin_addr.S_un.S_addr }.to_ne_bytes();

    Some(Ipv4Addr::from(address_bytes))
}

fn is_apipa(address: Ipv4Addr) -> bool {
    let octets = address.octets();

    octets[0] == 169 && octets[1] == 254
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
    use super::{is_apipa, is_link_local};
    use std::net::{Ipv4Addr, Ipv6Addr};

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

    #[test]
    fn automatic_private_ipv4_is_apipa() {
        assert!(is_apipa("169.254.132.227".parse::<Ipv4Addr>().unwrap(),),);
    }

    #[test]
    fn private_lan_ipv4_is_not_apipa() {
        assert!(!is_apipa("192.168.1.10".parse::<Ipv4Addr>().unwrap(),),);
    }
}
