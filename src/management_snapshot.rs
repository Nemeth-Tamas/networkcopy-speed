use crate::management_instance::AgentInstanceId;
use crate::management_protocol::MAX_MANAGEMENT_PAYLOAD_BYTES;
use std::io;
use std::net::SocketAddr;

const SNAPSHOT_VERSION: u16 = 3;

const SNAPSHOT_HEADER_BYTES: usize = 4 + AgentInstanceId::BYTE_COUNT;

const ACTIVE_HEADER_BYTES: usize = 32;
const SENDER_DETAIL_HEADER_BYTES: usize = 20;
const RECEIVER_DETAIL_HEADER_BYTES: usize = 8;

const RESULT_HEADER_BYTES: usize = 40;

const ACTIVE_PRESENT_FLAG: u8 = 0x01;
const RESULT_PRESENT_FLAG: u8 = 0x02;
const KNOWN_FLAGS: u8 = ACTIVE_PRESENT_FLAG | RESULT_PRESENT_FLAG;

const MAX_PHASE_BYTES: usize = 1024;
const MAX_MESSAGE_BYTES: usize = 4096;

const MAX_ACTIVE_PATH_BYTES: usize = 32 * 1024;

const MAX_RECEIVER_ENDPOINT_BYTES: usize = 128;

const UPDATE_EXISTING_FLAG: u8 = 0x01;
const KNOWN_RECEIVER_FLAGS: u8 = UPDATE_EXISTING_FLAG;

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

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ManagementActiveJobDetails {
    Sender {
        receiver_address: SocketAddr,

        source_root: String,

        worker_count: usize,

        calibration_mib: u64,
    },

    Receiver {
        transfer_port: u16,

        destination_root: String,

        update_existing: bool,
    },
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

    pub details: ManagementActiveJobDetails,
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManagementAgentSnapshot {
    pub agent_instance_id: AgentInstanceId,

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

    payload.extend_from_slice(&snapshot.agent_instance_id.to_le_bytes());

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

    let agent_instance_id = AgentInstanceId::from_le_bytes(
        payload[4..SNAPSHOT_HEADER_BYTES].try_into().map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "management agent instance ID was malformed",
            )
        })?,
    )
    .ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "management agent instance ID was zero",
        )
    })?;

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
        agent_instance_id,

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

    let details = encode_active_details(active.role, &active.details)?;

    let details_length = u32::try_from(details.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "active management details length cannot be represented",
        )
    })?;

    payload.push(active.role as u8);

    payload.push(u8::from(active.cancel_requested));

    payload.extend_from_slice(&phase_length.to_le_bytes());

    payload.extend_from_slice(&active.job_id.to_le_bytes());

    payload.extend_from_slice(&active.completed.to_le_bytes());

    payload.extend_from_slice(&active.total.to_le_bytes());

    payload.extend_from_slice(&details_length.to_le_bytes());

    payload.extend_from_slice(active.phase.as_bytes());

    payload.extend_from_slice(&details);

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

    let details_length = usize::try_from(u32::from_le_bytes(header[28..32].try_into().map_err(
        |_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "active details length was malformed",
            )
        },
    )?))
    .map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "active details length cannot be represented",
        )
    })?;

    let phase_bytes = take_bytes(payload, cursor, phase_length, "active management phase")?;

    let phase = decode_text(
        phase_bytes,
        MAX_PHASE_BYTES,
        "management progress phase",
        false,
    )?;

    let details_bytes = take_bytes(payload, cursor, details_length, "active management details")?;

    let details = decode_active_details(role, details_bytes)?;

    Ok(ManagementActiveJobSnapshot {
        role,

        job_id,

        phase,

        completed,

        total,

        cancel_requested,

        details,
    })
}

