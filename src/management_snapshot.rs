use crate::management_protocol::MAX_MANAGEMENT_PAYLOAD_BYTES;
use std::io;

const SNAPSHOT_VERSION: u16 = 1;
const SNAPSHOT_HEADER_BYTES: usize = 4;

const ACTIVE_HEADER_BYTES: usize = 28;
const RESULT_HEADER_BYTES: usize = 40;

const ACTIVE_PRESENT_FLAG: u8 = 0x01;
const RESULT_PRESENT_FLAG: u8 = 0x02;
const KNOWN_FLAGS: u8 = ACTIVE_PRESENT_FLAG | RESULT_PRESENT_FLAG;

const MAX_PHASE_BYTES: usize = 1024;
const MAX_MESSAGE_BYTES: usize = 4096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum ManagementJobRole {
    Sender = 1,
    Receiver = 2,
}

impl ManagementJobRole {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Sender => "sender",
            Self::Receiver => "receiver",
        }
    }
}

impl TryFrom<u8> for ManagementJobRole {
    type Error = io::Error;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Sender),
            2 => Ok(Self::Receiver),

            unknown => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown management job role {unknown}"),
            )),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum ManagementJobOutcome {
    Completed = 1,
    Cancelled = 2,
    Failed = 3,
}

impl ManagementJobOutcome {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
        }
    }
}

impl TryFrom<u8> for ManagementJobOutcome {
    type Error = io::Error;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Completed),
            2 => Ok(Self::Cancelled),
            3 => Ok(Self::Failed),

            unknown => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown management job outcome {unknown}"),
            )),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManagementActiveJobSnapshot {
    pub role: ManagementJobRole,

    pub job_id: u64,

    pub phase: String,

    pub completed: u64,

    pub total: u64,

    pub cancel_requested: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManagementJobResult {
    pub role: ManagementJobRole,

    pub outcome: ManagementJobOutcome,

    pub job_id: u64,

    pub files: u64,

    pub logical_bytes: u64,

    pub wire_bytes: u64,

    pub data_stream_count: u32,

    pub message: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ManagementAgentSnapshot {
    pub active: Option<ManagementActiveJobSnapshot>,

    pub latest_result: Option<ManagementJobResult>,
}

pub(crate) fn encode_snapshot(snapshot: &ManagementAgentSnapshot) -> io::Result<Vec<u8>> {
    let mut flags = 0_u8;

    if snapshot.active.is_some() {
        flags |= ACTIVE_PRESENT_FLAG;
    }

    if snapshot.latest_result.is_some() {
        flags |= RESULT_PRESENT_FLAG;
    }

    let mut payload = Vec::new();

    payload.extend_from_slice(&SNAPSHOT_VERSION.to_le_bytes());

    payload.push(flags);
    payload.push(0);

    if let Some(active) = &snapshot.active {
        encode_active(active, &mut payload)?;
    }

    if let Some(result) = &snapshot.latest_result {
        encode_result(result, &mut payload)?;
    }

    if payload.len() > MAX_MANAGEMENT_PAYLOAD_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "management snapshot requires {} bytes, exceeding the {MAX_MANAGEMENT_PAYLOAD_BYTES} byte limit",
                payload.len(),
            ),
        ));
    }

    Ok(payload)
}

pub(crate) fn decode_snapshot(payload: &[u8]) -> io::Result<ManagementAgentSnapshot> {
    if payload.len() < SNAPSHOT_HEADER_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "management snapshot has {} bytes, expected at least {SNAPSHOT_HEADER_BYTES}",
                payload.len(),
            ),
        ));
    }

    let version = u16::from_le_bytes(payload[0..2].try_into().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "management snapshot version was malformed",
        )
    })?);

    if version != SNAPSHOT_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unsupported management snapshot version {version}"),
        ));
    }

    let flags = payload[2];

    if flags & !KNOWN_FLAGS != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("management snapshot flags contained unknown bits 0x{flags:02X}"),
        ));
    }

    if payload[3] != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "management snapshot reserved byte was not zero",
        ));
    }

    let mut cursor = SNAPSHOT_HEADER_BYTES;

    let active = if flags & ACTIVE_PRESENT_FLAG != 0 {
        Some(decode_active(payload, &mut cursor)?)
    } else {
        None
    };

    let latest_result = if flags & RESULT_PRESENT_FLAG != 0 {
        Some(decode_result(payload, &mut cursor)?)
    } else {
        None
    };

    if cursor != payload.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "management snapshot contains {} trailing bytes",
                payload.len() - cursor,
            ),
        ));
    }

    Ok(ManagementAgentSnapshot {
        active,
        latest_result,
    })
}

