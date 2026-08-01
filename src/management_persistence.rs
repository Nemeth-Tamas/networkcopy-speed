use crate::management_orchestration::ManagedTransferRecord;
use crate::management_queue::{
    MAX_QUEUE_ENTRIES, QueuedTransfer, QueuedTransferId, QueuedTransferKind, QueuedTransferRequest,
    QueuedTransferState, TransferQueue,
};
use crate::management_snapshot::{ManagementJobOutcome, ManagementJobResult, ManagementJobRole};
use std::env;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

const STATE_MAGIC_V1: &str = "NCMS1";

const STATE_MAGIC_V2: &str = "NCMS2";

const MAX_STATE_BYTES: usize = 4 * 1024 * 1024;

const MAX_HISTORY_ENTRIES: usize = 20;

const MAX_TEXT_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManagerHistoryEntry {
    pub transfer: ManagedTransferRecord,

    pub sender_result: ManagementJobResult,

    pub receiver_result: ManagementJobResult,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManagerPersistedState {
    pub sender_agent: String,

    pub receiver_agent: String,

    pub source_root: String,

    pub destination_root: String,

    pub worker_count: usize,

    pub calibration_mib: u64,

    pub update_existing: bool,

    pub queue: TransferQueue,

    pub history: Vec<ManagerHistoryEntry>,
}

impl Default for ManagerPersistedState {
    fn default() -> Self {
        Self {
            sender_agent: String::new(),

            receiver_agent: String::new(),

            source_root: String::new(),

            destination_root: String::new(),

            worker_count: 4,

            calibration_mib: 8,

            update_existing: false,

            queue: TransferQueue::default(),

            history: Vec::new(),
        }
    }
}

pub fn default_state_path() -> io::Result<PathBuf> {
    let local_app_data = env::var_os("LOCALAPPDATA")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "LOCALAPPDATA is not available"))?;

    Ok(PathBuf::from(local_app_data)
        .join("NetworkCopy Speed Edition")
        .join("manager-state.txt"))
}

pub fn load_from(path: &Path) -> io::Result<Option<ManagerPersistedState>> {
    let mut file = match File::open(path) {
        Ok(file) => file,

        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(None);
        }

        Err(error) => {
            return Err(error);
        }
    };

    let metadata = file.metadata()?;

    if metadata.len() > MAX_STATE_BYTES as u64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "manager state contains {} bytes, exceeding the {MAX_STATE_BYTES} byte limit",
                metadata.len(),
            ),
        ));
    }

    let capacity = usize::try_from(metadata.len()).unwrap_or(MAX_STATE_BYTES);

    let mut text = String::with_capacity(capacity);

    file.read_to_string(&mut text)?;

    decode_state(&text).map(Some)
}

pub fn save_to(path: &Path, state: &ManagerPersistedState) -> io::Result<()> {
    let encoded = encode_state(state)?;

    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "manager state path has no parent directory",
        )
    })?;

    fs::create_dir_all(parent)?;

    let temporary = path.with_extension("tmp");

    let backup = path.with_extension("bak");

    {
        let mut file = File::create(&temporary)?;

        file.write_all(encoded.as_bytes())?;

        file.sync_all()?;
    }

    if backup.exists() {
        fs::remove_file(&backup)?;
    }

    let had_existing = path.exists();

    if had_existing {
        fs::rename(path, &backup)?;
    }

    match fs::rename(&temporary, path) {
        Ok(()) => {
            if had_existing && backup.exists() {
                fs::remove_file(backup)?;
            }

            Ok(())
        }

        Err(error) => {
            if had_existing && backup.exists() {
                let _ = fs::rename(&backup, path);
            }

            let _ = fs::remove_file(&temporary);

            Err(error)
        }
    }
}

