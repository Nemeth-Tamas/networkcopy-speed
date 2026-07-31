use crate::management_discovery::{AgentCapabilities, AgentState};
use crate::management_filesystem;
use crate::management_protocol::{
    MANAGEMENT_CONTROL_PORT, MANAGEMENT_PROTOCOL_VERSION, ManagementFrame, ManagementMessageKind,
    read_frame, write_frame,
};
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream};
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const HELLO_PAYLOAD_VERSION: u16 = 1;
const HELLO_HEADER_BYTES: usize = 8;

const ROOTS_PAYLOAD_VERSION: u16 = 1;
const ROOTS_HEADER_BYTES: usize = 4;

const MAX_ROOTS: usize = 26;
const MAX_ROOT_PATH_BYTES: usize = 1024;

const MAX_HOSTNAME_BYTES: usize = 255;
const MAX_APPLICATION_VERSION_BYTES: usize = 64;

const CONTROL_IO_TIMEOUT: Duration = Duration::from_secs(5);

static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManagementHello {
    pub hostname: String,

    pub application_version: String,

    pub protocol_version: u16,

    pub state: AgentState,

    pub capabilities: AgentCapabilities,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManagementRoot {
    pub path: String,
}

struct ManagementControlServer {
    listener: TcpListener,

    hostname: String,

    state: AgentState,

    capabilities: AgentCapabilities,
}

impl ManagementControlServer {
    fn bind(
        hostname: String,
        state: AgentState,
        capabilities: AgentCapabilities,
    ) -> io::Result<Self> {
        Self::bind_at(
            SocketAddr::V4(SocketAddrV4::new(
                Ipv4Addr::UNSPECIFIED,
                MANAGEMENT_CONTROL_PORT,
            )),
            hostname,
            state,
            capabilities,
        )
    }

    fn bind_at(
        address: SocketAddr,
        hostname: String,
        state: AgentState,
        capabilities: AgentCapabilities,
    ) -> io::Result<Self> {
        validate_text(&hostname, MAX_HOSTNAME_BYTES, "management hostname")?;

        let listener = TcpListener::bind(address)?;

        Ok(Self {
            listener,
            hostname,
            state,
            capabilities,
        })
    }

    #[cfg(test)]
    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    fn serve_forever(self) -> io::Result<()> {
        loop {
            self.serve_one()?;
        }
    }

    fn serve_one(&self) -> io::Result<()> {
        let (stream, _) = self.listener.accept()?;

        self.handle_client(stream)
    }

    fn handle_client(&self, mut stream: TcpStream) -> io::Result<()> {
        stream.set_read_timeout(Some(CONTROL_IO_TIMEOUT))?;

        stream.set_write_timeout(Some(CONTROL_IO_TIMEOUT))?;

        let request = read_frame(&mut stream)?;

        let response = match request.kind {
            ManagementMessageKind::HelloRequest if request.payload.is_empty() => {
                ManagementFrame::new(
                    request.request_id,
                    ManagementMessageKind::HelloResponse,
                    encode_hello_payload(&self.hostname, self.state, self.capabilities)?,
                )?
            }

            ManagementMessageKind::HelloRequest => {
                error_response(request.request_id, "HelloRequest payload must be empty")?
            }

            ManagementMessageKind::ListRootsRequest if request.payload.is_empty() => {
                match management_filesystem::list_roots()
                    .and_then(|roots| encode_roots_payload(&roots))
                {
                    Ok(payload) => ManagementFrame::new(
                        request.request_id,
                        ManagementMessageKind::ListRootsResponse,
                        payload,
                    )?,

                    Err(error) => error_response(
                        request.request_id,
                        &format!("failed to enumerate Windows drive roots: {error}"),
                    )?,
                }
            }

            ManagementMessageKind::ListRootsRequest => {
                error_response(request.request_id, "ListRootsRequest payload must be empty")?
            }

            _ => error_response(
                request.request_id,
                "management command is not implemented yet",
            )?,
        };

        write_frame(&mut stream, &response)
    }
}

pub(crate) fn spawn(
    hostname: String,
    state: AgentState,
    capabilities: AgentCapabilities,
) -> io::Result<()> {
    let server = ManagementControlServer::bind(hostname, state, capabilities)?;

    thread::Builder::new()
        .name("networkcopy-management-control".to_string())
        .spawn(move || {
            if let Err(error) = server.serve_forever() {
                eprintln!("management control server failed: {error}");

                process::exit(1);
            }
        })
        .map(|_| ())
}

pub fn hello(endpoint: SocketAddr) -> io::Result<ManagementHello> {
    let response = exchange(endpoint, ManagementMessageKind::HelloRequest, Vec::new())?;

    match response.kind {
        ManagementMessageKind::HelloResponse => decode_hello_payload(&response.payload),

        unexpected => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("management agent returned unexpected message {unexpected:?} for HelloRequest"),
        )),
    }
}