fn encode_active(active: &ManagementActiveJobSnapshot, payload: &mut Vec<u8>) -> io::Result<()> {
    validate_job_id(active.job_id)?;

    validate_text(
        &active.phase,
        MAX_PHASE_BYTES,
        "management progress phase",
        false,
    )?;

    let phase_length = u16::try_from(active.phase.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "management progress phase length cannot be represented",
        )
    })?;

    payload.push(active.role as u8);

    payload.push(u8::from(active.cancel_requested));

    payload.extend_from_slice(&phase_length.to_le_bytes());

    payload.extend_from_slice(&active.job_id.to_le_bytes());

    payload.extend_from_slice(&active.completed.to_le_bytes());

    payload.extend_from_slice(&active.total.to_le_bytes());

    payload.extend_from_slice(active.phase.as_bytes());

    Ok(())
}

fn decode_active(payload: &[u8], cursor: &mut usize) -> io::Result<ManagementActiveJobSnapshot> {
    let header = take_bytes(
        payload,
        cursor,
        ACTIVE_HEADER_BYTES,
        "active management job header",
    )?;

    let role = ManagementJobRole::try_from(header[0])?;

    let cancel_requested = match header[1] {
        0 => false,
        1 => true,

        unknown => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("active management job used invalid cancellation flag {unknown}"),
            ));
        }
    };

    let phase_length = usize::from(u16::from_le_bytes(header[2..4].try_into().map_err(
        |_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "active phase length was malformed",
            )
        },
    )?));

    let job_id = u64::from_le_bytes(header[4..12].try_into().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "active management job ID was malformed",
        )
    })?);

    validate_job_id(job_id).map_err(invalid_data)?;

    let completed = u64::from_le_bytes(header[12..20].try_into().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "active completed byte count was malformed",
        )
    })?);

    let total = u64::from_le_bytes(header[20..28].try_into().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "active total byte count was malformed",
        )
    })?);

    let phase_bytes = take_bytes(payload, cursor, phase_length, "active management phase")?;

    let phase = decode_text(
        phase_bytes,
        MAX_PHASE_BYTES,
        "management progress phase",
        false,
    )?;

    Ok(ManagementActiveJobSnapshot {
        role,
        job_id,
        phase,
        completed,
        total,
        cancel_requested,
    })
}

fn encode_result(result: &ManagementJobResult, payload: &mut Vec<u8>) -> io::Result<()> {
    validate_job_id(result.job_id)?;

    validate_text(
        &result.message,
        MAX_MESSAGE_BYTES,
        "management result message",
        true,
    )?;

    let message_length = u16::try_from(result.message.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "management result message length cannot be represented",
        )
    })?;

    payload.push(result.role as u8);
    payload.push(result.outcome as u8);

    payload.extend_from_slice(&message_length.to_le_bytes());

    payload.extend_from_slice(&result.data_stream_count.to_le_bytes());

    payload.extend_from_slice(&result.job_id.to_le_bytes());

    payload.extend_from_slice(&result.files.to_le_bytes());

    payload.extend_from_slice(&result.logical_bytes.to_le_bytes());

    payload.extend_from_slice(&result.wire_bytes.to_le_bytes());

    payload.extend_from_slice(result.message.as_bytes());

    Ok(())
}

