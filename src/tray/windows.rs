//! The notification-area icon: a hidden window, a shell icon, and a popup menu.
//!
//! The window procedure is deliberately almost empty. It cannot own a closure — it is an `extern "system"`
//! function — and the usual answer, stashing a pointer in the window's user data and casting it back, means
//! unsafe code holding a borrow across a callback the operating system decides when to invoke. Instead the
//! procedure writes to three atomics and every stateful thing happens in [`run`], which is ordinary safe
//! Rust. That leaves the FFI surface small enough to read in one sitting.
//!
//! Two behaviours here are the entire difficulty of a tray icon, and both are invisible until they bite:
//!
//! - **`WM_TASKBARCREATED`.** When Explorer restarts, every notification icon is discarded and never comes
//!   back unless its owner re-adds it. Without this the icon disappears permanently and the daemon appears
//!   to have died.
//! - **`SetForegroundWindow` before `TrackPopupMenu`.** A popup owned by a window that is not in the
//!   foreground does not dismiss when the user clicks elsewhere; it sits there until something else takes
//!   focus.

use super::{Item, Status, is_stopping};
use anyhow::{Context, Result, bail};
use std::{
    ffi::OsStr,
    iter::once,
    mem::size_of,
    os::windows::ffi::OsStrExt,
    ptr,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    thread,
    time::Duration,
};
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM},
    System::LibraryLoader::GetModuleHandleW,
    UI::{
        Shell::{
            NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW, Shell_NotifyIconW,
        },
        WindowsAndMessaging::{
            AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyIcon,
            DestroyMenu, DestroyWindow, DispatchMessageW, GetCursorPos, GetSystemMetrics, HICON,
            IDI_APPLICATION, IMAGE_ICON, LR_DEFAULTCOLOR, LoadIconW, LoadImageW, MF_STRING, MSG,
            PM_REMOVE, PeekMessageW, RegisterClassW, RegisterWindowMessageW, SM_CXSMICON,
            SM_CYSMICON, SetForegroundWindow, TPM_RETURNCMD, TPM_RIGHTALIGN, TrackPopupMenu,
            TranslateMessage, WM_APP, WM_DESTROY, WM_LBUTTONDBLCLK, WM_QUIT, WM_RBUTTONUP,
            WNDCLASSW, WS_EX_TOOLWINDOW, WS_OVERLAPPED,
        },
    },
};

/// Message the shell sends for activity on our icon.
const TRAY_MESSAGE: u32 = WM_APP + 1;

/// Identifier of our icon within this window. Only one, so a constant.
const ICON_ID: u32 = 1;

/// Resource identifier of the application icon inside this executable.
///
/// Set by `build.rs`, which links `branding/agentbench.ico` in under this number. The two have to agree, and
/// the number is also what makes the shell use this icon for the executable itself: it shows the
/// lowest-numbered icon resource it finds.
const ICON_RESOURCE_ID: usize = 1;

/// How often the loop wakes to look at the shutdown flag and the atomics below.
///
/// Polling rather than a blocking `GetMessageW`, because the loop has a second thing to watch: the daemon
/// thread may stop on its own, and the icon should go away when it does. A tenth of a second of latency on a
/// menu click is imperceptible, and the alternative — a timer message purely to interrupt a blocking wait —
/// is more moving parts for the same result.
const POLL: Duration = Duration::from_millis(100);

/// Set by the window procedure when the icon was right-clicked.
static SHOW_MENU: AtomicBool = AtomicBool::new(false);

/// Set when Explorer restarted and the icon must be added again.
static REBUILD: AtomicBool = AtomicBool::new(false);

/// The chosen menu command, or zero for none.
///
/// Zero is safe as "none" because [`Item::command_id`] never returns it — see that function's comment.
static CHOSEN: AtomicU32 = AtomicU32::new(0);

/// The `TaskbarCreated` message number, resolved at startup.
///
/// Zero until registered. Compared against every incoming message, so it must never match by accident, and a
/// message number of zero is `WM_NULL` — which is why the comparison below also checks for non-zero.
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);

pub(super) fn is_supported() -> bool {
    true
}