fn encode_active_details(
    role: ManagementJobRole,
    details: &ManagementActiveJobDetails,
) -> io::Result<Vec<u8>> {
    match (role, details) {
        (
            ManagementJobRole::Sender,
            ManagementActiveJobDetails::Sender {
                receiver_address,
                source_root,
                worker_count,
                calibration_mib,
            },
        ) => {
            if receiver_address.port() == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "active sender receiver address used port zero",
                ));
            }

            if *worker_count == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "active sender worker count must not be zero",
                ));
            }

            if *calibration_mib == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "active sender calibration size must not be zero",
                ));
            }

            validate_text(
                source_root,
                MAX_ACTIVE_PATH_BYTES,
                "active sender source path",
                false,
            )?;

            let receiver_text = receiver_address.to_string();

            validate_text(
                &receiver_text,
                MAX_RECEIVER_ENDPOINT_BYTES,
                "active sender receiver endpoint",
                false,
            )?;

            let receiver_length = u16::try_from(receiver_text.len()).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "active sender receiver endpoint length cannot be represented",
                )
            })?;

            let source_length = u32::try_from(source_root.len()).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "active sender source path length cannot be represented",
                )
            })?;

            let worker_count = u32::try_from(*worker_count).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "active sender worker count cannot be represented",
                )
            })?;

            let mut encoded = Vec::with_capacity(
                SENDER_DETAIL_HEADER_BYTES + receiver_text.len() + source_root.len(),
            );

            encoded.extend_from_slice(&worker_count.to_le_bytes());

            encoded.extend_from_slice(&calibration_mib.to_le_bytes());

            encoded.extend_from_slice(&receiver_length.to_le_bytes());

            encoded.extend_from_slice(&0_u16.to_le_bytes());

            encoded.extend_from_slice(&source_length.to_le_bytes());

            encoded.extend_from_slice(receiver_text.as_bytes());

            encoded.extend_from_slice(source_root.as_bytes());

            Ok(encoded)
        }

        (
            ManagementJobRole::Receiver,
            ManagementActiveJobDetails::Receiver {
                transfer_port,
                destination_root,
                update_existing,
            },
        ) => {
            if *transfer_port == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "active receiver transfer port must not be zero",
                ));
            }

            validate_text(
                destination_root,
                MAX_ACTIVE_PATH_BYTES,
                "active receiver destination path",
                false,
            )?;

            let destination_length = u32::try_from(destination_root.len()).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "active receiver destination length cannot be represented",
                )
            })?;

            let flags = if *update_existing {
                UPDATE_EXISTING_FLAG
            } else {
                0
            };

            let mut encoded =
                Vec::with_capacity(RECEIVER_DETAIL_HEADER_BYTES + destination_root.len());

            encoded.extend_from_slice(&transfer_port.to_le_bytes());

            encoded.push(flags);
            encoded.push(0);

            encoded.extend_from_slice(&destination_length.to_le_bytes());

            encoded.extend_from_slice(destination_root.as_bytes());

            Ok(encoded)
        }

        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "active management role did not match its detail payload",
        )),
    }
}