pub fn list_roots(endpoint: SocketAddr) -> io::Result<Vec<ManagementRoot>> {
    let response = exchange(
        endpoint,
        ManagementMessageKind::ListRootsRequest,
        Vec::new(),
    )?;

    match response.kind {
        ManagementMessageKind::ListRootsResponse => decode_roots_payload(&response.payload),

        unexpected => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "management agent returned unexpected message {unexpected:?} for ListRootsRequest"
            ),
        )),
    }
}

fn exchange(
    endpoint: SocketAddr,
    request_kind: ManagementMessageKind,
    payload: Vec<u8>,
) -> io::Result<ManagementFrame> {
    let mut stream = TcpStream::connect_timeout(&endpoint, CONTROL_IO_TIMEOUT)?;

    stream.set_read_timeout(Some(CONTROL_IO_TIMEOUT))?;

    stream.set_write_timeout(Some(CONTROL_IO_TIMEOUT))?;

    let request_id = create_request_id();

    let request = ManagementFrame::new(request_id, request_kind, payload)?;

    write_frame(&mut stream, &request)?;

    let response = read_frame(&mut stream)?;

    if response.request_id != request_id {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "management response request ID was {}, expected {request_id}",
                response.request_id,
            ),
        ));
    }

    if response.kind == ManagementMessageKind::ErrorResponse {
        let message = String::from_utf8_lossy(&response.payload);

        return Err(io::Error::other(format!(
            "management agent rejected {request_kind:?}: {message}"
        )));
    }

    Ok(response)
}

fn encode_roots_payload(roots: &[String]) -> io::Result<Vec<u8>> {
    if roots.len() > MAX_ROOTS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "management root list contains {} entries, exceeding the {MAX_ROOTS} entry limit",
                roots.len(),
            ),
        ));
    }

    let root_count = u16::try_from(roots.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "management root count cannot be represented",
        )
    })?;

    let mut payload_length = ROOTS_HEADER_BYTES;

    for root in roots {
        validate_text(root, MAX_ROOT_PATH_BYTES, "management root path")?;

        payload_length = payload_length
            .checked_add(2)
            .and_then(|length| length.checked_add(root.len()))
            .ok_or_else(|| io::Error::other("management roots payload length overflowed"))?;
    }

    let mut payload = Vec::with_capacity(payload_length);

    payload.extend_from_slice(&ROOTS_PAYLOAD_VERSION.to_le_bytes());

    payload.extend_from_slice(&root_count.to_le_bytes());

    for root in roots {
        let root_length = u16::try_from(root.len()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "management root path length cannot be represented",
            )
        })?;

        payload.extend_from_slice(&root_length.to_le_bytes());

        payload.extend_from_slice(root.as_bytes());
    }

    Ok(payload)
}

fn decode_roots_payload(payload: &[u8]) -> io::Result<Vec<ManagementRoot>> {
    if payload.len() < ROOTS_HEADER_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "management roots payload has {} bytes, expected at least {ROOTS_HEADER_BYTES}",
                payload.len(),
            ),
        ));
    }

    let payload_version = u16::from_le_bytes(payload[0..2].try_into().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "management roots version was malformed",
        )
    })?);

    if payload_version != ROOTS_PAYLOAD_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unsupported management roots payload version {payload_version}"),
        ));
    }

    let root_count = usize::from(u16::from_le_bytes(payload[2..4].try_into().map_err(
        |_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "management root count was malformed",
            )
        },
    )?));

    if root_count > MAX_ROOTS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "management root list contains {root_count} entries, exceeding the {MAX_ROOTS} entry limit"
            ),
        ));
    }

    let mut roots = Vec::with_capacity(root_count);

    let mut cursor = ROOTS_HEADER_BYTES;

    for _ in 0..root_count {
        let length_end = cursor.checked_add(2).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "management root length position overflowed",
            )
        })?;

        if length_end > payload.len() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "management roots payload ended before a root length",
            ));
        }

        let root_length = usize::from(u16::from_le_bytes(
            payload[cursor..length_end].try_into().map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "management root length was malformed",
                )
            })?,
        ));

        cursor = length_end;

        let root_end = cursor.checked_add(root_length).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "management root position overflowed",
            )
        })?;

        if root_end > payload.len() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "management roots payload ended inside a root path",
            ));
        }

        let path = decode_text(
            &payload[cursor..root_end],
            MAX_ROOT_PATH_BYTES,
            "management root path",
        )?;

        roots.push(ManagementRoot { path });

        cursor = root_end;
    }

    if cursor != payload.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "management roots payload contains {} trailing bytes",
                payload.len() - cursor,
            ),
        ));
    }

    Ok(roots)
}

