use crate::compression_probe::{self, CompressionDecision};
use crate::content_hash::{self, ContentHasher};
use crate::striped_file;
use std::fs::File;
use std::io::{self, Read, Write};

const MODE_RAW: u8 = 0;
const MODE_ZSTD: u8 = 1;

const DIGEST_BYTES: usize = 32;

pub(crate) const COMPRESSION_CHUNK_BYTES: usize = 1024 * 1024;

pub(crate) const MAX_COMPRESSED_CHUNK_BYTES: usize = COMPRESSION_CHUNK_BYTES * 2;

pub(crate) struct PayloadEncoder {
    compressor: zstd::bulk::Compressor<'static>,
    compressed_buffer: Vec<u8>,
}

impl PayloadEncoder {
    pub(crate) fn new(level: i32) -> io::Result<Self> {
        compression_probe::validate_level(level)?;

        let compressor = zstd::bulk::Compressor::new(level).map_err(|error| {
            io::Error::other(format!("failed to create Zstandard compressor: {error}"))
        })?;

        Ok(Self {
            compressor,
            compressed_buffer: vec![0_u8; MAX_COMPRESSED_CHUNK_BYTES],
        })
    }

    pub(crate) fn send_sequential(
        &mut self,
        writer: &mut impl Write,
        reader: &mut impl Read,
        byte_count: u64,
        raw_buffer: &mut [u8],
        decision: CompressionDecision,
    ) -> io::Result<bool> {
        self.send_payload(writer, byte_count, raw_buffer, decision, |chunk, _| {
            reader.read_exact(chunk)
        })
    }

    pub(crate) fn send_positional(
        &mut self,
        writer: &mut impl Write,
        file: &File,
        offset: u64,
        byte_count: u64,
        raw_buffer: &mut [u8],
        decision: CompressionDecision,
    ) -> io::Result<bool> {
        self.send_payload(
            writer,
            byte_count,
            raw_buffer,
            decision,
            |chunk, transferred| {
                let read_offset = offset
                    .checked_add(transferred)
                    .ok_or_else(|| io::Error::other("compressed read offset overflowed"))?;

                read_exact_at(file, chunk, read_offset)
            },
        )
    }

    fn send_payload<F>(
        &mut self,
        writer: &mut impl Write,
        byte_count: u64,
        raw_buffer: &mut [u8],
        decision: CompressionDecision,
        mut fill_chunk: F,
    ) -> io::Result<bool>
    where
        F: FnMut(&mut [u8], u64) -> io::Result<()>,
    {
        if raw_buffer.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "compression buffer must not be empty",
            ));
        }

        write_mode(writer, decision)?;

        let mut hasher = ContentHasher::new();
        let mut transferred = 0_u64;

        while transferred < byte_count {
            let remaining = byte_count - transferred;

            let requested =
                remaining.min(COMPRESSION_CHUNK_BYTES.min(raw_buffer.len()) as u64) as usize;

            let raw_chunk = &mut raw_buffer[..requested];

            fill_chunk(raw_chunk, transferred)?;
            hasher.update(raw_chunk);

            match decision {
                CompressionDecision::SendRaw => {
                    writer.write_all(raw_chunk)?;
                }

                CompressionDecision::Compress => {
                    let compressed_length = self
                        .compressor
                        .compress_to_buffer(raw_chunk, &mut self.compressed_buffer)
                        .map_err(|error| {
                            io::Error::other(format!(
                                "Zstandard payload compression failed: {error}"
                            ))
                        })?;

                    write_u32(
                        writer,
                        u32::try_from(requested)
                            .map_err(|_| io::Error::other("raw compression chunk is too large"))?,
                    )?;

                    write_u32(
                        writer,
                        u32::try_from(compressed_length)
                            .map_err(|_| io::Error::other("compressed chunk is too large"))?,
                    )?;

                    writer.write_all(&self.compressed_buffer[..compressed_length])?;
                }
            }

            transferred = transferred
                .checked_add(u64::try_from(requested).map_err(|_| {
                    io::Error::other("compression chunk length cannot be represented")
                })?)
                .ok_or_else(|| io::Error::other("compressed send byte count overflowed"))?;
        }

        writer.write_all(&hasher.finalize())?;

        Ok(decision == CompressionDecision::Compress)
    }
}

pub(crate) struct PayloadDecoder {
    decompressor: zstd::bulk::Decompressor<'static>,

    compressed_buffer: Vec<u8>,
}