fn encode_state(state: &ManagerPersistedState) -> io::Result<String> {
    validate_state(state)?;

    let mut output = String::new();

    push_line(&mut output, STATE_MAGIC_V2);

    push_text_field(&mut output, "sender_agent", &state.sender_agent);

    push_text_field(&mut output, "receiver_agent", &state.receiver_agent);

    push_text_field(&mut output, "source_root", &state.source_root);

    push_text_field(&mut output, "destination_root", &state.destination_root);

    push_number_field(&mut output, "worker_count", state.worker_count);

    push_number_field(&mut output, "calibration_mib", state.calibration_mib);

    push_bool_field(&mut output, "update_existing", state.update_existing);

    encode_queue(&state.queue, &mut output);

    push_number_field(&mut output, "history_count", state.history.len());

    for entry in &state.history {
        encode_history_entry(entry, &mut output);
    }

    push_line(&mut output, "end");

    if output.len() > MAX_STATE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "manager state requires {} bytes, exceeding the {MAX_STATE_BYTES} byte limit",
                output.len(),
            ),
        ));
    }

    Ok(output)
}

fn decode_state(text: &str) -> io::Result<ManagerPersistedState> {
    if text.len() > MAX_STATE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "manager state contains {} bytes, exceeding the {MAX_STATE_BYTES} byte limit",
                text.len(),
            ),
        ));
    }

    let mut lines = text.lines();

    let has_queue = match required_line(&mut lines)? {
        STATE_MAGIC_V1 => false,
        STATE_MAGIC_V2 => true,

        unknown => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("manager state used unsupported format {unknown:?}"),
            ));
        }
    };

    let sender_agent = decode_text_field(required_line(&mut lines)?, "sender_agent")?;

    let receiver_agent = decode_text_field(required_line(&mut lines)?, "receiver_agent")?;

    let source_root = decode_text_field(required_line(&mut lines)?, "source_root")?;

    let destination_root = decode_text_field(required_line(&mut lines)?, "destination_root")?;

    let worker_count = parse_usize_field(required_line(&mut lines)?, "worker_count")?;

    let calibration_mib = parse_u64_field(required_line(&mut lines)?, "calibration_mib")?;

    let update_existing = parse_bool_field(required_line(&mut lines)?, "update_existing")?;

    let queue = if has_queue {
        decode_queue(&mut lines)?
    } else {
        TransferQueue::default()
    };

    let history_count = parse_usize_field(required_line(&mut lines)?, "history_count")?;

    if history_count > MAX_HISTORY_ENTRIES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "manager state contains {history_count} history entries, exceeding the {MAX_HISTORY_ENTRIES} entry limit"
            ),
        ));
    }

    let mut history = Vec::with_capacity(history_count);

    for _ in 0..history_count {
        history.push(decode_history_entry(&mut lines)?);
    }

    expect_line(&mut lines, "end")?;

    if let Some(trailing) = lines.find(|line| !line.trim().is_empty()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("manager state contains trailing line {trailing:?}"),
        ));
    }

    let state = ManagerPersistedState {
        sender_agent,

        receiver_agent,

        source_root,

        destination_root,

        worker_count,

        calibration_mib,

        update_existing,

        queue,

        history,
    };

    validate_state(&state).map_err(invalid_data)?;

    Ok(state)
}

fn encode_queue(queue: &TransferQueue, output: &mut String) {
    push_number_field(output, "queue_next_id", queue.next_id());

    push_bool_field(
        output,
        "queue_paused_after_current",
        queue.paused_after_current(),
    );

    push_number_field(output, "queue_count", queue.len());

    for item in queue.items() {
        push_line(output, "queue_begin");

        push_number_field(output, "queue_id", item.id.get());

        let (kind, resume_stream_count) = match item.request.kind {
            QueuedTransferKind::Fresh => (0_u8, 0),

            QueuedTransferKind::Resume { data_stream_count } => (1_u8, data_stream_count),
        };

        push_number_field(output, "queue_kind", kind);

        push_number_field(output, "queue_resume_stream_count", resume_stream_count);

        push_number_field(output, "queue_state", queue_state_code(item.state));

        push_text_field(
            output,
            "queue_sender_agent",
            &item.request.sender_agent.to_string(),
        );

        push_text_field(
            output,
            "queue_receiver_agent",
            &item.request.receiver_agent.to_string(),
        );

        push_text_field(output, "queue_source_root", &item.request.source_root);

        push_text_field(
            output,
            "queue_destination_root",
            &item.request.destination_root,
        );

        push_bool_field(
            output,
            "queue_update_existing",
            item.request.update_existing,
        );

        push_number_field(output, "queue_worker_count", item.request.worker_count);

        push_number_field(
            output,
            "queue_calibration_mib",
            item.request.calibration_mib,
        );

        push_text_field(output, "queue_status_message", &item.status_message);

        push_line(output, "queue_end");
    }
}

