use crate::destination_layout::validate_windows_path_component;
use std::collections::HashSet;
use std::io;
use std::str;

pub const DESKTOP_LAYOUT_FORMAT_VERSION: u16 = 1;

pub const MAX_DESKTOP_LAYOUT_BYTES: usize = 1024 * 1024;

pub const MAX_DESKTOP_LAYOUT_ITEMS: usize = 4096;

pub const MAX_DESKTOP_LAYOUT_MONITORS: usize = 32;

const DESKTOP_LAYOUT_MAGIC: &[u8; 4] = b"NCDL";

const MAX_DESKTOP_COORDINATE_ABS: i32 = 1_000_000;

const MIN_DESKTOP_DPI: u32 = 48;

const MAX_DESKTOP_DPI: u32 = 1920;

const MIN_DESKTOP_ICON_SIZE: u32 = 8;

const MAX_DESKTOP_ICON_SIZE: u32 = 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DesktopPoint {
    pub x: i32,

    pub y: i32,
}

impl DesktopPoint {
    fn validate(self, description: &str) -> io::Result<()> {
        validate_coordinate(self.x, description)?;

        validate_coordinate(self.y, description)?;

        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DesktopRect {
    pub left: i32,

    pub top: i32,

    pub right: i32,

    pub bottom: i32,
}

impl DesktopRect {
    fn validate(self, description: &str) -> io::Result<()> {
        validate_coordinate(self.left, description)?;

        validate_coordinate(self.top, description)?;

        validate_coordinate(self.right, description)?;

        validate_coordinate(self.bottom, description)?;

        if self.left >= self.right {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{description} has a non-positive width"),
            ));
        }

        if self.top >= self.bottom {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{description} has a non-positive height"),
            ));
        }

        Ok(())
    }

    fn contains(self, other: Self) -> bool {
        other.left >= self.left
            && other.top >= self.top
            && other.right <= self.right
            && other.bottom <= self.bottom
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DesktopMonitor {
    pub bounds: DesktopRect,

    pub work_area: DesktopRect,

    pub dpi_x: u32,

    pub dpi_y: u32,

    pub primary: bool,
}

impl DesktopMonitor {
    fn validate(self) -> io::Result<()> {
        self.bounds.validate("desktop monitor bounds")?;

        self.work_area.validate("desktop monitor work area")?;

        if !self.bounds.contains(self.work_area) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "desktop monitor work area must remain inside the monitor bounds",
            ));
        }

        validate_dpi(self.dpi_x, "desktop monitor horizontal DPI")?;

        validate_dpi(self.dpi_y, "desktop monitor vertical DPI")?;

        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DesktopItemKind {
    File,

    Directory,
}

impl DesktopItemKind {
    const fn code(self) -> u8 {
        match self {
            Self::File => 0,

            Self::Directory => 1,
        }
    }

    fn from_code(code: u8) -> io::Result<Self> {
        match code {
            0 => Ok(Self::File),

            1 => Ok(Self::Directory),

            unknown => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("desktop layout item used unknown kind {unknown}"),
            )),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DesktopLayoutItem {
    pub name: String,

    pub kind: DesktopItemKind,

    pub position: DesktopPoint,
}

impl DesktopLayoutItem {
    fn validate(&self) -> io::Result<()> {
        validate_windows_path_component(&self.name, "desktop item name")?;

        self.position.validate("desktop item position")
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DesktopLayoutSnapshot {
    pub version: u16,

    pub icon_size: u32,

    pub auto_arrange: bool,

    pub monitors: Vec<DesktopMonitor>,

    pub items: Vec<DesktopLayoutItem>,
}

impl DesktopLayoutSnapshot {
    pub fn validate(&self) -> io::Result<()> {
        if self.version != DESKTOP_LAYOUT_FORMAT_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "desktop layout uses unsupported format version {}",
                    self.version,
                ),
            ));
        }

