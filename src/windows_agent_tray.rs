use crate::management_control;
use crate::management_discovery::AgentState;
use crate::management_protocol::MANAGEMENT_CONTROL_PORT;
use crate::windows_icon;
use crate::windows_notification::{self, NotificationKind};
use std::io;
use std::mem::size_of;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::process;
use std::ptr::{null, null_mut};
use std::thread;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
    Shell_NotifyIconW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    DispatchMessageW, GetCursorPos, GetMessageW, HWND_MESSAGE, KillTimer, MF_GRAYED, MF_SEPARATOR,
    MF_STRING, MSG, PostMessageW, PostQuitMessage, RegisterClassW, SetForegroundWindow, SetTimer,
    TPM_LEFTALIGN, TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenu, TranslateMessage,
    WM_APP, WM_DESTROY, WM_LBUTTONDBLCLK, WM_NULL, WM_RBUTTONUP, WM_TIMER, WNDCLASSW,
};

const TRAY_ICON_ID: u32 = 1;

const TRAY_CALLBACK_MESSAGE: u32 = WM_APP + 1;

const TRAY_TIMER_ID: usize = 1;

const TRAY_REFRESH_MILLISECONDS: u32 = 2_000;

const MENU_SHOW_STATUS: u32 = 1_001;

const MENU_EXIT_IDLE: u32 = 1_002;

#[derive(Clone, Debug, Eq, PartialEq)]
enum AgentTrayStatus {
    Idle { hostname: String },

    Busy { hostname: String },

    Unavailable { message: String },
}

impl AgentTrayStatus {
    const fn is_idle(&self) -> bool {
        matches!(self, Self::Idle { .. })
    }

    fn short_label(&self) -> String {
        match self {
            Self::Idle { .. } => "idle".to_string(),

            Self::Busy { .. } => "transfer active".to_string(),

            Self::Unavailable { .. } => "starting or unavailable".to_string(),
        }
    }

    fn tooltip(&self) -> String {
        format!("NetworkCopy Agent — {}", self.short_label(),)
    }
}

pub fn spawn() -> io::Result<()> {
    thread::Builder::new()
        .name("networkcopy-agent-tray".to_string())
        .spawn(|| {
            if let Err(error) = run_tray() {
                eprintln!("NetworkCopy Agent tray failed: {error}",);
            }
        })
        .map(|_| ())
}