pub(super) fn run(
    shutdown: Arc<AtomicBool>,
    status: Status,
    mut on: impl FnMut(Item),
) -> Result<()> {
    // These statics make this a once-per-process affair. That is what it is: a process has one notification
    // area presence, and a second concurrent tray in the same process would be a bug rather than a feature.
    let window = Window::create()?;
    TASKBAR_CREATED.store(register_taskbar_created(), Ordering::Relaxed);
    let icon = Icon::add(window.handle, &status.tooltip)?;

    loop {
        pump();
        if REBUILD.swap(false, Ordering::Relaxed) {
            // A failure here is survivable and not worth ending collection over: the daemon keeps running,
            // the user has simply lost the icon until the next Explorer restart.
            let _ = icon.add_again(&status.tooltip);
        }
        if SHOW_MENU.swap(false, Ordering::Relaxed)
            && let Some(item) = show_menu(window.handle)
        {
            CHOSEN.store(item.command_id(), Ordering::Relaxed);
        }
        let chosen = CHOSEN.swap(0, Ordering::Relaxed);
        if let Some(item) = Item::from_command_id(chosen) {
            on(item);
            if item == Item::Quit {
                shutdown.store(true, Ordering::Relaxed);
            }
        }
        if is_stopping(&shutdown) {
            return Ok(());
        }
        thread::sleep(POLL);
    }
}

/// Drain the message queue without blocking.
fn pump() {
    let mut message = MSG::default();
    loop {
        // SAFETY: `message` is a valid, correctly sized structure the call writes into; a null window handle
        // means "any window belonging to this thread".
        let received = unsafe { PeekMessageW(&raw mut message, ptr::null_mut(), 0, 0, PM_REMOVE) };
        if received == 0 {
            return;
        }
        if message.message == WM_QUIT {
            return;
        }
        // SAFETY: both take a message this thread just received and own nothing beyond the call.
        unsafe {
            TranslateMessage(&raw const message);
            DispatchMessageW(&raw const message);
        }
    }
}

/// A hidden window that exists only to receive the shell's messages.
struct Window {
    handle: HWND,
}

impl Window {
    fn create() -> Result<Self> {
        let class_name = wide("AgentBenchTray");
        // SAFETY: a null module name asks for the handle of the running executable, which always exists.
        let instance = unsafe { GetModuleHandleW(ptr::null()) };
        let class = WNDCLASSW {
            lpfnWndProc: Some(procedure),
            hInstance: instance,
            lpszClassName: class_name.as_ptr(),
            ..Default::default()
        };
        // SAFETY: `class` borrows `class_name`, which outlives the call. Re-registering the same class name
        // fails rather than corrupting anything, and this runs once per process.
        unsafe { RegisterClassW(&raw const class) };
        // A real window rather than a message-only one, even though it is never shown. A message-only window
        // cannot become the foreground window, and `SetForegroundWindow` on it is what makes the popup menu
        // dismissable — so the cheaper option would cost exactly the behaviour being paid for.
        // SAFETY: every pointer is either null or into a wide string that outlives the call.
        let handle = unsafe {
            CreateWindowExW(
                WS_EX_TOOLWINDOW,
                class_name.as_ptr(),
                class_name.as_ptr(),
                WS_OVERLAPPED,
                0,
                0,
                0,
                0,
                ptr::null_mut(),
                ptr::null_mut(),
                instance,
                ptr::null(),
            )
        };
        if handle.is_null() {
            bail!("could not create the window the tray icon needs");
        }
        Ok(Self { handle })
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        // SAFETY: `self.handle` came from a successful `CreateWindowExW` and is destroyed once.
        unsafe {
            DestroyWindow(self.handle);
        }
    }
}

/// The icon in the notification area, removed when dropped.
///
/// Removal matters more than it looks: an icon whose owning process exits without deleting it stays in the
/// notification area as a dead entry until the user hovers over it.
struct Icon {
    window: HWND,
    /// The image the shell shows, loaded once and reused.
    ///
    /// Loaded once rather than per [`Icon::data`] call because `data` runs again on every Explorer restart,
    /// and that is the one code path written specifically to be repeated — loading there would leak a handle
    /// per restart, in the loop least likely to be watched.
    ///
    /// `None` means the icon resource is not in this executable, which happens when `build.rs` could not
    /// find `rc.exe` and said so as a warning rather than failing. The stock application icon stands in, and
    /// because that handle belongs to the system, the distinction is also what [`Drop`] needs in order to
    /// destroy only what this process owns.
    image: Option<HICON>,
}

impl Icon {
    fn add(window: HWND, tooltip: &str) -> Result<Self> {
        let icon = Self {
            window,
            image: load_image(),
        };
        icon.add_again(tooltip)
            .context("add the icon to the notification area")?;
        Ok(icon)
    }

    fn add_again(&self, tooltip: &str) -> Result<()> {
        let mut data = self.data(tooltip);
        // SAFETY: `data` is correctly sized and fully initialised, and its `szTip` is NUL-terminated.
        let added = unsafe { Shell_NotifyIconW(NIM_ADD, &raw mut data) };
        if added == 0 {
            bail!("the shell refused the notification icon");
        }
        Ok(())
    }