fn decode_active_details(
    role: ManagementJobRole,
    payload: &[u8],
) -> io::Result<ManagementActiveJobDetails> {
    match role {
        ManagementJobRole::Sender => {
            if payload.len() < SENDER_DETAIL_HEADER_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "active sender details ended inside its header",
                ));
            }

            let worker_count = usize::try_from(u32::from_le_bytes(
                payload[0..4].try_into().map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "active sender worker count was malformed",
                    )
                })?,
            ))
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "active sender worker count cannot be represented",
                )
            })?;

            if worker_count == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "active sender worker count was zero",
                ));
            }

            let calibration_mib = u64::from_le_bytes(payload[4..12].try_into().map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "active sender calibration size was malformed",
                )
            })?);

            if calibration_mib == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "active sender calibration size was zero",
                ));
            }

            let receiver_length = usize::from(u16::from_le_bytes(
                payload[12..14].try_into().map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "active sender receiver length was malformed",
                    )
                })?,
            ));

            let reserved = u16::from_le_bytes(payload[14..16].try_into().map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "active sender reserved field was malformed",
                )
            })?);

            if reserved != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "active sender reserved field was not zero",
                ));
            }

            let source_length = usize::try_from(u32::from_le_bytes(
                payload[16..20].try_into().map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "active sender source length was malformed",
                    )
                })?,
            ))
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "active sender source length cannot be represented",
                )
            })?;

            let receiver_end = SENDER_DETAIL_HEADER_BYTES
                .checked_add(receiver_length)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "active sender receiver position overflowed",
                    )
                })?;

            let expected_length = receiver_end.checked_add(source_length).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "active sender detail length overflowed",
                )
            })?;

            if payload.len() != expected_length {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "active sender details contain {} bytes, expected {expected_length}",
                        payload.len(),
                    ),
                ));
            }

            let receiver_text = decode_text(
                &payload[SENDER_DETAIL_HEADER_BYTES..receiver_end],
                MAX_RECEIVER_ENDPOINT_BYTES,
                "active sender receiver endpoint",
                false,
            )?;

            let receiver_address = receiver_text.parse::<SocketAddr>().map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("active sender receiver endpoint was invalid: {error}"),
                )
            })?;

            if receiver_address.port() == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "active sender receiver endpoint used port zero",
                ));
            }

            let source_root = decode_text(
                &payload[receiver_end..],
                MAX_ACTIVE_PATH_BYTES,
                "active sender source path",
                false,
            )?;

            Ok(ManagementActiveJobDetails::Sender {
                receiver_address,

                source_root,

                worker_count,

                calibration_mib,
            })
        }

        ManagementJobRole::Receiver => {
            if payload.len() < RECEIVER_DETAIL_HEADER_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "active receiver details ended inside its header",
                ));
            }

            let transfer_port = u16::from_le_bytes(payload[0..2].try_into().map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "active receiver transfer port was malformed",
                )
            })?);

            if transfer_port == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "active receiver transfer port was zero",
                ));
            }

            let flags = payload[2];

            if flags & !KNOWN_RECEIVER_FLAGS != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("active receiver flags contained unknown bits 0x{flags:02X}"),
                ));
            }

            if payload[3] != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "active receiver reserved byte was not zero",
                ));
            }

            let destination_length = usize::try_from(u32::from_le_bytes(
                payload[4..8].try_into().map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "active receiver destination length was malformed",
                    )
                })?,
            ))
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "active receiver destination length cannot be represented",
                )
            })?;

            let expected_length = RECEIVER_DETAIL_HEADER_BYTES
                .checked_add(destination_length)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "active receiver detail length overflowed",
                    )
                })?;

            if payload.len() != expected_length {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "active receiver details contain {} bytes, expected {expected_length}",
                        payload.len(),
                    ),
                ));
            }

            let destination_root = decode_text(
                &payload[RECEIVER_DETAIL_HEADER_BYTES..],
                MAX_ACTIVE_PATH_BYTES,
                "active receiver destination path",
                false,
            )?;

            Ok(ManagementActiveJobDetails::Receiver {
                transfer_port,

                destination_root,

                update_existing: flags & UPDATE_EXISTING_FLAG != 0,
            })
        }
    }
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
        ManagementActiveJobDetails, ManagementActiveJobSnapshot, ManagementAgentSnapshot,
        ManagementJobOutcome, ManagementJobResult, ManagementJobRole, SNAPSHOT_HEADER_BYTES,
        decode_snapshot, encode_snapshot,
    };
    use crate::management_instance::AgentInstanceId;

    fn instance_id() -> AgentInstanceId {
        AgentInstanceId::from_raw(0x0011_2233_4455_6677_8899_AABB_CCDD_EEFF).unwrap()
    }

    #[test]
    fn complete_snapshot_round_trips() {
        let expected = ManagementAgentSnapshot {
            agent_instance_id: instance_id(),

            active: Some(ManagementActiveJobSnapshot {
                role: ManagementJobRole::Sender,

                job_id: 17,

                phase: "Transfer send".to_string(),

                completed: 512 * 1024,

                total: 1024 * 1024,

                cancel_requested: false,

                details: ManagementActiveJobDetails::Sender {
                    receiver_address: "127.0.0.1:7337".parse().unwrap(),

                    source_root: r"C:\Source".to_string(),

                    worker_count: 4,

                    calibration_mib: 8,
                },
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
        let expected = ManagementAgentSnapshot {
            agent_instance_id: instance_id(),

            active: None,

            latest_result: None,
        };

        let encoded = encode_snapshot(&expected).unwrap();

        let decoded = decode_snapshot(&encoded).unwrap();

        assert_eq!(decoded, expected);
    }

    #[test]
    fn decoder_rejects_trailing_bytes() {
        let mut encoded = encode_snapshot(&ManagementAgentSnapshot {
            agent_instance_id: instance_id(),

            active: None,

            latest_result: None,
        })
        .unwrap();

        encoded.push(0xAA);

        let error = decode_snapshot(&encoded).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData,);
    }

    #[test]
    fn decoder_rejects_zero_instance_id() {
        let snapshot = ManagementAgentSnapshot {
            agent_instance_id: instance_id(),

            active: None,

            latest_result: None,
        };

        let mut encoded = encode_snapshot(&snapshot).unwrap();

        encoded[4..SNAPSHOT_HEADER_BYTES].fill(0);

        let error = decode_snapshot(&encoded).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData,);
    }
}