        if !(MIN_DESKTOP_ICON_SIZE..=MAX_DESKTOP_ICON_SIZE).contains(&self.icon_size) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "desktop icon size {} is outside the supported range {MIN_DESKTOP_ICON_SIZE}..={MAX_DESKTOP_ICON_SIZE}",
                    self.icon_size,
                ),
            ));
        }

        if self.monitors.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "desktop layout must contain at least one monitor",
            ));
        }

        if self.monitors.len() > MAX_DESKTOP_LAYOUT_MONITORS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "desktop layout contains {} monitors, exceeding the {MAX_DESKTOP_LAYOUT_MONITORS} monitor limit",
                    self.monitors.len(),
                ),
            ));
        }

        let primary_monitor_count = self
            .monitors
            .iter()
            .filter(|monitor| monitor.primary)
            .count();

        if primary_monitor_count != 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "desktop layout must contain exactly one primary monitor, found {primary_monitor_count}",
                ),
            ));
        }

        for monitor in &self.monitors {
            monitor.validate()?;
        }

        if self.items.len() > MAX_DESKTOP_LAYOUT_ITEMS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "desktop layout contains {} items, exceeding the {MAX_DESKTOP_LAYOUT_ITEMS} item limit",
                    self.items.len(),
                ),
            ));
        }

        let mut names = HashSet::with_capacity(self.items.len());

        for item in &self.items {
            item.validate()?;

            if !names.insert(item.name.as_str()) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "desktop layout contains duplicate item name {:?}",
                        item.name,
                    ),
                ));
            }
        }

        Ok(())
    }
}

pub fn encode_desktop_layout(snapshot: &DesktopLayoutSnapshot) -> io::Result<Vec<u8>> {
    snapshot.validate()?;

    let monitor_count = u16::try_from(snapshot.monitors.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "desktop monitor count does not fit the layout format",
        )
    })?;

    let item_count = u32::try_from(snapshot.items.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "desktop item count does not fit the layout format",
        )
    })?;

    let mut output = Vec::with_capacity(4096);

    append_bytes(&mut output, DESKTOP_LAYOUT_MAGIC)?;

    append_u16(&mut output, snapshot.version)?;

    append_u32(&mut output, snapshot.icon_size)?;

    append_u8(&mut output, u8::from(snapshot.auto_arrange))?;

    append_u16(&mut output, monitor_count)?;

    append_u32(&mut output, item_count)?;

    for monitor in &snapshot.monitors {
        append_rect(&mut output, monitor.bounds)?;

        append_rect(&mut output, monitor.work_area)?;

        append_u32(&mut output, monitor.dpi_x)?;

        append_u32(&mut output, monitor.dpi_y)?;

        append_u8(&mut output, u8::from(monitor.primary))?;
    }

    for item in &snapshot.items {
        append_u8(&mut output, item.kind.code())?;

        append_i32(&mut output, item.position.x)?;

        append_i32(&mut output, item.position.y)?;

        let name = item.name.as_bytes();

        let name_length = u16::try_from(name.len()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "desktop item name {:?} is too large for the layout format",
                    item.name,
                ),
            )
        })?;

        append_u16(&mut output, name_length)?;

        append_bytes(&mut output, name)?;
    }

    Ok(output)
}