fn decode_queue(lines: &mut std::str::Lines<'_>) -> io::Result<TransferQueue> {
    let next_id = parse_u64_field(required_line(lines)?, "queue_next_id")?;

    let paused_after_current =
        parse_bool_field(required_line(lines)?, "queue_paused_after_current")?;

    let count = parse_usize_field(required_line(lines)?, "queue_count")?;

    if count > MAX_QUEUE_ENTRIES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "manager state contains {count} queue entries, exceeding the {MAX_QUEUE_ENTRIES} entry limit"
            ),
        ));
    }

    let mut items = Vec::with_capacity(count);

    for _ in 0..count {
        items.push(decode_queue_entry(lines)?);
    }

    TransferQueue::from_parts(next_id, paused_after_current, items)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn decode_queue_entry(lines: &mut std::str::Lines<'_>) -> io::Result<QueuedTransfer> {
    expect_line(lines, "queue_begin")?;

    let id_value = parse_u64_field(required_line(lines)?, "queue_id")?;

    let id = QueuedTransferId::from_raw(id_value).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "persisted queue entry used ID zero",
        )
    })?;

    let kind_code = parse_u8_field(required_line(lines)?, "queue_kind")?;

    let resume_stream_count =
        parse_usize_field(required_line(lines)?, "queue_resume_stream_count")?;

    let kind = match (kind_code, resume_stream_count) {
        (0, 0) => QueuedTransferKind::Fresh,

        (0, _) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "fresh queued transfer used a resume stream count",
            ));
        }

        (1, 0) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "resumed queued transfer used stream count zero",
            ));
        }

        (1, data_stream_count) => QueuedTransferKind::Resume { data_stream_count },

        (unknown, _) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("queued transfer used unknown kind {unknown}"),
            ));
        }
    };

    let state = decode_queue_state(parse_u8_field(required_line(lines)?, "queue_state")?)?;

    let sender_agent = parse_socket_address(
        &decode_text_field(required_line(lines)?, "queue_sender_agent")?,
        "queued sender agent",
    )?;

    let receiver_agent = parse_socket_address(
        &decode_text_field(required_line(lines)?, "queue_receiver_agent")?,
        "queued receiver agent",
    )?;

    let source_root = decode_text_field(required_line(lines)?, "queue_source_root")?;

    let destination_root = decode_text_field(required_line(lines)?, "queue_destination_root")?;

    let update_existing = parse_bool_field(required_line(lines)?, "queue_update_existing")?;

    let worker_count = parse_usize_field(required_line(lines)?, "queue_worker_count")?;

    let calibration_mib = parse_u64_field(required_line(lines)?, "queue_calibration_mib")?;

    let status_message = decode_text_field(required_line(lines)?, "queue_status_message")?;

    expect_line(lines, "queue_end")?;

    Ok(QueuedTransfer {
        id,

        request: QueuedTransferRequest {
            sender_agent,

            receiver_agent,

            source_root,

            destination_root,

            update_existing,

            worker_count,

            calibration_mib,

            kind,
        },

        state,

        status_message,
    })
}

const fn queue_state_code(state: QueuedTransferState) -> u8 {
    match state {
        QueuedTransferState::Pending => 0,
        QueuedTransferState::Running => 1,
        QueuedTransferState::Blocked => 2,
        QueuedTransferState::Failed => 3,
        QueuedTransferState::Completed => 4,
        QueuedTransferState::Cancelled => 5,
    }
}

fn decode_queue_state(value: u8) -> io::Result<QueuedTransferState> {
    match value {
        0 => Ok(QueuedTransferState::Pending),
        1 => Ok(QueuedTransferState::Running),
        2 => Ok(QueuedTransferState::Blocked),
        3 => Ok(QueuedTransferState::Failed),
        4 => Ok(QueuedTransferState::Completed),
        5 => Ok(QueuedTransferState::Cancelled),

        unknown => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("queued transfer used unknown state {unknown}"),
        )),
    }
}

