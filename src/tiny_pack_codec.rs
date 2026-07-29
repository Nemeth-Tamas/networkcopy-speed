use crate::compression_probe;
use std::io;

const MODE_RAW: u8 = 0;

const MODE_ZSTD: u8 = 1;

pub(crate) const MAX_TINY_PACK_BYTES: usize = 8 * 1024 * 1024;

pub(crate) const MAX_TINY_PACK_WIRE_BYTES: usize = MAX_TINY_PACK_BYTES * 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TinyPackEncoding {
    Raw,
    Zstandard,
}

impl TinyPackEncoding {
    pub(crate) const fn wire_value(self) -> u8 {
        match self {
            Self::Raw => MODE_RAW,

            Self::Zstandard => MODE_ZSTD,
        }
    }

    pub(crate) fn from_wire(value: u8) -> io::Result<Self> {
        match value {
            MODE_RAW => Ok(Self::Raw),

            MODE_ZSTD => Ok(Self::Zstandard),

            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown tiny-pack encoding mode 0x{value:02X}"),
            )),
        }
    }
}

#[derive(Debug)]
pub(crate) struct EncodedTinyPack {
    encoding: TinyPackEncoding,

    compressed: Option<Vec<u8>>,
}

impl EncodedTinyPack {
    pub(crate) const fn encoding(&self) -> TinyPackEncoding {
        self.encoding
    }

    pub(crate) const fn is_compressed(&self) -> bool {
        matches!(self.encoding, TinyPackEncoding::Zstandard)
    }

    pub(crate) fn wire_payload<'a>(&'a self, raw_payload: &'a [u8]) -> &'a [u8] {
        match &self.compressed {
            Some(compressed) => compressed,

            None => raw_payload,
        }
    }
}

pub(crate) fn encode(raw_payload: &[u8], level: i32) -> io::Result<EncodedTinyPack> {
    compression_probe::validate_level(level)?;

    validate_logical_size(raw_payload.len())?;

    if raw_payload.is_empty() {
        return Ok(EncodedTinyPack {
            encoding: TinyPackEncoding::Raw,

            compressed: None,
        });
    }

    let mut compressor = zstd::bulk::Compressor::new(level).map_err(|error| {
        io::Error::other(format!(
            "failed to create tiny-pack Zstandard compressor: {error}"
        ))
    })?;

    let compressed = compressor.compress(raw_payload).map_err(|error| {
        io::Error::other(format!("tiny-pack Zstandard compression failed: {error}"))
    })?;

    validate_wire_size(compressed.len())?;

    let raw_bytes = u64::try_from(raw_payload.len())
        .map_err(|_| io::Error::other("tiny-pack raw length cannot be represented"))?;

    let compressed_bytes = u64::try_from(compressed.len())
        .map_err(|_| io::Error::other("tiny-pack compressed length cannot be represented"))?;

    if compression_probe::should_compress_sizes(raw_bytes, compressed_bytes) {
        Ok(EncodedTinyPack {
            encoding: TinyPackEncoding::Zstandard,

            compressed: Some(compressed),
        })
    } else {
        Ok(EncodedTinyPack {
            encoding: TinyPackEncoding::Raw,

            compressed: None,
        })
    }
}

pub(crate) fn decode(
    encoding: TinyPackEncoding,
    wire_payload: &[u8],
    expected_logical_bytes: usize,
    output: &mut [u8],
) -> io::Result<()> {
    validate_logical_size(expected_logical_bytes)?;

    validate_wire_size(wire_payload.len())?;

    let output = output.get_mut(..expected_logical_bytes).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "tiny-pack decode buffer is too small",
        )
    })?;

    match encoding {
        TinyPackEncoding::Raw => {
            if wire_payload.len() != expected_logical_bytes {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "raw tiny pack contains {} bytes, expected {expected_logical_bytes}",
                        wire_payload.len(),
                    ),
                ));
            }

            output.copy_from_slice(wire_payload);

            Ok(())
        }

        TinyPackEncoding::Zstandard => {
            if wire_payload.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "compressed tiny-pack payload is empty",
                ));
            }

            let mut decompressor = zstd::bulk::Decompressor::new().map_err(|error| {
                io::Error::other(format!(
                    "failed to create tiny-pack Zstandard decompressor: {error}"
                ))
            })?;

            let decompressed = decompressor
                .decompress_to_buffer(wire_payload, output)
                .map_err(|error| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("tiny-pack Zstandard decompression failed: {error}"),
                    )
                })?;

            if decompressed != expected_logical_bytes {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "tiny pack decompressed to {decompressed} bytes, expected {expected_logical_bytes}",
                    ),
                ));
            }

            Ok(())
        }
    }
}

fn validate_logical_size(bytes: usize) -> io::Result<()> {
    if bytes > MAX_TINY_PACK_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "tiny pack contains {bytes} bytes, exceeding the {MAX_TINY_PACK_BYTES}-byte limit"
            ),
        ));
    }

    Ok(())
}

fn validate_wire_size(bytes: usize) -> io::Result<()> {
    if bytes > MAX_TINY_PACK_WIRE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "tiny-pack wire payload contains {bytes} bytes, exceeding the {MAX_TINY_PACK_WIRE_BYTES}-byte limit"
            ),
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{TinyPackEncoding, decode, encode};
    use crate::compression_probe;

    #[test]
    fn repetitive_pack_uses_zstandard() {
        let raw = vec![b'A'; 1024 * 1024];

        let encoded = encode(&raw, compression_probe::DEFAULT_LEVEL).unwrap();

        assert_eq!(encoded.encoding(), TinyPackEncoding::Zstandard);

        assert!(encoded.is_compressed());

        let wire = encoded.wire_payload(&raw);

        assert!(wire.len() < raw.len() / 10);

        let mut decoded = vec![0_u8; raw.len()];

        decode(encoded.encoding(), wire, raw.len(), &mut decoded).unwrap();

        assert_eq!(decoded, raw);
    }

    #[test]
    fn incompressible_pack_stays_raw() {
        let mut state = 0x4D59_5DF4_D0F3_3173_u64;

        let mut raw = vec![0_u8; 1024 * 1024];

        for byte in &mut raw {
            state ^= state << 13;

            state ^= state >> 7;

            state ^= state << 17;

            *byte = state as u8;
        }

        let encoded = encode(&raw, compression_probe::DEFAULT_LEVEL).unwrap();

        assert_eq!(encoded.encoding(), TinyPackEncoding::Raw);

        assert!(!encoded.is_compressed());

        assert_eq!(encoded.wire_payload(&raw), raw);
    }

    #[test]
    fn invalid_encoding_is_rejected() {
        let error = TinyPackEncoding::from_wire(0xFF).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }
}