fn decode_result(payload: &[u8], cursor: &mut usize) -> io::Result<ManagementJobResult> {
    let header = take_bytes(
        payload,
        cursor,
        RESULT_HEADER_BYTES,
        "management result header",
    )?;

    let role = ManagementJobRole::try_from(header[0])?;

    let outcome = ManagementJobOutcome::try_from(header[1])?;

    let message_length = usize::from(u16::from_le_bytes(header[2..4].try_into().map_err(
        |_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "management result message length was malformed",
            )
        },
    )?));

    let data_stream_count = u32::from_le_bytes(header[4..8].try_into().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "management result stream count was malformed",
        )
    })?);

    let job_id = u64::from_le_bytes(header[8..16].try_into().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "management result job ID was malformed",
        )
    })?);

    validate_job_id(job_id).map_err(invalid_data)?;

    let files = u64::from_le_bytes(header[16..24].try_into().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "management result file count was malformed",
        )
    })?);

    let logical_bytes = u64::from_le_bytes(header[24..32].try_into().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "management result logical byte count was malformed",
        )
    })?);

    let wire_bytes = u64::from_le_bytes(header[32..40].try_into().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "management result wire byte count was malformed",
        )
    })?);

    let message_bytes = take_bytes(payload, cursor, message_length, "management result message")?;

    let message = decode_text(
        message_bytes,
        MAX_MESSAGE_BYTES,
        "management result message",
        true,
    )?;

    Ok(ManagementJobResult {
        role,
        outcome,
        job_id,
        files,
        logical_bytes,
        wire_bytes,
        data_stream_count,
        message,
    })
}

fn take_bytes<'a>(
    payload: &'a [u8],
    cursor: &mut usize,
    length: usize,
    description: &str,
) -> io::Result<&'a [u8]> {
    let end = cursor.checked_add(length).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{description} position overflowed"),
        )
    })?;

    if end > payload.len() {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            format!("management snapshot ended inside {description}"),
        ));
    }

    let bytes = &payload[*cursor..end];

    *cursor = end;

    Ok(bytes)
}

fn decode_text(
    bytes: &[u8],
    maximum_bytes: usize,
    description: &str,
    allow_empty: bool,
) -> io::Result<String> {
    let value = std::str::from_utf8(bytes)
        .map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{description} was not valid UTF-8: {error}"),
            )
        })?
        .to_owned();

    validate_text(&value, maximum_bytes, description, allow_empty).map_err(invalid_data)?;

    Ok(value)
}

fn validate_text(
    value: &str,
    maximum_bytes: usize,
    description: &str,
    allow_empty: bool,
) -> io::Result<()> {
    if !allow_empty && value.is_empty() {
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

fn validate_job_id(job_id: u64) -> io::Result<()> {
    if job_id == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "management snapshot job ID must not be zero",
        ));
    }

    Ok(())
}

fn invalid_data(error: io::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

#[cfg(test)]
mod tests {
    use super::{
        ManagementActiveJobSnapshot, ManagementAgentSnapshot, ManagementJobOutcome,
        ManagementJobResult, ManagementJobRole, decode_snapshot, encode_snapshot,
    };

    #[test]
    fn complete_snapshot_round_trips() {
        let expected = ManagementAgentSnapshot {
            active: Some(ManagementActiveJobSnapshot {
                role: ManagementJobRole::Sender,

                job_id: 17,

                phase: "Transfer send".to_string(),

                completed: 512 * 1024,

                total: 1024 * 1024,

                cancel_requested: false,
            }),

            latest_result: Some(ManagementJobResult {
                role: ManagementJobRole::Receiver,

                outcome: ManagementJobOutcome::Completed,

                job_id: 11,

                files: 42,

                logical_bytes: 4_000_000,

                wire_bytes: 0,

                data_stream_count: 4,

                message: String::new(),
            }),
        };

        let encoded = encode_snapshot(&expected).unwrap();

        let decoded = decode_snapshot(&encoded).unwrap();

        assert_eq!(decoded, expected);
    }

    #[test]
    fn empty_snapshot_round_trips() {
        let expected = ManagementAgentSnapshot::default();

        let encoded = encode_snapshot(&expected).unwrap();

        let decoded = decode_snapshot(&encoded).unwrap();

        assert_eq!(decoded, expected);
    }

    #[test]
    fn decoder_rejects_trailing_bytes() {
        let mut encoded = encode_snapshot(&ManagementAgentSnapshot::default()).unwrap();

        encoded.push(0xAA);

        let error = decode_snapshot(&encoded).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData,);
    }
}