fn encode_history_entry(entry: &ManagerHistoryEntry, output: &mut String) {
    push_line(output, "history_begin");

    let transfer = &entry.transfer;

    push_text_field(output, "sender_agent", &transfer.sender_agent.to_string());

    push_number_field(output, "sender_job_id", transfer.sender_job_id);

    push_text_field(
        output,
        "receiver_agent",
        &transfer.receiver_agent.to_string(),
    );

    push_number_field(output, "receiver_job_id", transfer.receiver_job_id);

    push_text_field(
        output,
        "receiver_payload",
        &transfer.receiver_payload.to_string(),
    );

    push_text_field(output, "source_root", &transfer.source_root);

    push_text_field(output, "destination_root", &transfer.destination_root);

    push_bool_field(output, "update_existing", transfer.update_existing);

    push_number_field(output, "worker_count", transfer.worker_count);

    push_number_field(output, "calibration_mib", transfer.calibration_mib);

    push_line(output, "sender_result");

    encode_result(&entry.sender_result, output);

    push_line(output, "receiver_result");

    encode_result(&entry.receiver_result, output);

    push_line(output, "history_end");
}

fn decode_history_entry(lines: &mut std::str::Lines<'_>) -> io::Result<ManagerHistoryEntry> {
    expect_line(lines, "history_begin")?;

    let sender_agent = parse_socket_address(
        &decode_text_field(required_line(lines)?, "sender_agent")?,
        "history sender agent",
    )?;

    let sender_job_id = parse_u64_field(required_line(lines)?, "sender_job_id")?;

    let receiver_agent = parse_socket_address(
        &decode_text_field(required_line(lines)?, "receiver_agent")?,
        "history receiver agent",
    )?;

    let receiver_job_id = parse_u64_field(required_line(lines)?, "receiver_job_id")?;

    let receiver_payload = parse_socket_address(
        &decode_text_field(required_line(lines)?, "receiver_payload")?,
        "history receiver payload",
    )?;

    let source_root = decode_text_field(required_line(lines)?, "source_root")?;

    let destination_root = decode_text_field(required_line(lines)?, "destination_root")?;

    let update_existing = parse_bool_field(required_line(lines)?, "update_existing")?;

    let worker_count = parse_usize_field(required_line(lines)?, "worker_count")?;

    let calibration_mib = parse_u64_field(required_line(lines)?, "calibration_mib")?;

    expect_line(lines, "sender_result")?;

    let sender_result = decode_result(lines)?;

    expect_line(lines, "receiver_result")?;

    let receiver_result = decode_result(lines)?;

    expect_line(lines, "history_end")?;

    if sender_result.role != ManagementJobRole::Sender || sender_result.job_id != sender_job_id {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "persisted sender result does not match its transfer record",
        ));
    }

    if receiver_result.role != ManagementJobRole::Receiver
        || receiver_result.job_id != receiver_job_id
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "persisted receiver result does not match its transfer record",
        ));
    }

    Ok(ManagerHistoryEntry {
        transfer: ManagedTransferRecord {
            sender_agent,

            sender_job_id,

            receiver_agent,

            receiver_job_id,

            receiver_payload,

            source_root,

            destination_root,

            update_existing,

            worker_count,

            calibration_mib,
        },

        sender_result,

        receiver_result,
    })
}

fn encode_result(result: &ManagementJobResult, output: &mut String) {
    push_number_field(output, "role", result.role as u8);

    push_number_field(output, "outcome", result.outcome as u8);

    push_number_field(output, "job_id", result.job_id);

    push_number_field(output, "files", result.files);

    push_number_field(output, "logical_bytes", result.logical_bytes);

    push_number_field(output, "wire_bytes", result.wire_bytes);

    push_number_field(output, "data_stream_count", result.data_stream_count);

    push_text_field(output, "message", &result.message);
}

