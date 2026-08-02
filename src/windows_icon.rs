use std::env;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::ptr::null_mut;
use windows_sys::Win32::UI::Shell::ExtractIconExW;
use windows_sys::Win32::UI::WindowsAndMessaging::{DestroyIcon, HICON};

pub(crate) struct OwnedIcon {
    handle: HICON,
}

impl OwnedIcon {
    pub(crate) const fn raw(&self) -> HICON {
        self.handle
    }
}

impl Drop for OwnedIcon {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe {
                let _ = DestroyIcon(self.handle);
            }
        }
    }
}

pub(crate) fn load_executable_icon() -> io::Result<OwnedIcon> {
    let executable = env::current_exe()?;

    let executable = executable
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();

    let mut small_icon: HICON = null_mut();

    let extracted =
        unsafe { ExtractIconExW(executable.as_ptr(), 0, null_mut(), &mut small_icon, 1) };

    if extracted == 0 || small_icon.is_null() {
        return Err(io::Error::other(
            "the embedded NetworkCopy executable icon could not be extracted",
        ));
    }

    Ok(OwnedIcon { handle: small_icon })
}

pub(crate) fn load_executable_large_icon() -> io::Result<OwnedIcon> {
    let executable = env::current_exe()?;

    let executable = executable
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();

    let mut large_icon: HICON = null_mut();

    let extracted =
        unsafe { ExtractIconExW(executable.as_ptr(), 0, &mut large_icon, null_mut(), 1) };

    if extracted == 0 || large_icon.is_null() {
        return Err(io::Error::other(
            "the embedded large NetworkCopy executable icon could not be extracted",
        ));
    }

    Ok(OwnedIcon { handle: large_icon })
}