    fn data(&self, tooltip: &str) -> NOTIFYICONDATAW {
        let mut data = NOTIFYICONDATAW {
            cbSize: size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.window,
            uID: ICON_ID,
            uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
            uCallbackMessage: TRAY_MESSAGE,
            ..Default::default()
        };
        data.hIcon = match self.image {
            Some(image) => image,
            // SAFETY: a null instance handle with a predefined icon identifier is the documented way to load
            // a system icon, and the returned handle is owned by the system rather than by this process.
            None => unsafe { LoadIconW(ptr::null_mut(), IDI_APPLICATION) },
        };
        // Truncated to the field's capacity, terminator included. The caller already shortened it; this is
        // the guarantee that a longer one cannot overrun the buffer.
        let encoded = wide(tooltip);
        let limit = data.szTip.len() - 1;
        let length = encoded.len().min(limit);
        data.szTip[..length].copy_from_slice(&encoded[..length]);
        data.szTip[length] = 0;
        data
    }
}

impl Drop for Icon {
    fn drop(&mut self) {
        let mut data = NOTIFYICONDATAW {
            cbSize: size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.window,
            uID: ICON_ID,
            ..Default::default()
        };
        // SAFETY: identifies the icon added earlier by the same window and identifier.
        unsafe {
            Shell_NotifyIconW(NIM_DELETE, &raw mut data);
        }
        if let Some(image) = self.image {
            // After the removal above, not before: the shell is still drawing the icon until `NIM_DELETE`
            // returns, and destroying the image out from under it is what leaves a blank square behind.
            // SAFETY: `image` came from `LoadImageW` without `LR_SHARED`, so this process owns it and is the
            // one required to destroy it. The stock icon takes the other arm and is never passed here.
            unsafe {
                DestroyIcon(image);
            }
        }
    }
}

/// This executable's own icon, at the size the notification area is currently using.
///
/// Three things here are deliberate:
///
/// - **`LoadImageW`, not `LoadIconW`.** `LoadIconW` always returns the 32x32 frame and leaves the shell to
///   shrink it, which throws away the hand-sized 16, 20 and 24 pixel frames that are most of the reason for
///   shipping a multi-frame `.ico` at all. Asking for `SM_CXSMICON` gets the frame that matches the current
///   scaling instead.
/// - **Resource identifier 1.** Set by `build.rs`; changing it there changes it here.
/// - **`None` rather than an error.** A build without `rc.exe` has no icon resource, which `build.rs`
///   reports as a warning because artwork is not worth failing a build over. Refusing to start a daemon over
///   it would be worse still, so the caller falls back to the stock icon.
fn load_image() -> Option<HICON> {
    // SAFETY: a null module name asks for a handle to the running executable, which always exists.
    let instance = unsafe { GetModuleHandleW(ptr::null()) };
    // SAFETY: `GetSystemMetrics` reads a system-wide value and has no failure mode worth branching on — an
    // unrecognised index returns zero, and the two used here are documented constants.
    let (width, height) = unsafe { (GetSystemMetrics(SM_CXSMICON), GetSystemMetrics(SM_CYSMICON)) };
    // SAFETY: the name is an integer resource identifier rather than a pointer into memory, which is what
    // `MAKEINTRESOURCEW` expresses in C; `without_provenance` is the honest spelling of that in Rust, and
    // avoids claiming a provenance the value does not have. `LoadImageW` reads it as a number because the
    // high word is zero.
    let image = unsafe {
        LoadImageW(
            instance,
            ptr::without_provenance(ICON_RESOURCE_ID),
            IMAGE_ICON,
            width,
            height,
            LR_DEFAULTCOLOR,
        )
    };
    (!image.is_null()).then_some(image)
}

/// Show the context menu at the cursor and return what was chosen.
fn show_menu(window: HWND) -> Option<Item> {
    // SAFETY: creates an empty menu; a null return is handled below.
    let menu = unsafe { CreatePopupMenu() };
    if menu.is_null() {
        return None;
    }
    let labels: Vec<Vec<u16>> = Item::ALL.iter().map(|item| wide(item.label())).collect();
    for (item, label) in Item::ALL.iter().zip(&labels) {
        // SAFETY: `label` outlives the call, and the identifier is this item's own.
        unsafe {
            AppendMenuW(menu, MF_STRING, item.command_id() as usize, label.as_ptr());
        }
    }
    let mut point = POINT::default();
    // SAFETY: `point` is a valid out-parameter.
    unsafe {
        GetCursorPos(&raw mut point);
    }
    // Without this the menu will not close when the user clicks away from it. See the module documentation.
    // SAFETY: a valid window handle this process owns.
    unsafe {
        SetForegroundWindow(window);
    }
    // SAFETY: a menu this function created, positioned in screen coordinates, owned by our window.
    // `TPM_RETURNCMD` makes the chosen identifier the return value rather than a posted `WM_COMMAND`, which
    // is why the window procedure needs no command handling at all.
    let chosen = unsafe {
        TrackPopupMenu(
            menu,
            TPM_RIGHTALIGN | TPM_RETURNCMD,
            point.x,
            point.y,
            0,
            window,
            ptr::null(),
        )
    };
    // SAFETY: the menu is not in use once `TrackPopupMenu` has returned.
    unsafe {
        DestroyMenu(menu);
    }
    // Zero means the menu was dismissed without a choice.
    Item::from_command_id(chosen as u32)
}