impl PayloadDecoder {
    pub(crate) fn new() -> io::Result<Self> {
        let decompressor = zstd::bulk::Decompressor::new().map_err(|error| {
            io::Error::other(format!("failed to create Zstandard decompressor: {error}"))
        })?;

        Ok(Self {
            decompressor,
            compressed_buffer: Vec::with_capacity(MAX_COMPRESSED_CHUNK_BYTES),
        })
    }

    pub(crate) fn receive_sequential(
        &mut self,
        reader: &mut impl Read,
        writer: &mut impl Write,
        byte_count: u64,
        raw_buffer: &mut [u8],
        context: &str,
    ) -> io::Result<bool> {
        self.receive_payload(reader, byte_count, raw_buffer, context, |chunk, _| {
            writer.write_all(chunk)
        })
    }

    pub(crate) fn receive_positional(
        &mut self,
        reader: &mut impl Read,
        file: &File,
        offset: u64,
        byte_count: u64,
        raw_buffer: &mut [u8],
        context: &str,
    ) -> io::Result<bool> {
        self.receive_payload(
            reader,
            byte_count,
            raw_buffer,
            context,
            |chunk, transferred| {
                let write_offset = offset
                    .checked_add(transferred)
                    .ok_or_else(|| io::Error::other("decompressed write offset overflowed"))?;

                striped_file::write_all_at(file, chunk, write_offset)
            },
        )
    }

    fn receive_payload<F>(
        &mut self,
        reader: &mut impl Read,
        byte_count: u64,
        raw_buffer: &mut [u8],
        context: &str,
        mut write_chunk: F,
    ) -> io::Result<bool>
    where
        F: FnMut(&[u8], u64) -> io::Result<()>,
    {
        if raw_buffer.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "decompression buffer must not be empty",
            ));
        }

        let decision = read_mode(reader)?;
        let mut hasher = ContentHasher::new();
        let mut transferred = 0_u64;

        while transferred < byte_count {
            match decision {
                CompressionDecision::SendRaw => {
                    let requested =
                        (byte_count - transferred).min(raw_buffer.len() as u64) as usize;

                    reader.read_exact(&mut raw_buffer[..requested])?;

                    let chunk = &raw_buffer[..requested];

                    hasher.update(chunk);

                    write_chunk(chunk, transferred)?;

                    transferred = transferred
                        .checked_add(u64::try_from(requested).map_err(|_| {
                            io::Error::other("raw payload length cannot be represented")
                        })?)
                        .ok_or_else(|| io::Error::other("raw receive byte count overflowed"))?;
                }

                CompressionDecision::Compress => {
                    let raw_length = usize::try_from(read_u32(reader)?).map_err(|_| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            "raw chunk length cannot be represented",
                        )
                    })?;

                    let compressed_length = usize::try_from(read_u32(reader)?).map_err(|_| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            "compressed chunk length cannot be represented",
                        )
                    })?;

                    let remaining = byte_count - transferred;

                    if raw_length == 0
                        || raw_length > COMPRESSION_CHUNK_BYTES
                        || raw_length > raw_buffer.len()
                        || raw_length as u64 > remaining
                    {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("received invalid raw compression chunk length {raw_length}"),
                        ));
                    }

                    if compressed_length == 0 || compressed_length > MAX_COMPRESSED_CHUNK_BYTES {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("received invalid compressed chunk length {compressed_length}"),
                        ));
                    }

                    self.compressed_buffer.resize(compressed_length, 0);

                    reader.read_exact(&mut self.compressed_buffer)?;

                    let decompressed_length = self
                        .decompressor
                        .decompress_to_buffer(
                            &self.compressed_buffer,
                            &mut raw_buffer[..raw_length],
                        )
                        .map_err(|error| {
                            io::Error::new(
                                io::ErrorKind::InvalidData,
                                format!("Zstandard payload decompression failed: {error}"),
                            )
                        })?;

                    if decompressed_length != raw_length {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!(
                                "compressed chunk expanded to {decompressed_length} bytes, expected {raw_length}"
                            ),
                        ));
                    }

                    let chunk = &raw_buffer[..raw_length];

                    hasher.update(chunk);

                    write_chunk(chunk, transferred)?;

                    transferred = transferred
                        .checked_add(u64::try_from(raw_length).map_err(|_| {
                            io::Error::other("decompressed payload length cannot be represented")
                        })?)
                        .ok_or_else(|| {
                            io::Error::other("decompressed receive byte count overflowed")
                        })?;
                }
            }
        }

        let actual_digest = hasher.finalize();

        let mut expected_digest = [0_u8; DIGEST_BYTES];

        reader.read_exact(&mut expected_digest)?;

        verify_digest(context, &actual_digest, &expected_digest)?;

        Ok(decision == CompressionDecision::Compress)
    }
}

