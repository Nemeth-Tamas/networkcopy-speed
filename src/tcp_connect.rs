use crate::direct_address;
use socket2::{Domain, Protocol, Socket, Type};
use std::io;
use std::net::{IpAddr, Ipv6Addr, SocketAddr, TcpStream};
use std::sync::{Mutex, MutexGuard, OnceLock};

static DIRECT_BINDING: OnceLock<Mutex<Option<IpAddr>>> = OnceLock::new();

pub(crate) struct DirectBindingGuard;

impl Drop for DirectBindingGuard {
    fn drop(&mut self) {
        if let Ok(mut binding) = binding_slot().lock() {
            *binding = None;
        }
    }
}

pub(crate) fn begin_direct_binding(address: IpAddr) -> io::Result<DirectBindingGuard> {
    let mut binding = lock_binding()?;

    if binding.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "a direct-link source binding is already active",
        ));
    }

    *binding = Some(address);

    Ok(DirectBindingGuard)
}

fn binding_slot() -> &'static Mutex<Option<IpAddr>> {
    DIRECT_BINDING.get_or_init(|| Mutex::new(None))
}

fn lock_binding() -> io::Result<MutexGuard<'static, Option<IpAddr>>> {
    binding_slot()
        .lock()
        .map_err(|_| io::Error::other("direct-link source binding lock was poisoned"))
}

fn configured_binding() -> io::Result<Option<IpAddr>> {
    Ok(*lock_binding()?)
}

pub(crate) fn connect(receiver_address: SocketAddr) -> io::Result<TcpStream> {
    if let Some(address) = configured_binding()? {
        return connect_bound(SocketAddr::new(address, 0), receiver_address);
    }

    let Some(local_address) = local_bind_address(receiver_address)? else {
        return TcpStream::connect(receiver_address);
    };

    connect_bound(local_address, receiver_address)
}

fn local_bind_address(receiver_address: SocketAddr) -> io::Result<Option<SocketAddr>> {
    let SocketAddr::V6(receiver) = receiver_address else {
        return Ok(None);
    };

    if !is_link_local(*receiver.ip()) {
        return Ok(None);
    }

    let interface_index = receiver.scope_id();

    if interface_index == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("IPv6 link-local receiver {receiver_address} has no interface scope ID",),
        ));
    }

    let local_address = direct_address::link_local_endpoint(interface_index, 0)?;

    Ok(Some(SocketAddr::V6(local_address)))
}

fn connect_bound(local_address: SocketAddr, receiver_address: SocketAddr) -> io::Result<TcpStream> {
    if local_address.is_ipv4() != receiver_address.is_ipv4() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "local address {local_address} and receiver {receiver_address} use different address families",
            ),
        ));
    }

    let domain = if receiver_address.is_ipv4() {
        Domain::IPV4
    } else {
        Domain::IPV6
    };

    let socket = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))?;

    if receiver_address.is_ipv6() {
        socket.set_only_v6(true)?;
    }

    socket.bind(&local_address.into())?;

    socket.connect(&receiver_address.into())?;

    let stream = TcpStream::from(socket);

    validate_local_address(&stream, local_address.ip())?;

    Ok(stream)
}

fn validate_local_address(stream: &TcpStream, expected_ip: IpAddr) -> io::Result<()> {
    let actual = stream.local_addr()?;

    if actual.ip() != expected_ip {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "connected socket used local address {}, expected {expected_ip}",
                actual.ip(),
            ),
        ));
    }

    Ok(())
}

fn is_link_local(address: Ipv6Addr) -> bool {
    address.segments()[0] & 0xFFC0 == 0xFE80
}

#[cfg(test)]
mod tests {
    use super::{connect_bound, local_bind_address};
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV6, TcpListener};
    use std::thread;

    #[test]
    fn global_ipv6_does_not_require_binding() {
        let receiver = "[2001:db8::1]:7337".parse::<SocketAddr>().unwrap();

        assert_eq!(local_bind_address(receiver,).unwrap(), None,);
    }

    #[test]
    fn unscoped_link_local_address_is_rejected() {
        let receiver = SocketAddr::V6(SocketAddrV6::new("fe80::1234".parse().unwrap(), 7337, 0, 0));

        let error = local_bind_address(receiver).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput,);
    }

    #[test]
    fn mixed_address_families_are_rejected() {
        let local = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);

        let receiver = SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 7337);

        let error = connect_bound(local, receiver).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput,);
    }

    #[test]
    fn bound_connection_uses_requested_source() {
        let listener = TcpListener::bind((Ipv6Addr::LOCALHOST, 0)).unwrap();

        let receiver_address = listener.local_addr().unwrap();

        let receiver = thread::spawn(move || listener.accept().map(|_| ()));

        let local_address = SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 0);

        let stream = connect_bound(local_address, receiver_address).unwrap();

        assert_eq!(
            stream.local_addr().unwrap().ip(),
            IpAddr::V6(Ipv6Addr::LOCALHOST,),
        );

        drop(stream);

        receiver.join().unwrap().unwrap();
    }
}
