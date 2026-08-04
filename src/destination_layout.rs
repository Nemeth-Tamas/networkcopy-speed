use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

const MAX_WINDOWS_COMPONENT_UTF16_UNITS: usize = 255;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct SourceDirectoryName(String);

impl SourceDirectoryName {
    pub fn from_source_root(source_root: &Path) -> io::Result<Self> {
        let name = source_directory_name(source_root)?;

        let name = name.into_string().map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "source directory name is not valid Unicode",
            )
        })?;

        Self::parse(name)
    }

    pub fn parse(value: impl Into<String>) -> io::Result<Self> {
        let value = value.into();

        validate_source_directory_name(&value)?;

        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

fn validate_source_directory_name(value: &str) -> io::Result<()> {
    if value.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "source directory name must not be empty",
        ));
    }

    if matches!(value, "." | "..") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "source directory name must not be dot or dot-dot",
        ));
    }

    if value.ends_with(' ') || value.ends_with('.') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "source directory name must not end with a space or dot",
        ));
    }

    if value.encode_utf16().count() > MAX_WINDOWS_COMPONENT_UTF16_UNITS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "source directory name exceeds {MAX_WINDOWS_COMPONENT_UTF16_UNITS} UTF-16 units",
            ),
        ));
    }

    if value.chars().any(|character| {
        character <= '\u{1f}'
            || matches!(
                character,
                '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'
            )
    }) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "source directory name contains a character that is invalid in a Windows filename",
        ));
    }

    let device_stem = value
        .split('.')
        .next()
        .unwrap_or(value)
        .to_ascii_uppercase();

    let numbered_device = device_stem.len() == 4
        && (device_stem.starts_with("COM") || device_stem.starts_with("LPT"))
        && matches!(device_stem.as_bytes()[3], b'1'..=b'9');

    if matches!(device_stem.as_str(), "CON" | "PRN" | "AUX" | "NUL") || numbered_device {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("source directory name {value:?} is reserved by Windows",),
        ));
    }

    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DestinationLayout {
    Exact,

    SourceNameUnderRoot,
}

impl DestinationLayout {
    pub const fn code(self) -> u8 {
        match self {
            Self::Exact => 0,

            Self::SourceNameUnderRoot => 1,
        }
    }

    pub fn from_code(code: u8) -> io::Result<Self> {
        match code {
            0 => Ok(Self::Exact),

            1 => Ok(Self::SourceNameUnderRoot),

            unknown => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown destination layout code {unknown}",),
            )),
        }
    }
}

pub fn source_directory_name(source_root: &Path) -> io::Result<OsString> {
    source_root
        .file_name()
        .map(ToOwned::to_owned)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "source root {} has no directory name",
                    source_root.display(),
                ),
            )
        })
}

pub fn resolve_destination(
    layout: DestinationLayout,
    source_root: &Path,
    destination: &Path,
) -> io::Result<PathBuf> {
    if destination.as_os_str().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "destination path must not be empty",
        ));
    }

    match layout {
        DestinationLayout::Exact => Ok(destination.to_path_buf()),

        DestinationLayout::SourceNameUnderRoot => {
            let source_name = SourceDirectoryName::from_source_root(source_root)?;

            Ok(destination.join(source_name.as_str()))
        }
    }
}

pub fn resolve_destination_text(
    layout: DestinationLayout,
    source_root: &str,
    destination: &str,
) -> io::Result<String> {
    let resolved = resolve_destination(layout, Path::new(source_root), Path::new(destination))?;

    resolved.into_os_string().into_string().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "resolved destination path is not valid Unicode",
        )
    })
}

#[cfg(test)]
mod tests {
    use super::{
        DestinationLayout, SourceDirectoryName, resolve_destination, resolve_destination_text,
        source_directory_name,
    };
    use std::path::Path;

    #[test]
    fn exact_layout_preserves_destination() {
        let actual = resolve_destination(
            DestinationLayout::Exact,
            Path::new(r"C:\Users\User\Desktop"),
            Path::new(r"D:\Destination"),
        )
        .unwrap();

        assert_eq!(actual, Path::new(r"D:\Destination"),);
    }

    #[test]
    fn root_layout_appends_source_name() {
        let actual = resolve_destination(
            DestinationLayout::SourceNameUnderRoot,
            Path::new(r"C:\Users\User\Desktop"),
            Path::new(r"D:\Destination"),
        )
        .unwrap();

        assert_eq!(actual, Path::new(r"D:\Destination\Desktop",),);
    }

    #[test]
    fn trailing_separator_keeps_source_name() {
        let actual = resolve_destination_text(
            DestinationLayout::SourceNameUnderRoot,
            "C:\\Users\\User\\Documents\\",
            "E:\\Backups\\User",
        )
        .unwrap();

        assert_eq!(actual, r"E:\Backups\User\Documents",);
    }

    #[test]
    fn drive_root_cannot_supply_child_name() {
        assert!(source_directory_name(Path::new(r"C:\"),).is_err(),);
    }

    #[test]
    fn destination_layout_codes_round_trip() {
        for layout in [
            DestinationLayout::Exact,
            DestinationLayout::SourceNameUnderRoot,
        ] {
            assert_eq!(
                DestinationLayout::from_code(layout.code(),).unwrap(),
                layout,
            );
        }

        assert!(DestinationLayout::from_code(99).is_err(),);
    }

    #[test]
    fn empty_destination_is_rejected() {
        assert!(
            resolve_destination(
                DestinationLayout::Exact,
                Path::new(r"C:\Users\User\Desktop",),
                Path::new(""),
            )
            .is_err(),
        );
    }

    #[test]
    fn source_directory_metadata_accepts_safe_unicode_name() {
        let name = SourceDirectoryName::parse("Fényképek").unwrap();

        assert_eq!(name.as_str(), "Fényképek",);

        assert_eq!(name.into_string(), "Fényképek",);
    }

    #[test]
    fn source_directory_metadata_extracts_leaf_name() {
        let name =
            SourceDirectoryName::from_source_root(Path::new(r"C:\Users\User\Desktop")).unwrap();

        assert_eq!(name.as_str(), "Desktop",);
    }

    #[test]
    fn source_directory_metadata_rejects_unsafe_components() {
        for invalid in [
            "",
            ".",
            "..",
            r"Desktop\Nested",
            "Desktop/Nested",
            "Desk:top",
            "CON",
            "con.txt",
            "COM1",
            "LPT9",
            "folder.",
            "folder ",
        ] {
            assert!(
                SourceDirectoryName::parse(invalid,).is_err(),
                "{invalid:?} should be rejected",
            );
        }
    }

    #[test]
    fn source_directory_metadata_enforces_windows_component_limit() {
        let maximum = "a".repeat(255);

        assert!(SourceDirectoryName::parse(maximum,).is_ok(),);

        let too_long = "a".repeat(256);

        assert!(SourceDirectoryName::parse(too_long,).is_err(),);
    }

    #[test]
    fn root_layout_rejects_unsafe_source_leaf() {
        assert!(
            resolve_destination(
                DestinationLayout::SourceNameUnderRoot,
                Path::new(r"C:\Users\User\CON",),
                Path::new(r"D:\Backup",),
            )
            .is_err(),
        );
    }
}