pub fn decode_desktop_layout(bytes: &[u8]) -> io::Result<DesktopLayoutSnapshot> {
    if bytes.len() > MAX_DESKTOP_LAYOUT_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "desktop layout contains {} bytes, exceeding the {MAX_DESKTOP_LAYOUT_BYTES} byte limit",
                bytes.len(),
            ),
        ));
    }

    let mut decoder = Decoder::new(bytes);

    let magic = decoder.read_bytes(DESKTOP_LAYOUT_MAGIC.len())?;

    if magic != DESKTOP_LAYOUT_MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "desktop layout has an invalid magic header",
        ));
    }

    let version = decoder.read_u16()?;

    if version != DESKTOP_LAYOUT_FORMAT_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("desktop layout uses unsupported format version {version}",),
        ));
    }

    let icon_size = decoder.read_u32()?;

    let auto_arrange = decode_bool(decoder.read_u8()?, "desktop Auto Arrange")?;

    let monitor_count = usize::from(decoder.read_u16()?);

    if monitor_count == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "desktop layout contains no monitors",
        ));
    }

    if monitor_count > MAX_DESKTOP_LAYOUT_MONITORS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "desktop layout contains {monitor_count} monitors, exceeding the {MAX_DESKTOP_LAYOUT_MONITORS} monitor limit",
            ),
        ));
    }

    let item_count = usize::try_from(decoder.read_u32()?).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "desktop layout item count does not fit this platform",
        )
    })?;

    if item_count > MAX_DESKTOP_LAYOUT_ITEMS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "desktop layout contains {item_count} items, exceeding the {MAX_DESKTOP_LAYOUT_ITEMS} item limit",
            ),
        ));
    }

    let mut monitors = Vec::with_capacity(monitor_count);

    for _ in 0..monitor_count {
        monitors.push(DesktopMonitor {
            bounds: decoder.read_rect()?,

            work_area: decoder.read_rect()?,

            dpi_x: decoder.read_u32()?,

            dpi_y: decoder.read_u32()?,

            primary: decode_bool(decoder.read_u8()?, "desktop monitor primary flag")?,
        });
    }

    let mut items = Vec::with_capacity(item_count);

    for _ in 0..item_count {
        let kind = DesktopItemKind::from_code(decoder.read_u8()?)?;

        let position = DesktopPoint {
            x: decoder.read_i32()?,

            y: decoder.read_i32()?,
        };

        let name_length = usize::from(decoder.read_u16()?);

        let name_bytes = decoder.read_bytes(name_length)?;

        let name = str::from_utf8(name_bytes)
            .map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("desktop item name is not valid UTF-8: {error}",),
                )
            })?
            .to_string();

        items.push(DesktopLayoutItem {
            name,

            kind,

            position,
        });
    }

    if !decoder.is_finished() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "desktop layout contains {} trailing bytes",
                decoder.remaining(),
            ),
        ));
    }

    let snapshot = DesktopLayoutSnapshot {
        version,

        icon_size,

        auto_arrange,

        monitors,

        items,
    };

    snapshot
        .validate()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;

    Ok(snapshot)
}

fn validate_coordinate(value: i32, description: &str) -> io::Result<()> {
    if !(-MAX_DESKTOP_COORDINATE_ABS..=MAX_DESKTOP_COORDINATE_ABS).contains(&value) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "{description} coordinate {value} exceeds the supported +/-{MAX_DESKTOP_COORDINATE_ABS} range",
            ),
        ));
    }

    Ok(())
}

fn validate_dpi(value: u32, description: &str) -> io::Result<()> {
    if !(MIN_DESKTOP_DPI..=MAX_DESKTOP_DPI).contains(&value) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "{description} value {value} is outside the supported range {MIN_DESKTOP_DPI}..={MAX_DESKTOP_DPI}",
            ),
        ));
    }

    Ok(())
}

fn append_rect(output: &mut Vec<u8>, value: DesktopRect) -> io::Result<()> {
    append_i32(output, value.left)?;

    append_i32(output, value.top)?;

    append_i32(output, value.right)?;

    append_i32(output, value.bottom)
}

fn append_u8(output: &mut Vec<u8>, value: u8) -> io::Result<()> {
    append_bytes(output, &[value])
}

fn append_u16(output: &mut Vec<u8>, value: u16) -> io::Result<()> {
    append_bytes(output, &value.to_le_bytes())
}

fn append_u32(output: &mut Vec<u8>, value: u32) -> io::Result<()> {
    append_bytes(output, &value.to_le_bytes())
}

fn append_i32(output: &mut Vec<u8>, value: i32) -> io::Result<()> {
    append_bytes(output, &value.to_le_bytes())
}

