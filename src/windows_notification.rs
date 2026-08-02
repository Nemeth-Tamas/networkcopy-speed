use crate::windows_icon;
use std::io;
use std::mem::size_of;
use std::ptr::{null, null_mut};
use std::thread;
use std::time::Duration;
use windows_sys::Win32::UI::Shell::{
    NIF_ICON, NIF_INFO, NIF_TIP, NIIF_ERROR, NIIF_LARGE_ICON, NIIF_USER, NIIF_WARNING, NIM_ADD,
    NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{CreateWindowExW, DestroyWindow, HWND_MESSAGE};

const NOTIFICATION_ICON_ID: u32 = 1;

const NOTIFICATION_LIFETIME: Duration = Duration::from_secs(10);

const STATIC_WINDOW_CLASS: [u16; 7] = [
    b'S' as u16,
    b'T' as u16,
    b'A' as u16,
    b'T' as u16,
    b'I' as u16,
    b'C' as u16,
    0,
];

const APPLICATION_TOOLTIP: &str = "NetworkCopy Manager";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NotificationKind {
    Information,

    Warning,

    Error,
}

impl NotificationKind {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Information => "information",
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }

    const fn info_flags(self) -> u32 {
        match self {
            Self::Information => NIIF_USER | NIIF_LARGE_ICON,

            Self::Warning => NIIF_WARNING,

            Self::Error => NIIF_ERROR,
        }
    }
}

pub fn show(kind: NotificationKind, title: &str, body: &str) -> io::Result<()> {
    validate_notification_text(title, body)?;

    let title = title.to_string();
    let body = body.to_string();

    thread::Builder::new()
        .name("networkcopy-windows-notification".to_string())
        .spawn(move || {
            if let Err(error) = show_validated(kind, &title, &body) {
                eprintln!("Windows notification failed: {error}",);
            }
        })
        .map(|_| ())
}

pub fn show_blocking(kind: NotificationKind, title: &str, body: &str) -> io::Result<()> {
    validate_notification_text(title, body)?;

    show_validated(kind, title, body)
}

fn show_validated(kind: NotificationKind, title: &str, body: &str) -> io::Result<()> {
    let window = unsafe {
        CreateWindowExW(
            0,
            STATIC_WINDOW_CLASS.as_ptr(),
            null(),
            0,
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            null_mut(),
            null_mut(),
            null(),
        )
    };

    if window.is_null() {
        return Err(io::Error::last_os_error());
    }

    let icon = match windows_icon::load_executable_icon() {
        Ok(icon) => icon,

        Err(error) => {
            unsafe {
                DestroyWindow(window);
            }

            return Err(error);
        }
    };

    let balloon_icon = match windows_icon::load_executable_large_icon() {
        Ok(icon) => icon,

        Err(error) => {
            unsafe {
                DestroyWindow(window);
            }

            return Err(error);
        }
    };

    let structure_size = u32::try_from(size_of::<NOTIFYICONDATAW>())
        .map_err(|_| io::Error::other("notification structure size cannot be represented"))?;

    let mut data = NOTIFYICONDATAW {
        cbSize: structure_size,

        hWnd: window,

        uID: NOTIFICATION_ICON_ID,

        uFlags: NIF_ICON | NIF_TIP,

        hIcon: icon.raw(),

        hBalloonIcon: balloon_icon.raw(),

        ..Default::default()
    };

    copy_wide_truncated(&mut data.szTip, APPLICATION_TOOLTIP);

    let mut icon_added = false;

    let result = (|| {
        let added = unsafe { Shell_NotifyIconW(NIM_ADD, &data) };

        if added == 0 {
            return Err(io::Error::other(
                "Shell_NotifyIconW failed to add the temporary notification icon",
            ));
        }

        icon_added = true;

        data.uFlags = NIF_INFO;

        data.dwInfoFlags = kind.info_flags();

        copy_wide_truncated(&mut data.szInfoTitle, title);

        copy_wide_truncated(&mut data.szInfo, body);

        let modified = unsafe { Shell_NotifyIconW(NIM_MODIFY, &data) };

        if modified == 0 {
            return Err(io::Error::other(
                "Shell_NotifyIconW failed to display the notification",
            ));
        }

        thread::sleep(NOTIFICATION_LIFETIME);

        Ok(())
    })();

    if icon_added {
        unsafe {
            Shell_NotifyIconW(NIM_DELETE, &data);
        }
    }

    unsafe {
        DestroyWindow(window);
    }

    result
}

fn validate_notification_text(title: &str, body: &str) -> io::Result<()> {
    if title.trim().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "notification title must not be empty",
        ));
    }

    if body.trim().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "notification body must not be empty",
        ));
    }

    if title.contains('\0') || body.contains('\0') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "notification text must not contain a null character",
        ));
    }

    Ok(())
}

fn copy_wide_truncated(destination: &mut [u16], text: &str) {
    destination.fill(0);

    if destination.is_empty() {
        return;
    }

    let mut written = 0_usize;

    for character in text.chars() {
        let mut encoded = [0_u16; 2];

        let units = character.encode_utf16(&mut encoded);

        if written.saturating_add(units.len()) >= destination.len() {
            break;
        }

        destination[written..written + units.len()].copy_from_slice(units);

        written += units.len();
    }
}

#[cfg(test)]
mod tests {
    use super::{copy_wide_truncated, validate_notification_text};

    #[test]
    fn wide_text_is_null_terminated() {
        let mut destination = [99_u16; 8];

        copy_wide_truncated(&mut destination, "Test");

        assert_eq!(&destination[..5], &[84, 101, 115, 116, 0],);
    }

    #[test]
    fn truncation_does_not_split_surrogate_pairs() {
        let mut destination = [0_u16; 4];

        copy_wide_truncated(&mut destination, "A😀B");

        let used = destination.iter().position(|unit| *unit == 0).unwrap();

        assert_eq!(String::from_utf16(&destination[..used],).unwrap(), "A😀",);
    }

    #[test]
    fn notification_text_is_validated() {
        assert!(validate_notification_text("Title", "Body",).is_ok(),);

        assert!(validate_notification_text("", "Body",).is_err(),);

        assert!(validate_notification_text("Title", "Bad\0body",).is_err(),);
    }
}
