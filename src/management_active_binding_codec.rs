use crate::management_active_binding::ActiveQueueBinding;
use crate::management_instance::AgentInstanceId;
use crate::management_queue::QueuedTransferId;
use std::io;

const PRESENT_FIELD: &str = "active_binding_present";

const QUEUE_ID_FIELD: &str = "active_binding_queue_id";

const SENDER_INSTANCE_FIELD: &str = "active_binding_sender_instance";

const SENDER_JOB_FIELD: &str = "active_binding_sender_job_id";

const RECEIVER_INSTANCE_FIELD: &str = "active_binding_receiver_instance";

const RECEIVER_JOB_FIELD: &str = "active_binding_receiver_job_id";

pub fn encode_active_binding(binding: Option<ActiveQueueBinding>, output: &mut String) {
    match binding {
        None => {
            push_field(output, PRESENT_FIELD, 0);
        }

        Some(binding) => {
            push_field(output, PRESENT_FIELD, 1);

            push_field(output, QUEUE_ID_FIELD, binding.queue_id.get());

            push_field(output, SENDER_INSTANCE_FIELD, binding.sender_instance_id);

            push_field(output, SENDER_JOB_FIELD, binding.sender_job_id);

            push_field(
                output,
                RECEIVER_INSTANCE_FIELD,
                binding.receiver_instance_id,
            );

            push_field(output, RECEIVER_JOB_FIELD, binding.receiver_job_id);
        }
    }
}

pub fn decode_active_binding(
    lines: &mut std::str::Lines<'_>,
) -> io::Result<Option<ActiveQueueBinding>> {
    let present = parse_bool_field(required_line(lines)?, PRESENT_FIELD)?;

    if !present {
        return Ok(None);
    }

    let queue_id_raw = parse_number_field::<u64>(required_line(lines)?, QUEUE_ID_FIELD)?;

    let queue_id = QueuedTransferId::from_raw(queue_id_raw).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "active queue binding used queue ID zero",
        )
    })?;

    let sender_instance_id = parse_instance_field(required_line(lines)?, SENDER_INSTANCE_FIELD)?;

    let sender_job_id = parse_number_field::<u64>(required_line(lines)?, SENDER_JOB_FIELD)?;

    let receiver_instance_id =
        parse_instance_field(required_line(lines)?, RECEIVER_INSTANCE_FIELD)?;

    let receiver_job_id = parse_number_field::<u64>(required_line(lines)?, RECEIVER_JOB_FIELD)?;

    ActiveQueueBinding::new(
        queue_id,
        sender_instance_id,
        sender_job_id,
        receiver_instance_id,
        receiver_job_id,
    )
    .map(Some)
    .map_err(invalid_data)
}

fn push_field(output: &mut String, key: &str, value: impl std::fmt::Display) {
    output.push_str(key);
    output.push(' ');
    output.push_str(&value.to_string());
    output.push('\n');
}

fn required_line<'a>(lines: &mut std::str::Lines<'a>) -> io::Result<&'a str> {
    lines.next().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "active queue binding ended unexpectedly",
        )
    })
}

fn field_value<'a>(line: &'a str, expected_key: &str) -> io::Result<&'a str> {
    let (actual_key, value) = line.split_once(' ').ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("active queue binding field {line:?} has no value",),
        )
    })?;

    if actual_key != expected_key {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("active queue binding expected field {expected_key:?}, found {actual_key:?}",),
        ));
    }

    Ok(value)
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
            format!("active queue binding field {key:?} was invalid: {error}",),
        )
    })
}

fn parse_bool_field(line: &str, key: &str) -> io::Result<bool> {
    match parse_number_field::<u8>(line, key)? {
        0 => Ok(false),

        1 => Ok(true),

        unknown => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("active queue binding field {key:?} used invalid boolean {unknown}",),
        )),
    }
}

fn parse_instance_field(line: &str, key: &str) -> io::Result<AgentInstanceId> {
    let value = field_value(line, key)?;

    value.parse::<AgentInstanceId>().map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("active queue binding field {key:?} was invalid: {error}",),
        )
    })
}

