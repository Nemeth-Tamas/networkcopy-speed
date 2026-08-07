$ErrorActionPreference = "Stop"

$Path = Join-Path $PSScriptRoot "src\multistream_copy.rs"

if (-not (Test-Path -LiteralPath $Path)) {
    throw "Run this file from the repository root. Missing: $Path"
}

$text = [System.IO.File]::ReadAllText($Path)

function Replace-ExactlyOnce {
    param(
        [string]$Label,
        [string]$Old,
        [string]$New
    )

    $first = $text.IndexOf($Old, [System.StringComparison]::Ordinal)

    if ($first -lt 0) {
        throw "Could not find expected block for: $Label"
    }

    $second = $text.IndexOf(
        $Old,
        $first + $Old.Length,
        [System.StringComparison]::Ordinal
    )

    if ($second -ge 0) {
        throw "Expected exactly one block for '$Label', but found more than one."
    }

    $script:text = $text.Substring(0, $first) + $New + $text.Substring($first + $Old.Length)
}

Replace-ExactlyOnce `
    "sender TCP writer profiling" `
    @'
    let buffered_reader = BufReader::with_capacity(NETWORK_BUFFER_BYTES, reader_stream);

    let mut reader = CountingReader::new(buffered_reader);

    let buffered_writer = BufWriter::with_capacity(NETWORK_BUFFER_BYTES, writer_stream);

    let mut writer = CountingWriter::new(buffered_writer);

    let mut buffer = vec![0_u8; COPY_BUFFER_BYTES];

    let mut encoder = PayloadEncoder::new(compression_probe::DEFAULT_LEVEL)?;
'@ `
    @'
    let buffered_reader = BufReader::with_capacity(NETWORK_BUFFER_BYTES, reader_stream);

    let mut reader = CountingReader::new(buffered_reader);

    let profiled_writer = profiler.sender_socket_writer(writer_stream);

    let buffered_writer = BufWriter::with_capacity(NETWORK_BUFFER_BYTES, profiled_writer);

    let mut writer = CountingWriter::new(buffered_writer);

    let mut buffer = vec![0_u8; COPY_BUFFER_BYTES];

    let mut encoder = PayloadEncoder::new(compression_probe::DEFAULT_LEVEL)?;
'@

Replace-ExactlyOnce `
    "receiver TCP reader profiling" `
    @'
    let buffered_reader = BufReader::with_capacity(NETWORK_BUFFER_BYTES, reader_stream);

    let mut reader = CountingReader::new(buffered_reader);

    let buffered_writer = BufWriter::with_capacity(NETWORK_BUFFER_BYTES, writer_stream);

    let mut writer = CountingWriter::new(buffered_writer);

    let mut buffer = vec![0_u8; COPY_BUFFER_BYTES];

    let mut decoder = PayloadDecoder::new()?;
'@ `
    @'
    let profiled_reader = profiler.receiver_socket_reader(reader_stream);

    let buffered_reader = BufReader::with_capacity(NETWORK_BUFFER_BYTES, profiled_reader);

    let mut reader = CountingReader::new(buffered_reader);

    let buffered_writer = BufWriter::with_capacity(NETWORK_BUFFER_BYTES, writer_stream);

    let mut writer = CountingWriter::new(buffered_writer);

    let mut buffer = vec![0_u8; COPY_BUFFER_BYTES];

    let mut decoder = PayloadDecoder::new()?;
'@

Replace-ExactlyOnce `
    "tiny-pack sender compression timing" `
    @'
    let raw_payload = &buffer[..total_bytes];

    let encoded = tiny_pack_codec::encode(raw_payload, compression_probe::DEFAULT_LEVEL)?;

    let wire_payload = encoded.wire_payload(raw_payload);
'@ `
    @'
    let raw_payload = &buffer[..total_bytes];

    let compression_started = Instant::now();

    let encoded = tiny_pack_codec::encode(raw_payload, compression_probe::DEFAULT_LEVEL)?;

    profiler.record_sender_compression(
        compression_started.elapsed(),
        summary.bytes,
    );

    let wire_payload = encoded.wire_payload(raw_payload);
'@

Replace-ExactlyOnce `
    "remove tiny-pack socket double counting" `
    @'
    let mut wire_payload = vec![0_u8; wire_bytes];

    let socket_read_started = std::time::Instant::now();
    reader.read_exact(&mut wire_payload)?;
    profiler.record_receiver_socket_read(
        socket_read_started.elapsed(),
        wire_bytes as u64,
    );

    let total_bytes = usize::try_from(summary.bytes).map_err(|_| {
'@ `
    @'
    let mut wire_payload = vec![0_u8; wire_bytes];

    reader.read_exact(&mut wire_payload)?;

    let total_bytes = usize::try_from(summary.bytes).map_err(|_| {
'@

Replace-ExactlyOnce `
    "tiny-pack receiver decompression timing" `
    @'
    tiny_pack_codec::decode(encoding, &wire_payload, total_bytes, buffer)?;

    let expected_pack_digest = read_digest(reader)?;
'@ `
    @'
    let decompression_started = Instant::now();

    tiny_pack_codec::decode(encoding, &wire_payload, total_bytes, buffer)?;

    if matches!(encoding, TinyPackEncoding::Zstandard) {
        profiler.record_receiver_decompression(
            decompression_started.elapsed(),
            summary.bytes,
        );
    }

    let expected_pack_digest = read_digest(reader)?;
'@

Replace-ExactlyOnce `
    "striped destination sync timing" `
    @'
    file.sync_all()?;

    Ok(compressed)
}

fn validate_stripe(
'@ `
    @'
    let sync_started = Instant::now();

    file.sync_all()?;

    profiler.record_receiver_destination_write(
        sync_started.elapsed(),
        0,
    );

    Ok(compressed)
}

fn validate_stripe(
'@

Replace-ExactlyOnce `
    "whole-file destination flush timing" `
    @'
        file.flush()?;
        Ok(compressed)
    })();
'@ `
    @'
        let flush_started = Instant::now();

        file.flush()?;

        profiler.record_receiver_destination_write(
            flush_started.elapsed(),
            0,
        );

        Ok(compressed)
    })();
'@

[System.IO.File]::WriteAllText(
    $Path,
    $text,
    [System.Text.UTF8Encoding]::new($false)
)

Write-Host "Updated src\multistream_copy.rs"