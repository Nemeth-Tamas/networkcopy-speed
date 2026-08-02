use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

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
            let source_name = source_directory_name(source_root)?;

            Ok(destination.join(source_name))
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
        DestinationLayout, resolve_destination, resolve_destination_text, source_directory_name,
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
}
