use crate::compression_probe::{self, CompressionDecision};
use crate::console_progress::ProgressCounter;
use crate::content_hash::{self, ContentHasher};
use crate::striped_file;
use crate::transfer_profile::TransferProfiler;
use std::fs::File;
use std::io::{self, Read, Write};
use std::ops::Range;
use std::time::Instant;

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

    pub(crate) fn send_sequential_with_progress(
        &mut self,
        writer: &mut impl Write,
        reader: &mut impl Read,
        byte_count: u64,
        raw_buffer: &mut [u8],
        decision: CompressionDecision,
        progress: Option<&ProgressCounter>,
    ) -> io::Result<bool> {
        self.send_sequential_with_progress_profiled(
            writer,
            reader,
            byte_count,
            raw_buffer,
            decision,
            progress,
            None,
        )
    }

    pub(crate) fn send_sequential_with_progress_profiled(
        &mut self,
        writer: &mut impl Write,
        reader: &mut impl Read,
        byte_count: u64,
        raw_buffer: &mut [u8],
        decision: CompressionDecision,
        progress: Option<&ProgressCounter>,
        profiler: Option<&TransferProfiler>,
    ) -> io::Result<bool> {
        self.send_payload(
            writer,
            byte_count,
            raw_buffer,
            decision,
            progress,
            profiler,
            |chunk, _| reader.read_exact(chunk),
        )
    }

    pub(crate) fn send_positional_with_progress(
        &mut self,
        writer: &mut impl Write,
        file: &File,
        range: Range<u64>,
        raw_buffer: &mut [u8],
        decision: CompressionDecision,
        progress: Option<&ProgressCounter>,
    ) -> io::Result<bool> {
        self.send_positional_with_progress_profiled(
            writer,
            file,
            range,
            raw_buffer,
            decision,
            progress,
            None,
        )
    }

    pub(crate) fn send_positional_with_progress_profiled(
        &mut self,
        writer: &mut impl Write,
        file: &File,
        range: Range<u64>,
        raw_buffer: &mut [u8],
        decision: CompressionDecision,
        progress: Option<&ProgressCounter>,
        profiler: Option<&TransferProfiler>,
    ) -> io::Result<bool> {
        let byte_count =
            range.end.checked_sub(range.start).ok_or_else(
                || {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "positional send range ends before it starts",
                    )
                },
            )?;

        let offset = range.start;

        self.send_payload(
            writer,
            byte_count,
            raw_buffer,
            decision,
            progress,
            profiler,
            |chunk, transferred| {
                let read_offset = offset
                    .checked_add(transferred)
                    .ok_or_else(|| {
                        io::Error::other(
                            "compressed read offset overflowed",
                        )
                    })?;

                read_exact_at(
                    file,
                    chunk,
                    read_offset,
                )
            },
        )
    }

    fn send_payload<F>(
        &mut self,
        writer: &mut impl Write,
        byte_count: u64,
        raw_buffer: &mut [u8],
        decision: CompressionDecision,
        progress: Option<&ProgressCounter>,
        profiler: Option<&TransferProfiler>,
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
            if let Some(progress) = progress {
                progress.check_cancelled()?;
            }

            let remaining = byte_count - transferred;

            let requested =
                remaining.min(COMPRESSION_CHUNK_BYTES.min(raw_buffer.len()) as u64) as usize;

            let raw_chunk = &mut raw_buffer[..requested];

            let read_started = Instant::now();

            fill_chunk(raw_chunk, transferred)?;

            if let Some(profiler) = profiler {
                profiler.record_sender_source_read(
                    read_started.elapsed(),
                    u64::try_from(requested)
                        .unwrap_or(u64::MAX),
                );
            }

            hasher.update(raw_chunk);

            match decision {
                CompressionDecision::SendRaw => {
                    writer.write_all(raw_chunk)?;
                }

                CompressionDecision::Compress => {
                    let compression_started = Instant::now();

                    let compressed_length = self
                        .compressor
                        .compress_to_buffer(
                            raw_chunk,
                            &mut self.compressed_buffer,
                        )
                        .map_err(|error| {
                            io::Error::other(format!(
                                "Zstandard payload compression failed: {error}"
                            ))
                        })?;

                    if let Some(profiler) = profiler {
                        profiler.record_sender_compression(
                            compression_started.elapsed(),
                            u64::try_from(requested)
                                .unwrap_or(u64::MAX),
                        );
                    }

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

            let requested_bytes = u64::try_from(requested)
                .map_err(|_| io::Error::other("compression chunk length cannot be represented"))?;

            transferred = transferred
                .checked_add(requested_bytes)
                .ok_or_else(|| io::Error::other("compressed send byte count overflowed"))?;

            if let Some(progress) = progress {
                progress.add(requested_bytes);
            }
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

    pub(crate) fn receive_sequential_with_progress(
        &mut self,
        reader: &mut impl Read,
        writer: &mut impl Write,
        byte_count: u64,
        raw_buffer: &mut [u8],
        context: &str,
        progress: Option<&ProgressCounter>,
    ) -> io::Result<bool> {
        self.receive_sequential_with_progress_profiled(
            reader,
            writer,
            byte_count,
            raw_buffer,
            context,
            progress,
            None,
        )
    }

    pub(crate) fn receive_sequential_with_progress_profiled(
        &mut self,
        reader: &mut impl Read,
        writer: &mut impl Write,
        byte_count: u64,
        raw_buffer: &mut [u8],
        context: &str,
        progress: Option<&ProgressCounter>,
        profiler: Option<&TransferProfiler>,
    ) -> io::Result<bool> {
        self.receive_payload(
            reader,
            byte_count,
            raw_buffer,
            context,
            progress,
            profiler,
            |chunk, _| writer.write_all(chunk),
        )
    }

    pub(crate) fn receive_positional_with_progress(
        &mut self,
        reader: &mut impl Read,
        file: &File,
        range: Range<u64>,
        raw_buffer: &mut [u8],
        context: &str,
        progress: Option<&ProgressCounter>,
    ) -> io::Result<bool> {
        self.receive_positional_with_progress_profiled(
            reader,
            file,
            range,
            raw_buffer,
            context,
            progress,
            None,
        )
    }

    pub(crate) fn receive_positional_with_progress_profiled(
        &mut self,
        reader: &mut impl Read,
        file: &File,
        range: Range<u64>,
        raw_buffer: &mut [u8],
        context: &str,
        progress: Option<&ProgressCounter>,
        profiler: Option<&TransferProfiler>,
    ) -> io::Result<bool> {
        let byte_count =
            range.end.checked_sub(range.start).ok_or_else(
                || {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "positional receive range ends before it starts",
                    )
                },
            )?;

        let offset = range.start;

        self.receive_payload(
            reader,
            byte_count,
            raw_buffer,
            context,
            progress,
            profiler,
            |chunk, transferred| {
                let write_offset = offset
                    .checked_add(transferred)
                    .ok_or_else(|| {
                        io::Error::other(
                            "decompressed write offset overflowed",
                        )
                    })?;

                striped_file::write_all_at(
                    file,
                    chunk,
                    write_offset,
                )
            },
        )
    }

    fn receive_payload<F>(
        &mut self,
        reader: &mut impl Read,
        byte_count: u64,
        raw_buffer: &mut [u8],
        context: &str,
        progress: Option<&ProgressCounter>,
        profiler: Option<&TransferProfiler>,
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
            if let Some(progress) = progress {
                progress.check_cancelled()?;
            }

            match decision {
                CompressionDecision::SendRaw => {
                    let requested =
                        (byte_count - transferred).min(raw_buffer.len() as u64) as usize;

                    reader.read_exact(&mut raw_buffer[..requested])?;

                    let chunk = &raw_buffer[..requested];

                    hasher.update(chunk);

                    let write_started = Instant::now();

                    write_chunk(chunk, transferred)?;

                    if let Some(profiler) = profiler {
                        profiler.record_receiver_destination_write(
                            write_started.elapsed(),
                            u64::try_from(requested)
                                .unwrap_or(u64::MAX),
                        );
                    }

                    let requested_bytes = u64::try_from(requested).map_err(|_| {
                        io::Error::other("raw payload length cannot be represented")
                    })?;

                    transferred = transferred
                        .checked_add(requested_bytes)
                        .ok_or_else(|| io::Error::other("raw receive byte count overflowed"))?;

                    if let Some(progress) = progress {
                        progress.add(requested_bytes);
                    }
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

                    let decompression_started = Instant::now();

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

                    if let Some(profiler) = profiler {
                        profiler.record_receiver_decompression(
                            decompression_started.elapsed(),
                            u64::try_from(raw_length)
                                .unwrap_or(u64::MAX),
                        );
                    }

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

                    let write_started = Instant::now();

                    write_chunk(chunk, transferred)?;

                    if let Some(profiler) = profiler {
                        profiler.record_receiver_destination_write(
                            write_started.elapsed(),
                            u64::try_from(raw_length)
                                .unwrap_or(u64::MAX),
                        );
                    }

                    let raw_length_bytes = u64::try_from(raw_length).map_err(|_| {
                        io::Error::other("decompressed payload length cannot be represented")
                    })?;

                    transferred = transferred.checked_add(raw_length_bytes).ok_or_else(|| {
                        io::Error::other("decompressed receive byte count overflowed")
                    })?;

                    if let Some(progress) = progress {
                        progress.add(raw_length_bytes);
                    }
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
    use crate::transfer_profile::TransferProfiler;
    use std::io::Cursor;

    #[test]
    fn compressed_payload_round_trips() {
        let contents = vec![0x5A_u8; COMPRESSION_CHUNK_BYTES * 2 + 137];

        let mut encoder = PayloadEncoder::new(1).unwrap();

        let mut source = Cursor::new(&contents);

        let mut wire = Vec::new();

        let mut encode_buffer = vec![0_u8; COMPRESSION_CHUNK_BYTES];

        let compressed = encoder
            .send_sequential_with_progress(
                &mut wire,
                &mut source,
                contents.len() as u64,
                &mut encode_buffer,
                CompressionDecision::Compress,
                None,
            )
            .unwrap();

        assert!(compressed);
        assert!(wire.len() < contents.len());

        let mut decoder = PayloadDecoder::new().unwrap();

        let mut wire_reader = Cursor::new(wire);

        let mut destination = Vec::new();

        let mut decode_buffer = vec![0_u8; COMPRESSION_CHUNK_BYTES];

        let received_compressed = decoder
            .receive_sequential_with_progress(
                &mut wire_reader,
                &mut destination,
                contents.len() as u64,
                &mut decode_buffer,
                "compressed test payload",
                None,
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
            .send_sequential_with_progress(
                &mut wire,
                &mut source,
                contents.len() as u64,
                &mut encode_buffer,
                CompressionDecision::SendRaw,
                None,
            )
            .unwrap();

        assert!(!compressed);

        let mut decoder = PayloadDecoder::new().unwrap();

        let mut wire_reader = Cursor::new(wire);

        let mut destination = Vec::new();
        let mut decode_buffer = vec![0_u8; 64 * 1024];

        let received_compressed = decoder
            .receive_sequential_with_progress(
                &mut wire_reader,
                &mut destination,
                contents.len() as u64,
                &mut decode_buffer,
                "raw test payload",
                None,
            )
            .unwrap();

        assert!(!received_compressed);
        assert_eq!(destination, contents);
    }

    #[test]
    fn profiled_payload_records_stage_work() {
        let contents =
            vec![
                0x5A_u8;
                COMPRESSION_CHUNK_BYTES * 2 + 137
            ];

        let profiler =
            TransferProfiler::default();

        let mut encoder =
            PayloadEncoder::new(1).unwrap();

        let mut source =
            Cursor::new(&contents);

        let mut wire = Vec::new();

        let mut encode_buffer =
            vec![
                0_u8;
                COMPRESSION_CHUNK_BYTES
            ];

        encoder
            .send_sequential_with_progress_profiled(
                &mut wire,
                &mut source,
                contents.len() as u64,
                &mut encode_buffer,
                CompressionDecision::Compress,
                None,
                Some(&profiler),
            )
            .unwrap();

        let sender =
            profiler.snapshot();

        assert_eq!(
            sender.sender_source_read.bytes,
            contents.len() as u64,
        );

        assert_eq!(
            sender.sender_compression.bytes,
            contents.len() as u64,
        );

        assert!(
            sender.sender_source_read.operations
                > 0,
        );

        assert!(
            sender.sender_compression.operations
                > 0,
        );

        let mut decoder =
            PayloadDecoder::new().unwrap();

        let mut wire_reader =
            Cursor::new(wire);

        let mut destination = Vec::new();

        let mut decode_buffer =
            vec![
                0_u8;
                COMPRESSION_CHUNK_BYTES
            ];

        decoder
            .receive_sequential_with_progress_profiled(
                &mut wire_reader,
                &mut destination,
                contents.len() as u64,
                &mut decode_buffer,
                "profiled payload",
                None,
                Some(&profiler),
            )
            .unwrap();

        assert_eq!(destination, contents);

        let profile =
            profiler.snapshot();

        assert_eq!(
            profile.receiver_decompression.bytes,
            contents.len() as u64,
        );

        assert_eq!(
            profile.receiver_destination_write.bytes,
            contents.len() as u64,
        );

        assert!(
            profile.receiver_decompression.operations
                > 0,
        );

        assert!(
            profile
                .receiver_destination_write
                .operations
                > 0,
        );
    }
}