fn decode_result(lines: &mut std::str::Lines<'_>) -> io::Result<ManagementJobResult> {
    let role = ManagementJobRole::try_from(parse_u8_field(required_line(lines)?, "role")?)?;

    let outcome =
        ManagementJobOutcome::try_from(parse_u8_field(required_line(lines)?, "outcome")?)?;

    let job_id = parse_u64_field(required_line(lines)?, "job_id")?;

    let files = parse_u64_field(required_line(lines)?, "files")?;

    let logical_bytes = parse_u64_field(required_line(lines)?, "logical_bytes")?;

    let wire_bytes = parse_u64_field(required_line(lines)?, "wire_bytes")?;

    let data_stream_count = parse_u32_field(required_line(lines)?, "data_stream_count")?;

    let message = decode_text_field(required_line(lines)?, "message")?;

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

fn validate_state(state: &ManagerPersistedState) -> io::Result<()> {
    if state.worker_count == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "persisted worker count must not be zero",
        ));
    }

    if state.calibration_mib == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "persisted calibration size must not be zero",
        ));
    }

    if state.history.len() > MAX_HISTORY_ENTRIES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "manager state contains {} history entries, exceeding the {MAX_HISTORY_ENTRIES} entry limit",
                state.history.len(),
            ),
        ));
    }

    validate_text(&state.sender_agent, "sender agent")?;

    validate_text(&state.receiver_agent, "receiver agent")?;

    validate_text(&state.source_root, "source root")?;

    validate_text(&state.destination_root, "destination root")?;

    state
        .queue
        .validate()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;

    for entry in &state.history {
        validate_history_entry(entry)?;
    }

    Ok(())
}

fn validate_history_entry(entry: &ManagerHistoryEntry) -> io::Result<()> {
    let transfer = &entry.transfer;

    if transfer.sender_job_id == 0 || transfer.receiver_job_id == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "persisted transfer used job ID zero",
        ));
    }

    if transfer.worker_count == 0 || transfer.calibration_mib == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "persisted transfer used zero workers or calibration size",
        ));
    }

    validate_text(&transfer.source_root, "history source root")?;

    validate_text(&transfer.destination_root, "history destination root")?;

    validate_text(&entry.sender_result.message, "sender result message")?;

    validate_text(&entry.receiver_result.message, "receiver result message")?;

    if entry.sender_result.role != ManagementJobRole::Sender
        || entry.sender_result.job_id != transfer.sender_job_id
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "sender result does not match its persisted transfer",
        ));
    }

    if entry.receiver_result.role != ManagementJobRole::Receiver
        || entry.receiver_result.job_id != transfer.receiver_job_id
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "receiver result does not match its persisted transfer",
        ));
    }

    Ok(())
}

fn validate_text(value: &str, description: &str) -> io::Result<()> {
    if value.len() > MAX_TEXT_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "{description} contains {} bytes, exceeding the {MAX_TEXT_BYTES} byte limit",
                value.len(),
            ),
        ));
    }

    Ok(())
}

fn push_line(output: &mut String, line: &str) {
    output.push_str(line);
    output.push('\n');
}

fn push_text_field(output: &mut String, key: &str, value: &str) {
    push_line(output, &format!("{key} {}", hex_encode(value.as_bytes()),));
}

fn push_number_field(output: &mut String, key: &str, value: impl std::fmt::Display) {
    push_line(output, &format!("{key} {value}"));
}

fn push_bool_field(output: &mut String, key: &str, value: bool) {
    push_number_field(output, key, u8::from(value));
}

fn required_line<'a>(lines: &mut std::str::Lines<'a>) -> io::Result<&'a str> {
    lines.next().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "manager state ended unexpectedly",
        )
    })
}

fn expect_line(lines: &mut std::str::Lines<'_>, expected: &str) -> io::Result<()> {
    let actual = required_line(lines)?;

    if actual != expected {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("manager state expected line {expected:?}, found {actual:?}"),
        ));
    }

    Ok(())
}

fn field_value<'a>(line: &'a str, expected_key: &str) -> io::Result<&'a str> {
    let (actual_key, value) = line.split_once(' ').ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("manager state field {line:?} has no value"),
        )
    })?;

    if actual_key != expected_key {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("manager state expected field {expected_key:?}, found {actual_key:?}"),
        ));
    }

    Ok(value)
}

