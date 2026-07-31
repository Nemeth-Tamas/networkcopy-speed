use std::io::{self, Read, Write};

pub const MANAGEMENT_DISCOVERY_PORT: u16 = 7338;
pub const MANAGEMENT_CONTROL_PORT: u16 = 7339;

pub const MANAGEMENT_PROTOCOL_VERSION: u16 = 1;

pub const MAX_MANAGEMENT_PAYLOAD_BYTES: usize = 1024 * 1024;

const MANAGEMENT_MAGIC: [u8; 4] = *b"NCM1";
const MANAGEMENT_HEADER_BYTES: usize = 20;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum ManagementMessageKind {
    HelloRequest = 0x01,
    HelloResponse = 0x02,

    ListRootsRequest = 0x10,
    ListRootsResponse = 0x11,

    ListDirectoryRequest = 0x12,
    ListDirectoryResponse = 0x13,

    PrepareReceiveRequest = 0x20,
    PrepareReceiveResponse = 0x21,

    StartSendRequest = 0x22,
    StartSendResponse = 0x23,

    CancelJobRequest = 0x24,
    CancelJobResponse = 0x25,

    JobStatusRequest = 0x26,
    JobStatusResponse = 0x27,

    AgentSnapshotRequest = 0x28,
    AgentSnapshotResponse = 0x29,

    ErrorResponse = 0xFF,
}

impl TryFrom<u8> for ManagementMessageKind {
    type Error = io::Error;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        let kind = match value {
            0x01 => Self::HelloRequest,
            0x02 => Self::HelloResponse,

            0x10 => Self::ListRootsRequest,
            0x11 => Self::ListRootsResponse,

            0x12 => Self::ListDirectoryRequest,
            0x13 => Self::ListDirectoryResponse,

            0x20 => Self::PrepareReceiveRequest,
            0x21 => Self::PrepareReceiveResponse,

            0x22 => Self::StartSendRequest,
            0x23 => Self::StartSendResponse,

            0x24 => Self::CancelJobRequest,
            0x25 => Self::CancelJobResponse,

            0x26 => Self::JobStatusRequest,
            0x27 => Self::JobStatusResponse,

            0x28 => Self::AgentSnapshotRequest,
            0x29 => Self::AgentSnapshotResponse,

            0xFF => Self::ErrorResponse,

            unknown => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unknown management message kind 0x{unknown:02X}"),
                ));
            }
        };

        Ok(kind)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManagementFrame {
    pub request_id: u64,
    pub kind: ManagementMessageKind,
    pub payload: Vec<u8>,
}

impl ManagementFrame {
    pub fn new(request_id: u64, kind: ManagementMessageKind, payload: Vec<u8>) -> io::Result<Self> {
        validate_payload_length(payload.len())?;

        Ok(Self {
            request_id,
            kind,
            payload,
        })
    }
}

pub fn write_frame(writer: &mut impl Write, frame: &ManagementFrame) -> io::Result<()> {
    validate_payload_length(frame.payload.len())?;

    let payload_length = u32::try_from(frame.payload.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "management payload length cannot be represented",
        )
    })?;

    writer.write_all(&MANAGEMENT_MAGIC)?;

    writer.write_all(&MANAGEMENT_PROTOCOL_VERSION.to_le_bytes())?;

    writer.write_all(&[frame.kind as u8])?;
    writer.write_all(&[0])?;

    writer.write_all(&frame.request_id.to_le_bytes())?;
    writer.write_all(&payload_length.to_le_bytes())?;

    writer.write_all(&frame.payload)?;
    writer.flush()
}

pub fn read_frame(reader: &mut impl Read) -> io::Result<ManagementFrame> {
    let mut header = [0_u8; MANAGEMENT_HEADER_BYTES];
    reader.read_exact(&mut header)?;

    if header[0..4] != MANAGEMENT_MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "management frame used an invalid magic value",
        ));
    }

    let version = u16::from_le_bytes([header[4], header[5]]);

    if version != MANAGEMENT_PROTOCOL_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unsupported management protocol version {version}"),
        ));
    }

    let kind = ManagementMessageKind::try_from(header[6])?;

    if header[7] != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "management frame reserved byte was not zero",
        ));
    }

    let request_id = u64::from_le_bytes(header[8..16].try_into().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "management request ID was malformed",
        )
    })?);

    let payload_length = u32::from_le_bytes(header[16..20].try_into().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "management payload length was malformed",
        )
    })?);

    let payload_length = usize::try_from(payload_length).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "management payload length cannot be represented",
        )
    })?;

    if payload_length > MAX_MANAGEMENT_PAYLOAD_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "management payload contains {payload_length} bytes, exceeding the {} byte limit",
                MAX_MANAGEMENT_PAYLOAD_BYTES,
            ),
        ));
    }

    let mut payload = vec![0_u8; payload_length];
    reader.read_exact(&mut payload)?;

    Ok(ManagementFrame {
        request_id,
        kind,
        payload,
    })
}