fn append_bytes(output: &mut Vec<u8>, bytes: &[u8]) -> io::Result<()> {
    let Some(new_length) = output.len().checked_add(bytes.len()) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "desktop layout encoded size overflowed",
        ));
    };

    if new_length > MAX_DESKTOP_LAYOUT_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("desktop layout exceeds the {MAX_DESKTOP_LAYOUT_BYTES} byte limit",),
        ));
    }

    output.extend_from_slice(bytes);

    Ok(())
}

fn decode_bool(value: u8, description: &str) -> io::Result<bool> {
    match value {
        0 => Ok(false),

        1 => Ok(true),

        unknown => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{description} used invalid boolean value {unknown}"),
        )),
    }
}

struct Decoder<'a> {
    bytes: &'a [u8],

    offset: usize,
}

impl<'a> Decoder<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn read_bytes(&mut self, length: usize) -> io::Result<&'a [u8]> {
        let Some(end) = self.offset.checked_add(length) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "desktop layout offset overflowed",
            ));
        };

        let bytes = self.bytes.get(self.offset..end).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "desktop layout ended unexpectedly",
            )
        })?;

        self.offset = end;

        Ok(bytes)
    }

    fn read_u8(&mut self) -> io::Result<u8> {
        Ok(self.read_bytes(1)?[0])
    }

    fn read_u16(&mut self) -> io::Result<u16> {
        let bytes = self.read_bytes(2)?;

        Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    fn read_u32(&mut self) -> io::Result<u32> {
        let bytes = self.read_bytes(4)?;

        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn read_i32(&mut self) -> io::Result<i32> {
        let bytes = self.read_bytes(4)?;

        Ok(i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn read_rect(&mut self) -> io::Result<DesktopRect> {
        Ok(DesktopRect {
            left: self.read_i32()?,

            top: self.read_i32()?,

            right: self.read_i32()?,

            bottom: self.read_i32()?,
        })
    }

    fn remaining(&self) -> usize {
        self.bytes.len() - self.offset
    }

    fn is_finished(&self) -> bool {
        self.offset == self.bytes.len()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DESKTOP_LAYOUT_FORMAT_VERSION, DesktopItemKind, DesktopLayoutItem, DesktopLayoutSnapshot,
        DesktopMonitor, DesktopPoint, DesktopRect, MAX_DESKTOP_LAYOUT_BYTES,
        MAX_DESKTOP_LAYOUT_ITEMS, decode_desktop_layout, encode_desktop_layout,
    };

    fn example_snapshot() -> DesktopLayoutSnapshot {
        DesktopLayoutSnapshot {
            version: DESKTOP_LAYOUT_FORMAT_VERSION,

            icon_size: 48,

            auto_arrange: false,

            monitors: vec![
                DesktopMonitor {
                    bounds: DesktopRect {
                        left: 0,

                        top: 0,

                        right: 1920,

                        bottom: 1080,
                    },

                    work_area: DesktopRect {
                        left: 0,

                        top: 0,

                        right: 1920,

                        bottom: 1040,
                    },

                    dpi_x: 96,

                    dpi_y: 96,

                    primary: true,
                },
                DesktopMonitor {
                    bounds: DesktopRect {
                        left: -1280,

                        top: 0,

                        right: 0,

                        bottom: 1024,
                    },

                    work_area: DesktopRect {
                        left: -1280,

                        top: 0,

                        right: 0,

                        bottom: 984,
                    },

                    dpi_x: 120,

                    dpi_y: 120,

                    primary: false,
                },
            ],

            items: vec![
                DesktopLayoutItem {
                    name: "Notes.txt".to_string(),

                    kind: DesktopItemKind::File,

                    position: DesktopPoint { x: 172, y: 94 },
                },
                DesktopLayoutItem {
                    name: "Projects".to_string(),

                    kind: DesktopItemKind::Directory,

                    position: DesktopPoint { x: -1110, y: 205 },
                },
                DesktopLayoutItem {
                    name: "Fényképek".to_string(),

                    kind: DesktopItemKind::Directory,

                    position: DesktopPoint { x: 386, y: 94 },
                },
            ],
        }
    }

    #[test]
    fn desktop_layout_metadata_round_trips() {
        let expected = example_snapshot();

        let encoded = encode_desktop_layout(&expected).unwrap();

        let decoded = decode_desktop_layout(&encoded).unwrap();

        assert_eq!(decoded, expected);
    }

    #[test]
    fn desktop_layout_accepts_negative_virtual_screen_coordinates() {
        let snapshot = example_snapshot();

        assert!(snapshot.validate().is_ok());

        assert_eq!(snapshot.monitors[1].bounds.left, -1280);
    }

    #[test]
    fn desktop_layout_rejects_unsafe_item_names() {
        for invalid in [
            "",
            ".",
            "..",
            r"Nested\File.txt",
            "Nested/File.txt",
            "bad:name.txt",
            "CON",
            "con.txt",
            "folder.",
            "folder ",
        ] {
            let mut snapshot = example_snapshot();

            snapshot.items[0].name = invalid.to_string();

            assert!(
                snapshot.validate().is_err(),
                "{invalid:?} should be rejected",
            );
        }
    }

    #[test]
    fn desktop_layout_rejects_duplicate_item_names() {
        let mut snapshot = example_snapshot();

        snapshot.items.push(snapshot.items[0].clone());

        assert!(snapshot.validate().is_err());
    }

    #[test]
    fn desktop_layout_requires_exactly_one_primary_monitor() {
        let mut snapshot = example_snapshot();

        snapshot.monitors[0].primary = false;

        assert!(snapshot.validate().is_err());

        snapshot.monitors[0].primary = true;

        snapshot.monitors[1].primary = true;

        assert!(snapshot.validate().is_err());
    }

    #[test]
    fn desktop_layout_rejects_work_area_outside_monitor() {
        let mut snapshot = example_snapshot();

        snapshot.monitors[0].work_area.right = 2000;

        assert!(snapshot.validate().is_err());
    }

    #[test]
    fn desktop_layout_rejects_invalid_icon_size() {
        let mut snapshot = example_snapshot();

        snapshot.icon_size = 0;

        assert!(snapshot.validate().is_err());

        snapshot.icon_size = 2048;

        assert!(snapshot.validate().is_err());
    }

    #[test]
    fn desktop_layout_decoder_rejects_trailing_data() {
        let mut encoded = encode_desktop_layout(&example_snapshot()).unwrap();

        encoded.push(0xAA);

        assert!(decode_desktop_layout(&encoded).is_err());
    }

    #[test]
    fn desktop_layout_decoder_rejects_unknown_version() {
        let mut encoded = encode_desktop_layout(&example_snapshot()).unwrap();

        encoded[4..6].copy_from_slice(&2_u16.to_le_bytes());

        assert!(decode_desktop_layout(&encoded).is_err());
    }

    #[test]
    fn desktop_layout_codec_enforces_total_byte_limit() {
        let mut snapshot = example_snapshot();

        snapshot.items.clear();

        for index in 0..MAX_DESKTOP_LAYOUT_ITEMS {
            snapshot.items.push(DesktopLayoutItem {
                name: format!("{index:04}-{}", "x".repeat(245),),

                kind: DesktopItemKind::File,

                position: DesktopPoint {
                    x: i32::try_from(index).unwrap(),

                    y: 0,
                },
            });
        }

        assert!(snapshot.validate().is_ok());

        let error = encode_desktop_layout(&snapshot).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput,);

        assert!(
            error
                .to_string()
                .contains(&MAX_DESKTOP_LAYOUT_BYTES.to_string(),),
        );
    }

    #[test]
    fn desktop_layout_decoder_rejects_oversized_input() {
        let oversized = vec![0_u8; MAX_DESKTOP_LAYOUT_BYTES + 1];

        let error = decode_desktop_layout(&oversized).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData,);
    }
}