fn decode_text_field(line: &str, key: &str) -> io::Result<String> {
    let encoded = field_value(line, key)?;

    let bytes = hex_decode(encoded)?;

    if bytes.len() > MAX_TEXT_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{key} contains {} bytes, exceeding the {MAX_TEXT_BYTES} byte limit",
                bytes.len(),
            ),
        ));
    }

    String::from_utf8(bytes).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{key} was not valid UTF-8: {error}"),
        )
    })
}

fn parse_u8_field(line: &str, key: &str) -> io::Result<u8> {
    parse_number_field(line, key)
}

fn parse_u32_field(line: &str, key: &str) -> io::Result<u32> {
    parse_number_field(line, key)
}

fn parse_u64_field(line: &str, key: &str) -> io::Result<u64> {
    parse_number_field(line, key)
}

fn parse_usize_field(line: &str, key: &str) -> io::Result<usize> {
    parse_number_field(line, key)
}

fn parse_number_field<T>(line: &str, key: &str) -> io::Result<T>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    let value = field_value(line, key)?;

    value.parse::<T>().map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("manager state field {key:?} was invalid: {error}"),
        )
    })
}

fn parse_bool_field(line: &str, key: &str) -> io::Result<bool> {
    match parse_u8_field(line, key)? {
        0 => Ok(false),
        1 => Ok(true),

        unknown => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("manager state field {key:?} used invalid boolean {unknown}"),
        )),
    }
}

fn parse_socket_address(value: &str, description: &str) -> io::Result<SocketAddr> {
    value.parse::<SocketAddr>().map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{description} was invalid: {error}"),
        )
    })
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";

    let mut encoded = String::with_capacity(bytes.len() * 2);

    for byte in bytes {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));

        encoded.push(char::from(HEX[usize::from(byte & 0x0F)]));
    }

    encoded
}

fn hex_decode(encoded: &str) -> io::Result<Vec<u8>> {
    if !encoded.len().is_multiple_of(2) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "hexadecimal manager-state field has odd length",
        ));
    }

    let bytes = encoded.as_bytes();

    let mut decoded = Vec::with_capacity(bytes.len() / 2);

    for pair in bytes.chunks_exact(2) {
        let high = decode_hex_nibble(pair[0])?;

        let low = decode_hex_nibble(pair[1])?;

        decoded.push(high << 4 | low);
    }

    Ok(decoded)
}

fn decode_hex_nibble(value: u8) -> io::Result<u8> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),

        b'A'..=b'F' => Ok(value - b'A' + 10),

        b'a'..=b'f' => Ok(value - b'a' + 10),

        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid hexadecimal digit 0x{value:02X}"),
        )),
    }
}