fn error_response(request_id: u64, message: &str) -> io::Result<ManagementFrame> {
    ManagementFrame::new(
        request_id,
        ManagementMessageKind::ErrorResponse,
        message.as_bytes().to_vec(),
    )
}

fn encode_hello_payload(
    hostname: &str,
    state: AgentState,
    capabilities: AgentCapabilities,
) -> io::Result<Vec<u8>> {
    let application_version = env!("CARGO_PKG_VERSION");

    validate_text(hostname, MAX_HOSTNAME_BYTES, "management hostname")?;

    validate_text(
        application_version,
        MAX_APPLICATION_VERSION_BYTES,
        "application version",
    )?;

    let application_version_length = u16::try_from(application_version.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "application version length cannot be represented",
        )
    })?;

    let hostname_length = u16::try_from(hostname.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "management hostname length cannot be represented",
        )
    })?;

    let total_length = HELLO_HEADER_BYTES
        .checked_add(application_version.len())
        .and_then(|length| length.checked_add(hostname.len()))
        .ok_or_else(|| io::Error::other("management hello payload length overflowed"))?;

    let mut payload = Vec::with_capacity(total_length);

    payload.extend_from_slice(&HELLO_PAYLOAD_VERSION.to_le_bytes());

    payload.push(state as u8);
    payload.push(capabilities.bits());

    payload.extend_from_slice(&application_version_length.to_le_bytes());

    payload.extend_from_slice(&hostname_length.to_le_bytes());

    payload.extend_from_slice(application_version.as_bytes());

    payload.extend_from_slice(hostname.as_bytes());

    Ok(payload)
}

fn decode_hello_payload(payload: &[u8]) -> io::Result<ManagementHello> {
    if payload.len() < HELLO_HEADER_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "management hello payload has {} bytes, expected at least {HELLO_HEADER_BYTES}",
                payload.len(),
            ),
        ));
    }

    let payload_version = u16::from_le_bytes(payload[0..2].try_into().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "management hello version was malformed",
        )
    })?);

    if payload_version != HELLO_PAYLOAD_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unsupported management hello payload version {payload_version}"),
        ));
    }

    let state = AgentState::try_from(payload[2])?;

    let capabilities = AgentCapabilities::from_bits(payload[3])?;

    let application_version_length = usize::from(u16::from_le_bytes(
        payload[4..6].try_into().map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "application version length was malformed",
            )
        })?,
    ));

    let hostname_length = usize::from(u16::from_le_bytes(payload[6..8].try_into().map_err(
        |_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "management hostname length was malformed",
            )
        },
    )?));

    let expected_length = HELLO_HEADER_BYTES
        .checked_add(application_version_length)
        .and_then(|length| length.checked_add(hostname_length))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "management hello payload length overflowed",
            )
        })?;

    if payload.len() != expected_length {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "management hello payload has {} bytes, expected {expected_length}",
                payload.len(),
            ),
        ));
    }

    let application_version_end = HELLO_HEADER_BYTES + application_version_length;

    let application_version = decode_text(
        &payload[HELLO_HEADER_BYTES..application_version_end],
        MAX_APPLICATION_VERSION_BYTES,
        "application version",
    )?;

    let hostname = decode_text(
        &payload[application_version_end..],
        MAX_HOSTNAME_BYTES,
        "management hostname",
    )?;

    Ok(ManagementHello {
        hostname,
        application_version,
        protocol_version: MANAGEMENT_PROTOCOL_VERSION,
        state,
        capabilities,
    })
}