fn read_exact_at(file: &File, mut buffer: &mut [u8], mut offset: u64) -> io::Result<()> {
    while !buffer.is_empty() {
        let read = striped_file::read_at_retry(file, buffer, offset)?;

        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "positional compression read ended unexpectedly",
            ));
        }

        buffer = &mut buffer[read..];

        offset =
            offset
                .checked_add(u64::try_from(read).map_err(|_| {
                    io::Error::other("positional read length cannot be represented")
                })?)
                .ok_or_else(|| io::Error::other("positional compression offset overflowed"))?;
    }

    Ok(())
}

fn write_mode(writer: &mut impl Write, decision: CompressionDecision) -> io::Result<()> {
    let mode = match decision {
        CompressionDecision::SendRaw => MODE_RAW,
        CompressionDecision::Compress => MODE_ZSTD,
    };

    writer.write_all(&[mode])
}

fn read_mode(reader: &mut impl Read) -> io::Result<CompressionDecision> {
    let mut mode = [0_u8; 1];
    reader.read_exact(&mut mode)?;

    match mode[0] {
        MODE_RAW => Ok(CompressionDecision::SendRaw),

        MODE_ZSTD => Ok(CompressionDecision::Compress),

        unknown => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("received unknown compression mode {unknown}"),
        )),
    }
}

fn write_u32(writer: &mut impl Write, value: u32) -> io::Result<()> {
    writer.write_all(&value.to_be_bytes())
}

fn read_u32(reader: &mut impl Read) -> io::Result<u32> {
    let mut value = [0_u8; 4];
    reader.read_exact(&mut value)?;
    Ok(u32::from_be_bytes(value))
}

fn verify_digest(
    context: &str,
    actual: &[u8; DIGEST_BYTES],
    expected: &[u8; DIGEST_BYTES],
) -> io::Result<()> {
    if actual == expected {
        return Ok(());
    }

    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        format!(
            "BLAKE3 verification failed for {context}: expected {}, calculated {}",
            content_hash::format_digest(expected),
            content_hash::format_digest(actual)
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::{COMPRESSION_CHUNK_BYTES, PayloadDecoder, PayloadEncoder};
    use crate::compression_probe::CompressionDecision;
    use std::io::Cursor;

    #[test]
    fn compressed_payload_round_trips() {
        let contents = vec![0x5A_u8; COMPRESSION_CHUNK_BYTES * 2 + 137];

        let mut encoder = PayloadEncoder::new(1).unwrap();

        let mut source = Cursor::new(&contents);

        let mut wire = Vec::new();

        let mut encode_buffer = vec![0_u8; COMPRESSION_CHUNK_BYTES];

        let compressed = encoder
            .send_sequential(
                &mut wire,
                &mut source,
                contents.len() as u64,
                &mut encode_buffer,
                CompressionDecision::Compress,
            )
            .unwrap();

        assert!(compressed);
        assert!(wire.len() < contents.len());

        let mut decoder = PayloadDecoder::new().unwrap();

        let mut wire_reader = Cursor::new(wire);

        let mut destination = Vec::new();

        let mut decode_buffer = vec![0_u8; COMPRESSION_CHUNK_BYTES];

        let received_compressed = decoder
            .receive_sequential(
                &mut wire_reader,
                &mut destination,
                contents.len() as u64,
                &mut decode_buffer,
                "compressed test payload",
            )
            .unwrap();

        assert!(received_compressed);
        assert_eq!(destination, contents);
    }

    #[test]
    fn raw_payload_round_trips() {
        let contents: Vec<u8> = (0..300_000).map(|index| (index % 251) as u8).collect();

        let mut encoder = PayloadEncoder::new(1).unwrap();

        let mut source = Cursor::new(&contents);

        let mut wire = Vec::new();
        let mut encode_buffer = vec![0_u8; 64 * 1024];

        let compressed = encoder
            .send_sequential(
                &mut wire,
                &mut source,
                contents.len() as u64,
                &mut encode_buffer,
                CompressionDecision::SendRaw,
            )
            .unwrap();

        assert!(!compressed);

        let mut decoder = PayloadDecoder::new().unwrap();

        let mut wire_reader = Cursor::new(wire);

        let mut destination = Vec::new();
        let mut decode_buffer = vec![0_u8; 64 * 1024];

        let received_compressed = decoder
            .receive_sequential(
                &mut wire_reader,
                &mut destination,
                contents.len() as u64,
                &mut decode_buffer,
                "raw test payload",
            )
            .unwrap();

        assert!(!received_compressed);
        assert_eq!(destination, contents);
    }
}