fn invalid_data(error: io::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

#[cfg(test)]
mod tests {
    use super::{
        ManagerHistoryEntry, ManagerPersistedState, decode_state, encode_state, load_from, save_to,
    };
    use crate::management_orchestration::ManagedTransferRecord;
    use crate::management_queue::{
        QueuedTransferKind, QueuedTransferRequest, QueuedTransferState, TransferQueue,
    };
    use crate::management_snapshot::{
        ManagementJobOutcome, ManagementJobResult, ManagementJobRole,
    };
    use std::fs;
    use std::process;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn example_state() -> ManagerPersistedState {
        let transfer = ManagedTransferRecord {
            sender_agent: "127.0.0.1:7339".parse().unwrap(),

            sender_job_id: 11,

            receiver_agent: "127.0.0.2:7339".parse().unwrap(),

            receiver_job_id: 17,

            receiver_payload: "127.0.0.2:7337".parse().unwrap(),

            source_root: r"C:\Source".to_string(),

            destination_root: r"D:\Destination".to_string(),

            update_existing: true,

            worker_count: 4,

            calibration_mib: 8,
        };

        let mut queue = TransferQueue::default();

        let desktop = queue
            .add(QueuedTransferRequest {
                sender_agent: "127.0.0.1:7339".parse().unwrap(),

                receiver_agent: "127.0.0.2:7339".parse().unwrap(),

                source_root: r"C:\Users\User\Desktop".to_string(),

                destination_root: r"D:\Backup\Desktop".to_string(),

                update_existing: true,

                worker_count: 4,

                calibration_mib: 8,

                kind: QueuedTransferKind::Fresh,
            })
            .unwrap();

        queue
            .set_state(
                desktop,
                QueuedTransferState::Completed,
                "Transfer completed",
            )
            .unwrap();

        let documents = queue
            .add(QueuedTransferRequest {
                sender_agent: "127.0.0.1:7339".parse().unwrap(),

                receiver_agent: "127.0.0.2:7339".parse().unwrap(),

                source_root: r"C:\Users\User\Documents".to_string(),

                destination_root: r"D:\Backup\Documents".to_string(),

                update_existing: true,

                worker_count: 4,

                calibration_mib: 8,

                kind: QueuedTransferKind::Resume {
                    data_stream_count: 4,
                },
            })
            .unwrap();

        queue
            .set_state(
                documents,
                QueuedTransferState::Failed,
                "Transfer interrupted",
            )
            .unwrap();

        queue.set_paused_after_current(true);

        ManagerPersistedState {
            sender_agent: "127.0.0.1:7339".to_string(),

            receiver_agent: "127.0.0.2:7339".to_string(),

            source_root: r"C:\Source".to_string(),

            destination_root: r"D:\Destination".to_string(),

            worker_count: 4,

            calibration_mib: 8,

            update_existing: true,

            queue,

            history: vec![ManagerHistoryEntry {
                sender_result: ManagementJobResult {
                    role: ManagementJobRole::Sender,

                    outcome: ManagementJobOutcome::Completed,

                    job_id: 11,

                    files: 42,

                    logical_bytes: 1_000_000,

                    wire_bytes: 750_000,

                    data_stream_count: 4,

                    message: String::new(),
                },

                receiver_result: ManagementJobResult {
                    role: ManagementJobRole::Receiver,

                    outcome: ManagementJobOutcome::Completed,

                    job_id: 17,

                    files: 42,

                    logical_bytes: 1_000_000,

                    wire_bytes: 0,

                    data_stream_count: 4,

                    message: String::new(),
                },

                transfer,
            }],
        }
    }

    fn encode_v1_state(state: &ManagerPersistedState) -> String {
        let mut output = String::new();

        super::push_line(&mut output, super::STATE_MAGIC_V1);

        super::push_text_field(&mut output, "sender_agent", &state.sender_agent);

        super::push_text_field(&mut output, "receiver_agent", &state.receiver_agent);

        super::push_text_field(&mut output, "source_root", &state.source_root);

        super::push_text_field(&mut output, "destination_root", &state.destination_root);

        super::push_number_field(&mut output, "worker_count", state.worker_count);

        super::push_number_field(&mut output, "calibration_mib", state.calibration_mib);

        super::push_bool_field(&mut output, "update_existing", state.update_existing);

        super::push_number_field(&mut output, "history_count", state.history.len());

        for entry in &state.history {
            super::encode_history_entry(entry, &mut output);
        }

        super::push_line(&mut output, "end");

        output
    }

    #[test]
    fn manager_state_round_trips() {
        let expected = example_state();

        let encoded = encode_state(&expected).unwrap();

        assert!(encoded.starts_with("NCMS2\n"));

        let decoded = decode_state(&encoded).unwrap();

        assert_eq!(decoded, expected);
    }

    #[test]
    fn version_one_state_loads_with_empty_queue() {
        let source = example_state();

        let encoded = encode_v1_state(&source);

        let mut expected = source;

        expected.queue = TransferQueue::default();

        let decoded = decode_state(&encoded).unwrap();

        assert_eq!(decoded, expected);
    }

    #[test]
    fn manager_state_saves_and_loads() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();

        let directory = std::env::temp_dir().join(format!(
            "networkcopy-manager-state-{}-{unique}",
            process::id(),
        ));

        let path = directory.join("manager-state.txt");

        let expected = example_state();

        save_to(&path, &expected).unwrap();

        let loaded = load_from(&path).unwrap().unwrap();

        assert_eq!(loaded, expected);

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn missing_state_is_not_an_error() {
        let path =
            std::env::temp_dir().join(format!("networkcopy-missing-state-{}.txt", process::id(),));

        let _ = fs::remove_file(&path);

        assert!(load_from(&path).unwrap().is_none(),);
    }
}
