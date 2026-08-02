use std::fmt;
use std::io;
use std::ptr::null_mut;
use std::str::FromStr;
use windows_sys::Win32::Security::Cryptography::{
    BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom,
};

const INSTANCE_ID_BYTES: usize = 16;

const INSTANCE_ID_BYTES_U32: u32 = 16;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct AgentInstanceId(u128);

impl AgentInstanceId {
    pub const BYTE_COUNT: usize = INSTANCE_ID_BYTES;

    pub fn generate() -> io::Result<Self> {
        let mut bytes = [0_u8; INSTANCE_ID_BYTES];

        let status = unsafe {
            BCryptGenRandom(
                null_mut(),
                bytes.as_mut_ptr(),
                INSTANCE_ID_BYTES_U32,
                BCRYPT_USE_SYSTEM_PREFERRED_RNG,
            )
        };

        if status < 0 {
            return Err(io::Error::other(format!(
                "BCryptGenRandom failed with NTSTATUS 0x{:08X}",
                status as u32,
            )));
        }

        Self::from_le_bytes(bytes).ok_or_else(|| {
            io::Error::other("Windows generated an invalid all-zero agent instance ID")
        })
    }

    pub const fn from_raw(value: u128) -> Option<Self> {
        if value == 0 { None } else { Some(Self(value)) }
    }

    pub const fn get(self) -> u128 {
        self.0
    }

    pub const fn from_le_bytes(bytes: [u8; INSTANCE_ID_BYTES]) -> Option<Self> {
        Self::from_raw(u128::from_le_bytes(bytes))
    }

    pub const fn to_le_bytes(self) -> [u8; INSTANCE_ID_BYTES] {
        self.0.to_le_bytes()
    }
}

impl fmt::Display for AgentInstanceId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:032X}", self.0,)
    }
}

impl FromStr for AgentInstanceId {
    type Err = io::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let value = value.trim();

        if value.len() != 32 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "agent instance ID must contain exactly 32 hexadecimal characters, found {}",
                    value.len(),
                ),
            ));
        }

        let raw = u128::from_str_radix(value, 16).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("agent instance ID is not valid hexadecimal: {error}",),
            )
        })?;

        Self::from_raw(raw).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "agent instance ID must not be zero",
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::AgentInstanceId;

    #[test]
    fn generated_identity_is_nonzero() {
        let identity = AgentInstanceId::generate().unwrap();

        assert_ne!(identity.get(), 0);
    }

    #[test]
    fn identity_text_round_trips() {
        let expected =
            AgentInstanceId::from_raw(0x0011_2233_4455_6677_8899_AABB_CCDD_EEFF).unwrap();

        let encoded = expected.to_string();

        assert_eq!(encoded, "00112233445566778899AABBCCDDEEFF",);

        assert_eq!(encoded.parse::<AgentInstanceId>().unwrap(), expected,);

        assert_eq!(
            encoded
                .to_ascii_lowercase()
                .parse::<AgentInstanceId>()
                .unwrap(),
            expected,
        );
    }

    #[test]
    fn zero_identity_is_rejected() {
        assert!(AgentInstanceId::from_raw(0).is_none(),);

        assert!(
            "00000000000000000000000000000000"
                .parse::<AgentInstanceId>()
                .is_err(),
        );
    }
}
