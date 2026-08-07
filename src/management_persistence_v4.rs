#[doc(hidden)]
pub mod implementation {
    include!("management_persistence.rs");

    use crate::management_active_binding_codec::{decode_active_binding, encode_active_binding};

    const STATE_MAGIC_V4: &str = "NCMS4";

    const STATE_MAGIC_V5: &str = "NCMS5";

    const ACTIVE_BINDING_PRESENT_PREFIX: &str = "active_binding_present ";

    const QUEUE_PRESERVE_DESKTOP_LAYOUT_PREFIX: &str = "queue_preserve_desktop_layout ";

    pub fn load_from_v4(path: &Path) -> io::Result<Option<ManagerPersistedState>> {
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

        decode_state_with_v5(&text).map(Some)
    }

    pub fn save_to_v4(path: &Path, state: &ManagerPersistedState) -> io::Result<()> {
        let encoded = encode_state_with_v5(state)?;

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

    fn encode_state_with_v5(state: &ManagerPersistedState) -> io::Result<String> {
        let encoded_v4 = encode_state_with_v4(state)?;

        let mut lines = encoded_v4.lines().map(str::to_string).collect::<Vec<_>>();

        let first = lines
            .first_mut()
            .ok_or_else(|| io::Error::other("NCMS4 encoder produced an empty manager state"))?;

        if first != STATE_MAGIC_V4 {
            return Err(io::Error::other(
                "NCMS4 encoder produced an unexpected manager-state format",
            ));
        }

        *first = STATE_MAGIC_V5.to_string();

        let mut queue_index = 0_usize;

        let mut line_index = 0_usize;

        while line_index < lines.len() {
            if lines[line_index].starts_with("queue_update_existing ") {
                let item = state.queue.items().get(queue_index).ok_or_else(|| {
                    io::Error::other("NCMS5 encoder found more queue fields than queue items")
                })?;

                lines.insert(
                    line_index + 1,
                    format!(
                        "{QUEUE_PRESERVE_DESKTOP_LAYOUT_PREFIX}{}",
                        u8::from(item.preserve_desktop_layout,),
                    ),
                );

                queue_index += 1;

                line_index += 1;
            }

            line_index += 1;
        }

        if queue_index != state.queue.len() {
            return Err(io::Error::other(format!(
                "NCMS5 encoder found {queue_index} queue entries in the legacy state but expected {}",
                state.queue.len(),
            )));
        }

        let mut output = lines.join("\n");

        output.push('\n');

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

    fn decode_state_with_v5(text: &str) -> io::Result<ManagerPersistedState> {
        if text.len() > MAX_STATE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "manager state contains {} bytes, exceeding the {MAX_STATE_BYTES} byte limit",
                    text.len(),
                ),
            ));
        }

        let first_line = text.lines().next().ok_or_else(|| {
            io::Error::new(io::ErrorKind::UnexpectedEof, "manager state was empty")
        })?;

        if first_line != STATE_MAGIC_V5 {
            return decode_state_with_v4(text);
        }

        let mut legacy_lines = Vec::new();

        let mut desktop_layout_flags = Vec::new();

        for (index, line) in text.lines().enumerate() {
            if index == 0 {
                legacy_lines.push(STATE_MAGIC_V4.to_string());

                continue;
            }

            if line.starts_with(QUEUE_PRESERVE_DESKTOP_LAYOUT_PREFIX) {
                desktop_layout_flags.push(parse_bool_field(line, "queue_preserve_desktop_layout")?);

                continue;
            }

            legacy_lines.push(line.to_string());
        }

        let mut legacy_text = legacy_lines.join("\n");

        legacy_text.push('\n');

        let mut state = decode_state_with_v4(&legacy_text)?;

        if desktop_layout_flags.len() != state.queue.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "NCMS5 manager state contained {} Desktop-layout queue flags for {} queue entries",
                    desktop_layout_flags.len(),
                    state.queue.len(),
                ),
            ));
        }

        for (item, preserve_desktop_layout) in
            state.queue.items_mut().iter_mut().zip(desktop_layout_flags)
        {
            item.preserve_desktop_layout = preserve_desktop_layout;
        }

        validate_state(&state).map_err(invalid_data)?;

        Ok(state)
    }

    fn encode_state_with_v4(state: &ManagerPersistedState) -> io::Result<String> {
        let mut output = encode_state(state)?;

        if !output.starts_with(STATE_MAGIC_V3) {
            return Err(io::Error::other(
                "legacy manager-state encoder did not produce NCMS3",
            ));
        }

        output.replace_range(..STATE_MAGIC_V3.len(), STATE_MAGIC_V4);

        let history_marker = "\nhistory_count ";

        let insertion_index = output
            .find(history_marker)
            .map(|index| index + 1)
            .ok_or_else(|| {
                io::Error::other("legacy manager-state encoder omitted history_count")
            })?;

        let mut binding_text = String::new();

        encode_active_binding(state.queue.active_binding(), &mut binding_text);

        output.insert_str(insertion_index, &binding_text);

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

    fn decode_state_with_v4(text: &str) -> io::Result<ManagerPersistedState> {
        if text.len() > MAX_STATE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "manager state contains {} bytes, exceeding the {MAX_STATE_BYTES} byte limit",
                    text.len(),
                ),
            ));
        }

        let first_line = text.lines().next().ok_or_else(|| {
            io::Error::new(io::ErrorKind::UnexpectedEof, "manager state was empty")
        })?;

        if first_line != STATE_MAGIC_V4 {
            return decode_state(text);
        }

        let mut lines = text.lines().collect::<Vec<_>>();

        let history_index = lines
            .iter()
            .position(|line| line.starts_with("history_count "))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "NCMS4 manager state omitted history_count",
                )
            })?;

        let binding_positions = lines[..history_index]
            .iter()
            .enumerate()
            .filter_map(|(index, line)| {
                line.starts_with(ACTIVE_BINDING_PRESENT_PREFIX)
                    .then_some(index)
            })
            .collect::<Vec<_>>();

        let binding_index = match binding_positions.as_slice() {
            [index] => *index,

            [] => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "NCMS4 manager state omitted its active-binding block",
                ));
            }

            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "NCMS4 manager state contained multiple active-binding blocks",
                ));
            }
        };

        let preceding_line = binding_index
            .checked_sub(1)
            .and_then(|index| lines.get(index))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "NCMS4 active-binding block appeared before the queue",
                )
            })?;

        if *preceding_line != "queue_end" && !preceding_line.starts_with("queue_count ") {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "NCMS4 active-binding block did not immediately follow the queue",
            ));
        }

        let mut binding_text = lines[binding_index..history_index].join("\n");

        binding_text.push('\n');

        let mut binding_lines = binding_text.lines();

        let active_binding = decode_active_binding(&mut binding_lines)?;

        if let Some(trailing) = binding_lines.find(|line| !line.trim().is_empty()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("NCMS4 active-binding block contained trailing line {trailing:?}",),
            ));
        }

        lines.drain(binding_index..history_index);

        lines[0] = STATE_MAGIC_V3;

        let mut legacy_text = lines.join("\n");

        legacy_text.push('\n');

        let mut state = decode_state(&legacy_text)?;

        if let Some(binding) = active_binding {
            state
                .queue
                .set_active_binding(binding)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        }

        validate_state(&state).map_err(invalid_data)?;

        Ok(state)
    }

    #[cfg(test)]
    mod v4_tests {
        use super::{
            ManagerPersistedState, decode_state, decode_state_with_v4, decode_state_with_v5,
            encode_state, encode_state_with_v4, encode_state_with_v5, load_from_v4, save_to_v4,
        };
        use crate::management_active_binding::ActiveQueueBinding;
        use crate::management_instance::AgentInstanceId;
        use crate::management_queue::{
            QueuedTransferKind, QueuedTransferRequest, QueuedTransferState, TransferQueue,
        };
        use crate::management_route::ManagementRouteMode;
        use std::fs;
        use std::process;
        use std::time::{SystemTime, UNIX_EPOCH};

        fn instance(value: u128) -> AgentInstanceId {
            AgentInstanceId::from_raw(value).unwrap()
        }

        fn bound_state() -> ManagerPersistedState {
            let mut state = ManagerPersistedState::default();

            let mut queue = TransferQueue::default();

            let queue_id = queue
                .add(QueuedTransferRequest {
                    sender_agent: "127.0.0.1:7339".parse().unwrap(),

                    receiver_agent: "127.0.0.2:7339".parse().unwrap(),

                    route_mode: ManagementRouteMode::AutomaticLan,

                    source_root: r"C:\Users\User\Desktop".to_string(),

                    destination_root: r"D:\Backup\Desktop".to_string(),

                    update_existing: true,

                    worker_count: 4,

                    calibration_mib: 8,

                    kind: QueuedTransferKind::Fresh,
                })
                .unwrap();

            queue
                .set_state(queue_id, QueuedTransferState::Running, "Transfer active")
                .unwrap();

            queue
                .set_active_binding(
                    ActiveQueueBinding::new(
                        queue_id,
                        instance(0x0011_2233_4455_6677_8899_AABB_CCDD_EEFF),
                        11,
                        instance(0xFFEE_DDCC_BBAA_9988_7766_5544_3322_1100),
                        17,
                    )
                    .unwrap(),
                )
                .unwrap();

            state.queue = queue;

            state
        }

        #[test]
        fn version_five_round_trips_desktop_layout_choice() {
            let mut expected = bound_state();

            expected.queue.items_mut()[0].preserve_desktop_layout = true;

            let encoded = encode_state_with_v5(&expected).unwrap();

            assert!(encoded.starts_with("NCMS5\n"),);

            assert!(encoded.contains("queue_preserve_desktop_layout 1\n",));

            let decoded = decode_state_with_v5(&encoded).unwrap();

            assert_eq!(decoded, expected);

            assert!(decoded.queue.items()[0].preserve_desktop_layout,);
        }

        #[test]
        fn version_four_migrates_desktop_layout_choice_off() {
            let expected = bound_state();

            let encoded = encode_state_with_v4(&expected).unwrap();

            let decoded = decode_state_with_v5(&encoded).unwrap();

            assert!(
                decoded
                    .queue
                    .items()
                    .iter()
                    .all(|item| { !item.preserve_desktop_layout }),
            );
        }

        #[test]
        fn version_four_state_round_trips_binding() {
            let expected = bound_state();

            let encoded = encode_state_with_v4(&expected).unwrap();

            assert!(encoded.starts_with("NCMS4\n",),);

            assert!(encoded.contains("active_binding_present 1\n",),);

            let decoded = decode_state_with_v4(&encoded).unwrap();

            assert_eq!(decoded, expected);
        }

        #[test]
        fn version_four_state_round_trips_without_binding() {
            let expected = ManagerPersistedState::default();

            let encoded = encode_state_with_v4(&expected).unwrap();

            assert!(encoded.contains("active_binding_present 0\n",),);

            let decoded = decode_state_with_v4(&encoded).unwrap();

            assert_eq!(decoded, expected);
        }

        #[test]
        fn version_three_state_migrates_without_binding() {
            let mut expected = bound_state();

            expected.queue.clear_active_binding();

            let encoded = encode_state(&expected).unwrap();

            assert!(encoded.starts_with("NCMS3\n",),);

            let decoded = decode_state_with_v4(&encoded).unwrap();

            assert_eq!(decoded, expected);

            assert_eq!(decoded.queue.active_binding(), None,);
        }

        #[test]
        fn legacy_decoder_still_reads_generated_version_three_state() {
            let mut expected = bound_state();

            expected.queue.clear_active_binding();

            let encoded = encode_state(&expected).unwrap();

            assert_eq!(decode_state(&encoded).unwrap(), expected,);
        }

        #[test]
        fn version_four_rejects_binding_for_pending_item() {
            let encoded = encode_state_with_v4(&bound_state()).unwrap();

            let invalid = encoded.replacen("queue_state 1\n", "queue_state 0\n", 1);

            let error = decode_state_with_v4(&invalid).unwrap_err();

            assert_eq!(error.kind(), std::io::ErrorKind::InvalidData,);
        }

        #[test]
        fn version_four_rejects_extra_binding_fields() {
            let encoded = encode_state_with_v4(&bound_state()).unwrap();

            let invalid = encoded.replacen(
                "\nhistory_count ",
                "\nunexpected_binding_field 1\nhistory_count ",
                1,
            );

            let error = decode_state_with_v4(&invalid).unwrap_err();

            assert_eq!(error.kind(), std::io::ErrorKind::InvalidData,);
        }

        #[test]
        fn version_four_rejects_missing_binding_block() {
            let encoded = encode_state_with_v4(&ManagerPersistedState::default()).unwrap();

            let invalid = encoded.replace("active_binding_present 0\n", "");

            let error = decode_state_with_v4(&invalid).unwrap_err();

            assert_eq!(error.kind(), std::io::ErrorKind::InvalidData,);
        }

        #[test]
        fn version_five_saves_and_loads_atomically() {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();

            let directory = std::env::temp_dir().join(format!(
                "networkcopy-manager-state-v4-{}-{unique}",
                process::id(),
            ));

            let path = directory.join("manager-state.txt");

            let expected = bound_state();

            save_to_v4(&path, &expected).unwrap();

            let saved = fs::read_to_string(&path).unwrap();

            assert!(saved.starts_with("NCMS5\n",),);

            let loaded = load_from_v4(&path).unwrap().unwrap();

            assert_eq!(loaded, expected);

            fs::remove_dir_all(directory).unwrap();
        }
    }
}

pub use implementation::{ManagerHistoryEntry, ManagerPersistedState, default_state_path};

pub use implementation::{load_from_v4 as load_from, save_to_v4 as save_to};