/// Ask the shell for the message number it uses to announce that the taskbar was recreated.
fn register_taskbar_created() -> u32 {
    let name = wide("TaskbarCreated");
    // SAFETY: `name` is a NUL-terminated wide string that outlives the call.
    unsafe { RegisterWindowMessageW(name.as_ptr()) }
}

/// Records what happened and returns. Everything else is decided in [`run`].
unsafe extern "system" fn procedure(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == TRAY_MESSAGE {
        // The shell packs the mouse event into the low word of `lparam`.
        match (lparam as u32) & 0xFFFF {
            WM_RBUTTONUP => SHOW_MENU.store(true, Ordering::Relaxed),
            WM_LBUTTONDBLCLK => {
                CHOSEN.store(Item::OpenDashboard.command_id(), Ordering::Relaxed);
            }
            _ => {}
        }
        return 0;
    }
    let taskbar_created = TASKBAR_CREATED.load(Ordering::Relaxed);
    if taskbar_created != 0 && message == taskbar_created {
        REBUILD.store(true, Ordering::Relaxed);
        return 0;
    }
    if message == WM_DESTROY {
        return 0;
    }
    // SAFETY: forwarding the arguments this procedure was handed, unmodified.
    unsafe { DefWindowProcW(window, message, wparam, lparam) }
}

/// A NUL-terminated UTF-16 string.
fn wide(value: &str) -> Vec<u16> {
    OsStr::new(value).encode_wide().chain(once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_strings_are_nul_terminated() {
        let encoded = wide("Open dashboard");
        assert_eq!(encoded.last(), Some(&0));
    }

    /// The shell announces an Explorer restart with a registered message, and a zero would mean the
    /// comparison in the window procedure could match `WM_NULL`.
    #[test]
    fn the_taskbar_created_message_registers_to_a_usable_number() {
        assert_ne!(register_taskbar_created(), 0);
    }

    /// Whichever way the icon was obtained, the shell must never be handed a null image.
    ///
    /// `NIF_ICON` tells the shell to draw `hIcon`, and a null one draws a blank square in the notification
    /// area — an outcome indistinguishable, at a glance, from the daemon having failed to start. This pins
    /// the fallback: a build with no icon resource shows the stock icon, not nothing.
    #[test]
    fn the_shell_is_never_handed_a_null_image() {
        let icon = Icon {
            window: ptr::null_mut(),
            image: load_image(),
        };
        let data = icon.data("AgentBench");
        assert!(!data.hIcon.is_null());
        assert_ne!(data.uFlags & NIF_ICON, 0, "the image would not be drawn");
        std::mem::forget(icon);
    }

    /// A tooltip longer than the field must be truncated rather than overrun it. Exercised through the
    /// structure the shell is actually handed.
    #[test]
    fn an_overlong_tooltip_is_truncated_and_terminated() {
        let icon = Icon {
            window: ptr::null_mut(),
            // The stock-icon arm. A test binary is not one of the `bins` the resource is linked into, so
            // this is also what `load_image` would return here.
            image: None,
        };
        let data = icon.data(&"x".repeat(500));
        assert_eq!(data.szTip[data.szTip.len() - 1], 0);
        assert!(
            data.szTip.contains(&0),
            "the tooltip must be terminated somewhere"
        );
        // Not dropped as an `Icon`: that would ask the shell to delete an icon that was never added.
        std::mem::forget(icon);
    }

    #[test]
    fn a_short_tooltip_is_copied_intact() {
        let icon = Icon {
            window: ptr::null_mut(),
            // The stock-icon arm. A test binary is not one of the `bins` the resource is linked into, so
            // this is also what `load_image` would return here.
            image: None,
        };
        let data = icon.data("AgentBench — collecting");
        let end = data
            .szTip
            .iter()
            .position(|unit| *unit == 0)
            .expect("a NUL");
        assert_eq!(
            String::from_utf16_lossy(&data.szTip[..end]),
            "AgentBench — collecting"
        );
        std::mem::forget(icon);
    }
}