fn validate_payload_length(payload_length: usize) -> io::Result<()> {
    if payload_length > MAX_MANAGEMENT_PAYLOAD_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "management payload contains {payload_length} bytes, exceeding the {} byte limit",
                MAX_MANAGEMENT_PAYLOAD_BYTES,
            ),
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        MANAGEMENT_PROTOCOL_VERSION, MAX_MANAGEMENT_PAYLOAD_BYTES, ManagementFrame,
        ManagementMessageKind, read_frame, write_frame,
    };
    use std::io::{self, Cursor};

    #[test]
    fn frame_round_trips() {
        let expected = ManagementFrame::new(
            0x1234_5678_90AB_CDEF,
            ManagementMessageKind::HelloResponse,
            b"hello management".to_vec(),
        )
        .unwrap();

        let mut encoded = Vec::new();

        write_frame(&mut encoded, &expected).unwrap();

        let actual = read_frame(&mut Cursor::new(encoded)).unwrap();

        assert_eq!(actual, expected);
    }

    #[test]
    fn empty_payload_round_trips() {
        let expected =
            ManagementFrame::new(42, ManagementMessageKind::JobStatusRequest, Vec::new()).unwrap();

        let mut encoded = Vec::new();

        write_frame(&mut encoded, &expected).unwrap();

        let actual = read_frame(&mut Cursor::new(encoded)).unwrap();

        assert_eq!(actual, expected);
    }

    #[test]
    fn constructor_rejects_oversized_payload() {
        let error = ManagementFrame::new(
            1,
            ManagementMessageKind::HelloRequest,
            vec![0; MAX_MANAGEMENT_PAYLOAD_BYTES + 1],
        )
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput,);
    }

    #[test]
    fn reader_rejects_invalid_magic() {
        let mut encoded = valid_encoded_frame();

        encoded[0..4].copy_from_slice(b"NOPE");

        let error = read_frame(&mut Cursor::new(encoded)).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData,);
    }

    #[test]
    fn reader_rejects_unknown_version() {
        let mut encoded = valid_encoded_frame();

        encoded[4..6].copy_from_slice(&(MANAGEMENT_PROTOCOL_VERSION + 1).to_le_bytes());

        let error = read_frame(&mut Cursor::new(encoded)).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData,);
    }

    #[test]
    fn reader_rejects_unknown_message_kind() {
        let mut encoded = valid_encoded_frame();

        encoded[6] = 0x7E;

        let error = read_frame(&mut Cursor::new(encoded)).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData,);
    }

    #[test]
    fn reader_rejects_nonzero_reserved_byte() {
        let mut encoded = valid_encoded_frame();

        encoded[7] = 1;

        let error = read_frame(&mut Cursor::new(encoded)).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData,);
    }

    #[test]
    fn reader_rejects_declared_oversized_payload() {
        let mut encoded = valid_encoded_frame();

        let oversized = u32::try_from(MAX_MANAGEMENT_PAYLOAD_BYTES + 1).unwrap();

        encoded[16..20].copy_from_slice(&oversized.to_le_bytes());

        let error = read_frame(&mut Cursor::new(encoded)).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData,);
    }

    #[test]
    fn reader_rejects_truncated_payload() {
        let mut encoded = valid_encoded_frame();

        encoded.pop();

        let error = read_frame(&mut Cursor::new(encoded)).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof,);
    }

    fn valid_encoded_frame() -> Vec<u8> {
        let frame =
            ManagementFrame::new(99, ManagementMessageKind::HelloRequest, b"hello".to_vec())
                .unwrap();

        let mut encoded = Vec::new();

        write_frame(&mut encoded, &frame).unwrap();

        encoded
    }
}