fn invalid_data(error: io::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

#[cfg(test)]
mod tests {
    use super::{decode_active_binding, encode_active_binding};
    use crate::management_active_binding::ActiveQueueBinding;
    use crate::management_instance::AgentInstanceId;
    use crate::management_queue::QueuedTransferId;
    use std::io;

    fn instance(value: u128) -> AgentInstanceId {
        AgentInstanceId::from_raw(value).unwrap()
    }

    fn binding() -> ActiveQueueBinding {
        ActiveQueueBinding::new(
            QueuedTransferId::from_raw(42).unwrap(),
            instance(0x0011_2233_4455_6677_8899_AABB_CCDD_EEFF),
            11,
            instance(0xFFEE_DDCC_BBAA_9988_7766_5544_3322_1100),
            17,
        )
        .unwrap()
    }

    fn decode_complete(encoded: &str) -> io::Result<Option<ActiveQueueBinding>> {
        let mut lines = encoded.lines();

        let decoded = decode_active_binding(&mut lines)?;

        if let Some(trailing) = lines.find(|line| !line.trim().is_empty()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("active queue binding contained trailing line {trailing:?}",),
            ));
        }

        Ok(decoded)
    }

    #[test]
    fn absent_binding_round_trips() {
        let mut encoded = String::new();

        encode_active_binding(None, &mut encoded);

        assert_eq!(encoded, "active_binding_present 0\n",);

        assert_eq!(decode_complete(&encoded).unwrap(), None,);
    }

    #[test]
    fn complete_binding_round_trips() {
        let expected = binding();

        let mut encoded = String::new();

        encode_active_binding(Some(expected), &mut encoded);

        assert_eq!(decode_complete(&encoded).unwrap(), Some(expected),);

        assert!(encoded.contains("active_binding_queue_id 42\n",),);

        assert!(encoded.contains("active_binding_sender_job_id 11\n",),);

        assert!(encoded.contains("active_binding_receiver_job_id 17\n",),);
    }

    #[test]
    fn invalid_presence_boolean_is_rejected() {
        let error = decode_complete("active_binding_present 2\n").unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData,);
    }

    #[test]
    fn zero_queue_id_is_rejected() {
        let encoded = concat!(
            "active_binding_present 1\n",
            "active_binding_queue_id 0\n",
            "active_binding_sender_instance 00112233445566778899AABBCCDDEEFF\n",
            "active_binding_sender_job_id 11\n",
            "active_binding_receiver_instance FFEEDDCCBBAA99887766554433221100\n",
            "active_binding_receiver_job_id 17\n",
        );

        let error = decode_complete(encoded).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData,);
    }

    #[test]
    fn malformed_instance_id_is_rejected() {
        let encoded = concat!(
            "active_binding_present 1\n",
            "active_binding_queue_id 42\n",
            "active_binding_sender_instance NOT-AN-INSTANCE\n",
            "active_binding_sender_job_id 11\n",
            "active_binding_receiver_instance FFEEDDCCBBAA99887766554433221100\n",
            "active_binding_receiver_job_id 17\n",
        );

        let error = decode_complete(encoded).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData,);
    }

    #[test]
    fn zero_job_id_is_rejected() {
        let encoded = concat!(
            "active_binding_present 1\n",
            "active_binding_queue_id 42\n",
            "active_binding_sender_instance 00112233445566778899AABBCCDDEEFF\n",
            "active_binding_sender_job_id 0\n",
            "active_binding_receiver_instance FFEEDDCCBBAA99887766554433221100\n",
            "active_binding_receiver_job_id 17\n",
        );

        let error = decode_complete(encoded).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData,);
    }

    #[test]
    fn missing_binding_field_is_rejected() {
        let encoded = concat!(
            "active_binding_present 1\n",
            "active_binding_queue_id 42\n",
            "active_binding_sender_instance 00112233445566778899AABBCCDDEEFF\n",
        );

        let error = decode_complete(encoded).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof,);
    }

    #[test]
    fn unexpected_field_name_is_rejected() {
        let encoded = concat!(
            "active_binding_present 1\n",
            "wrong_queue_field 42\n",
            "active_binding_sender_instance 00112233445566778899AABBCCDDEEFF\n",
            "active_binding_sender_job_id 11\n",
            "active_binding_receiver_instance FFEEDDCCBBAA99887766554433221100\n",
            "active_binding_receiver_job_id 17\n",
        );

        let error = decode_complete(encoded).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData,);
    }

    #[test]
    fn trailing_fields_are_detectable_by_owner() {
        let mut encoded = String::new();

        encode_active_binding(Some(binding()), &mut encoded);

        encoded.push_str("unexpected trailing field\n");

        let error = decode_complete(&encoded).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData,);
    }
}