fn decode_text(bytes: &[u8], maximum_bytes: usize, description: &str) -> io::Result<String> {
    let value = std::str::from_utf8(bytes)
        .map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{description} was not valid UTF-8: {error}"),
            )
        })?
        .to_owned();

    validate_text(&value, maximum_bytes, description)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;

    Ok(value)
}

fn validate_text(value: &str, maximum_bytes: usize, description: &str) -> io::Result<()> {
    if value.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{description} must not be empty"),
        ));
    }

    if value.len() > maximum_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "{description} contains {} bytes, exceeding the {maximum_bytes} byte limit",
                value.len(),
            ),
        ));
    }

    Ok(())
}

fn create_request_id() -> u64 {
    let sequence = NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed);

    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos() as u64);

    timestamp ^ (u64::from(process::id()) << 32) ^ sequence
}

#[cfg(test)]
mod tests {
    use super::{
        HELLO_PAYLOAD_VERSION, ManagementControlServer, ManagementRoot, decode_hello_payload,
        decode_roots_payload, encode_hello_payload, encode_roots_payload, hello,
        list_roots as request_roots,
    };
    use crate::management_discovery::{AgentCapabilities, AgentState};
    use crate::management_filesystem;
    use crate::management_protocol::MANAGEMENT_PROTOCOL_VERSION;
    use std::io;
    use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
    use std::thread;

    #[test]
    fn loopback_hello_round_trips() {
        let server = ManagementControlServer::bind_at(
            SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)),
            "LOOPBACK-PC".to_string(),
            AgentState::Idle,
            AgentCapabilities::SEND_RECEIVE,
        )
        .unwrap();

        let endpoint = server.local_addr().unwrap();

        let server_thread = thread::spawn(move || {
            server.serve_one().unwrap();
        });

        let response = hello(endpoint).unwrap();

        server_thread.join().unwrap();

        assert_eq!(response.hostname, "LOOPBACK-PC",);

        assert_eq!(response.application_version, env!("CARGO_PKG_VERSION"),);

        assert_eq!(response.protocol_version, MANAGEMENT_PROTOCOL_VERSION,);

        assert_eq!(response.state, AgentState::Idle,);

        assert!(response.capabilities.can_send(),);

        assert!(response.capabilities.can_receive(),);
    }

    #[test]
    fn loopback_lists_windows_roots() {
        let server = ManagementControlServer::bind_at(
            SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)),
            "LOOPBACK-PC".to_string(),
            AgentState::Idle,
            AgentCapabilities::SEND_RECEIVE,
        )
        .unwrap();

        let endpoint = server.local_addr().unwrap();

        let server_thread = thread::spawn(move || {
            server.serve_one().unwrap();
        });

        let actual = request_roots(endpoint).unwrap();

        server_thread.join().unwrap();

        let expected = management_filesystem::list_roots()
            .unwrap()
            .into_iter()
            .map(|path| ManagementRoot { path })
            .collect::<Vec<_>>();

        assert_eq!(actual, expected);
    }

    #[test]
    fn roots_payload_round_trips() {
        let expected = vec!["C:\\".to_string(), "D:\\".to_string(), "Z:\\".to_string()];

        let encoded = encode_roots_payload(&expected).unwrap();

        let decoded = decode_roots_payload(&encoded).unwrap();

        assert_eq!(
            decoded,
            expected
                .into_iter()
                .map(|path| { ManagementRoot { path } })
                .collect::<Vec<_>>(),
        );
    }

    #[test]
    fn hello_payload_round_trips() {
        let encoded =
            encode_hello_payload("TEST-PC", AgentState::Busy, AgentCapabilities::SEND_RECEIVE)
                .unwrap();

        let decoded = decode_hello_payload(&encoded).unwrap();

        assert_eq!(decoded.hostname, "TEST-PC",);

        assert_eq!(decoded.state, AgentState::Busy,);
    }

    #[test]
    fn hello_payload_rejects_unknown_version() {
        let mut encoded =
            encode_hello_payload("TEST-PC", AgentState::Idle, AgentCapabilities::SEND_RECEIVE)
                .unwrap();

        encoded[0..2].copy_from_slice(&(HELLO_PAYLOAD_VERSION + 1).to_le_bytes());

        let error = decode_hello_payload(&encoded).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData,);
    }
}