fn run_tray() -> io::Result<()> {
    let instance = unsafe { GetModuleHandleW(null()) };

    if instance.is_null() {
        return Err(io::Error::last_os_error());
    }

    let class_name = wide("NetworkCopyAgentTrayWindow");

    let window_title = wide("NetworkCopy Agent");

    let window_class = WNDCLASSW {
        lpfnWndProc: Some(tray_window_proc),

        hInstance: instance,

        lpszClassName: class_name.as_ptr(),

        ..Default::default()
    };

    let registered = unsafe { RegisterClassW(&window_class) };

    if registered == 0 {
        return Err(io::Error::last_os_error());
    }

    let window = unsafe {
        CreateWindowExW(
            0,
            class_name.as_ptr(),
            window_title.as_ptr(),
            0,
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            null_mut(),
            instance,
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

    let mut data = tray_icon_data(window, NIF_ICON | NIF_MESSAGE | NIF_TIP)?;

    data.uCallbackMessage = TRAY_CALLBACK_MESSAGE;

    data.hIcon = icon.raw();

    copy_wide_truncated(&mut data.szTip, &query_agent_status().tooltip());

    let added = unsafe { Shell_NotifyIconW(NIM_ADD, &data) };

    if added == 0 {
        unsafe {
            DestroyWindow(window);
        }

        return Err(io::Error::other(
            "Shell_NotifyIconW failed to add the agent tray icon",
        ));
    }

    let timer = unsafe { SetTimer(window, TRAY_TIMER_ID, TRAY_REFRESH_MILLISECONDS, None) };

    if timer == 0 {
        delete_tray_icon(window);

        unsafe {
            DestroyWindow(window);
        }

        return Err(io::Error::last_os_error());
    }

    let mut message = MSG::default();

    loop {
        let result = unsafe { GetMessageW(&mut message, null_mut(), 0, 0) };

        if result == -1 {
            let error = io::Error::last_os_error();

            unsafe {
                KillTimer(window, TRAY_TIMER_ID);
            }

            delete_tray_icon(window);

            unsafe {
                DestroyWindow(window);
            }

            return Err(error);
        }

        if result == 0 {
            break;
        }

        unsafe {
            TranslateMessage(&message);

            DispatchMessageW(&message);
        }
    }

    unsafe {
        KillTimer(window, TRAY_TIMER_ID);
    }

    delete_tray_icon(window);

    unsafe {
        DestroyWindow(window);
    }

    Ok(())
}

unsafe extern "system" fn tray_window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        TRAY_CALLBACK_MESSAGE => {
            match lparam as u32 {
                WM_LBUTTONDBLCLK => {
                    show_status_notification();
                }

                WM_RBUTTONUP => {
                    if let Err(error) = show_context_menu(window) {
                        let body = format!("The agent tray menu could not open: {error}",);

                        let _ = windows_notification::show(
                            NotificationKind::Error,
                            "Agent tray error",
                            &body,
                        );
                    }
                }

                _ => {}
            }

            0
        }

        WM_TIMER if wparam == TRAY_TIMER_ID => {
            if let Err(error) = update_tooltip(window) {
                eprintln!("Agent tray tooltip update failed: {error}",);
            }

            0
        }

        WM_DESTROY => {
            unsafe {
                PostQuitMessage(0);
            }

            0
        }

        _ => unsafe { DefWindowProcW(window, message, wparam, lparam) },
    }
}

fn show_context_menu(window: HWND) -> io::Result<()> {
    let status = query_agent_status();

    let menu = unsafe { CreatePopupMenu() };

    if menu.is_null() {
        return Err(io::Error::last_os_error());
    }

    let result = (|| {
        append_menu_text(
            menu,
            0,
            MF_STRING | MF_GRAYED,
            &format!("NetworkCopy Agent — {}", status.short_label(),),
        )?;

        append_menu_separator(menu)?;

        append_menu_text(menu, MENU_SHOW_STATUS, MF_STRING, "Show status")?;

        let exit_flags = if status.is_idle() {
            MF_STRING
        } else {
            MF_STRING | MF_GRAYED
        };

        append_menu_text(menu, MENU_EXIT_IDLE, exit_flags, "Exit idle agent")?;

        let mut cursor = POINT::default();

        let positioned = unsafe { GetCursorPos(&mut cursor) };

        if positioned == 0 {
            return Err(io::Error::last_os_error());
        }

        unsafe {
            SetForegroundWindow(window);
        }

        let command = unsafe {
            TrackPopupMenu(
                menu,
                TPM_LEFTALIGN | TPM_RIGHTBUTTON | TPM_RETURNCMD | TPM_NONOTIFY,
                cursor.x,
                cursor.y,
                0,
                window,
                null(),
            )
        };

        unsafe {
            let _ = PostMessageW(window, WM_NULL, 0, 0);
        }

        match command as u32 {
            MENU_SHOW_STATUS => {
                show_status_notification();
            }

            MENU_EXIT_IDLE => {
                exit_idle_agent(window);
            }

            _ => {}
        }

        Ok(())
    })();

    unsafe {
        DestroyMenu(menu);
    }

    result
}

fn append_menu_text(
    menu: *mut core::ffi::c_void,
    identifier: u32,
    flags: u32,
    text: &str,
) -> io::Result<()> {
    let text = wide(text);

    let appended = unsafe { AppendMenuW(menu, flags, identifier as usize, text.as_ptr()) };

    if appended == 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(())
}

fn append_menu_separator(menu: *mut core::ffi::c_void) -> io::Result<()> {
    let appended = unsafe { AppendMenuW(menu, MF_SEPARATOR, 0, null()) };

    if appended == 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(())
}

fn update_tooltip(window: HWND) -> io::Result<()> {
    let mut data = tray_icon_data(window, NIF_TIP)?;

    copy_wide_truncated(&mut data.szTip, &query_agent_status().tooltip());

    let modified = unsafe { Shell_NotifyIconW(NIM_MODIFY, &data) };

    if modified == 0 {
        return Err(io::Error::other(
            "Shell_NotifyIconW failed to update the agent tooltip",
        ));
    }

    Ok(())
}

fn show_status_notification() {
    let status = query_agent_status();

    let (kind, title, body) = match status {
        AgentTrayStatus::Idle { hostname } => (
            NotificationKind::Information,
            "NetworkCopy Agent is idle",
            format!("{hostname} is ready to send or receive a managed transfer.",),
        ),

        AgentTrayStatus::Busy { hostname } => (
            NotificationKind::Information,
            "NetworkCopy transfer active",
            format!("{hostname} currently has an active managed transfer.",),
        ),

        AgentTrayStatus::Unavailable { message } => (
            NotificationKind::Warning,
            "NetworkCopy Agent unavailable",
            message,
        ),
    };

    if let Err(error) = windows_notification::show(kind, title, &body) {
        eprintln!("Agent status notification failed: {error}",);
    }
}

fn exit_idle_agent(window: HWND) {
    match query_agent_status() {
        AgentTrayStatus::Idle { .. } => {
            delete_tray_icon(window);

            process::exit(0);
        }

        AgentTrayStatus::Busy { hostname } => {
            let body = format!(
                "{hostname} has an active transfer. Exit was refused so the transfer remains untouched.",
            );

            let _ = windows_notification::show(NotificationKind::Warning, "Agent is busy", &body);
        }

        AgentTrayStatus::Unavailable { message } => {
            let _ = windows_notification::show(
                NotificationKind::Error,
                "Agent state unavailable",
                &message,
            );
        }
    }
}

fn query_agent_status() -> AgentTrayStatus {
    match management_control::hello(local_agent_endpoint()) {
        Ok(agent) => match agent.state {
            AgentState::Idle => AgentTrayStatus::Idle {
                hostname: agent.hostname,
            },

            AgentState::Busy => AgentTrayStatus::Busy {
                hostname: agent.hostname,
            },
        },

        Err(error) => AgentTrayStatus::Unavailable {
            message: format!("The local management endpoint did not answer: {error}",),
        },
    }
}

fn local_agent_endpoint() -> SocketAddr {
    SocketAddr::V4(SocketAddrV4::new(
        Ipv4Addr::LOCALHOST,
        MANAGEMENT_CONTROL_PORT,
    ))
}

fn tray_icon_data(window: HWND, flags: u32) -> io::Result<NOTIFYICONDATAW> {
    let structure_size = u32::try_from(size_of::<NOTIFYICONDATAW>())
        .map_err(|_| io::Error::other("tray icon structure size cannot be represented"))?;

    Ok(NOTIFYICONDATAW {
        cbSize: structure_size,

        hWnd: window,

        uID: TRAY_ICON_ID,

        uFlags: flags,

        ..Default::default()
    })
}

fn delete_tray_icon(window: HWND) {
    let Ok(data) = tray_icon_data(window, 0) else {
        return;
    };

    unsafe {
        let _ = Shell_NotifyIconW(NIM_DELETE, &data);
    }
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

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::AgentTrayStatus;

    #[test]
    fn tray_status_controls_idle_exit() {
        let idle = AgentTrayStatus::Idle {
            hostname: "SOURCE-PC".to_string(),
        };

        let busy = AgentTrayStatus::Busy {
            hostname: "SOURCE-PC".to_string(),
        };

        assert!(idle.is_idle());

        assert!(!busy.is_idle());

        assert_eq!(idle.tooltip(), "NetworkCopy Agent — idle",);

        assert_eq!(busy.tooltip(), "NetworkCopy Agent — transfer active",);
    }
}
