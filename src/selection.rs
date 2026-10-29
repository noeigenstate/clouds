//! Detects text selections anywhere on screen and captures the selected text.
//!
//! A low-level mouse hook watches for a drag (left button moved while held) or a
//! double-click. A worker thread then sends Ctrl+C to the foreground window and
//! reads the clipboard, restoring its previous text afterwards. Remote-desktop
//! viewers forward Ctrl+C to the remote machine and sync the clipboard back,
//! which is slower, so they get a longer wait; if nothing comes back (no
//! clipboard sync, VNC, screen sharing) the selected area is read with OCR.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use windows::core::PWSTR;
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::System::DataExchange::GetClipboardSequenceNumber;
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::HiDpi::{SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    keybd_event, GetAsyncKeyState, GetDoubleClickTime, KEYEVENTF_KEYUP, VK_C, VK_CONTROL, VK_LWIN, VK_MENU, VK_RWIN,
    VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetAncestor, GetClassNameW, GetCursorInfo, GetForegroundWindow, GetMessageW, GetSystemMetrics,
    GetWindowRect, LoadCursorW, SetWindowsHookExW, WindowFromPoint, CURSORINFO, GA_ROOT, IDC_SIZEALL, IDC_SIZENESW,
    IDC_SIZENS, IDC_SIZENWSE, IDC_SIZEWE, MSG, MSLLHOOKSTRUCT, SM_CXDOUBLECLK, SM_CYDOUBLECLK, WH_MOUSE_LL,
    WM_LBUTTONDOWN, WM_LBUTTONUP,
};

use crate::window::window_pid;
use crate::{clipboard, log, ocr};

#[derive(Clone, Copy, Debug)]
pub enum Gesture {
    Drag { from: POINT, to: POINT },
    DoubleClick { at: POINT },
}

/// Process names (lower case) of remote-desktop and screen-viewer clients.
const REMOTE_APPS: &[&str] = &[
    "mstsc.exe",
    "msrdc.exe",
    "msrdcw.exe",
    "windows365.exe",
    "rdcman.exe",
    "vmconnect.exe",
    "todesk.exe",
    "teamviewer.exe",
    "anydesk.exe",
    "rustdesk.exe",
    "sunloginclient.exe",
    "awesun.exe",
    "vncviewer.exe",
    "tvnviewer.exe",
    "parsecd.exe",
    "wfica32.exe",
    "cdviewer.exe",
    "nxplayer.bin",
    "strwinclt.exe",
    "vmware.exe",
    "vmplayer.exe",
    "virtualboxvm.exe",
    "royalts.exe",
    "mremoteng.exe",
];

/// Consoles treat Ctrl+C as "interrupt", so never send it there.
const CONSOLE_CLASSES: &[&str] =
    &["ConsoleWindowClass", "CASCADIA_HOSTING_WINDOW_CLASS", "mintty", "VirtualConsoleClass"];

/// Screenshot tools: dragging there draws a snip, it never selects text.
const SKIP_APPS: &[&str] = &["screenclippinghost.exe", "snippingtool.exe", "screensketch.exe"];

/// Standard dialog boxes (message boxes, task dialogs): Ctrl+C there copies the
/// whole dialog ("[Window Title] ..."), selection or not, so only text boxes in
/// them are copied from.
const DIALOG_CLASS: &str = "#32770";

/// Button-down captures are requested here, so the hook never waits for pixels.
enum MouseEvent {
    Down { window: isize, point: POINT, at: Instant },
    Up { gesture: Option<Gesture>, at: Instant },
}

static GESTURES: OnceLock<Sender<MouseEvent>> = OnceLock::new();

/// Starts watching for selections; `on_text` gets each newly selected text.
pub fn start(paused: Arc<AtomicBool>, on_text: impl Fn(String) + Send + 'static) {
    let (tx, rx) = mpsc::channel();
    GESTURES.set(tx).ok();
    thread::spawn(hook_thread);
    thread::spawn(move || worker(rx, paused, on_text));
}

fn hook_thread() {
    unsafe {
        let _ = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        if SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), None, 0).is_err() {
            log!("could not install the mouse hook");
            return;
        }
        // Low-level hooks are called on this thread, so it must pump messages.
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {}
    }
}

/// What the hook remembers between button events. Only touched on the hook thread.
struct ClickState {
    down: Option<POINT>,
    /// The press began on a resize/move handle, not on text.
    down_on_handle: bool,
    /// The window under the press and its rectangle then, to spot window drags.
    down_window: Option<(HWND, RECT)>,
    last_up: Option<(POINT, Instant)>,
}

static mut CLICKS: ClickState = ClickState { down: None, down_on_handle: false, down_window: None, last_up: None };

unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        let point = (*(lparam.0 as *const MSLLHOOKSTRUCT)).pt;
        let clicks = &mut *std::ptr::addr_of_mut!(CLICKS);
        match wparam.0 as u32 {
            WM_LBUTTONDOWN => {
                let window = GetAncestor(WindowFromPoint(point), GA_ROOT);
                clicks.down = Some(point);
                clicks.down_on_handle = on_handle_cursor();
                clicks.down_window = Some((window, rect_of(window)));
                if let Some(tx) = GESTURES.get() {
                    let _ = tx.send(MouseEvent::Down { window: window.0 as isize, point, at: Instant::now() });
                }
            }
            WM_LBUTTONUP => {
                let at = Instant::now();
                let gesture = button_released(clicks, point);
                if let Some(tx) = GESTURES.get() {
                    let _ = tx.send(MouseEvent::Up { gesture, at });
                }
            }
            _ => {}
        }
    }
    CallNextHookEx(None, code, wparam, lparam)
}

/// Turns a button release into a drag or double-click, unless the press was on
/// a resize handle or it moved a window (title bar or edge drag).
unsafe fn button_released(clicks: &mut ClickState, up: POINT) -> Option<Gesture> {
    let down = clicks.down.take()?;
    let now = Instant::now();
    let (dx, dy) = (up.x - down.x, up.y - down.y);
    let is_double_click = clicks.last_up.is_some_and(|(p, t)| {
        now - t <= Duration::from_millis(GetDoubleClickTime() as u64)
            && (p.x - up.x).abs() <= GetSystemMetrics(SM_CXDOUBLECLK)
            && (p.y - up.y).abs() <= GetSystemMetrics(SM_CYDOUBLECLK)
    });
    clicks.last_up = Some((up, now));
    let gesture = if dx * dx + dy * dy >= 36 {
        Gesture::Drag { from: down, to: up }
    } else if is_double_click {
        Gesture::DoubleClick { at: up }
    } else {
        return None;
    };

    let moved_window = clicks.down_window.take().is_some_and(|(w, r)| rect_of(w) != r);
    if clicks.down_on_handle {
        log!("gesture ignored: started on a resize handle");
        None
    } else if moved_window {
        log!("gesture ignored: a window was moved or resized");
        None
    } else {
        Some(gesture)
    }
}

/// True when the pointer is a resize/move handle: dragging that resizes a pane,
/// it doesn't select. The arrow and the hand still count, because apps and web
/// pages show them over selectable (and clickable) text; dragging a real link
/// selects nothing, so nothing gets copied anyway.
unsafe fn on_handle_cursor() -> bool {
    static HANDLES: OnceLock<Vec<isize>> = OnceLock::new();
    let handles = HANDLES.get_or_init(|| {
        [IDC_SIZEALL, IDC_SIZENS, IDC_SIZEWE, IDC_SIZENESW, IDC_SIZENWSE]
            .iter()
            .filter_map(|&id| LoadCursorW(None, id).ok().map(|c| c.0 as isize))
            .collect()
    });
    let mut info = CURSORINFO { cbSize: std::mem::size_of::<CURSORINFO>() as u32, ..Default::default() };
    GetCursorInfo(&mut info).is_ok() && handles.contains(&(info.hCursor.0 as isize))
}

/// Pairs each release with its own timely baseline, including during debounce.
fn selection_event(
    event: MouseEvent,
    baseline: &mut Option<ocr::Baseline>,
    paused: bool,
    capture: impl FnOnce(isize, POINT, Instant) -> Option<ocr::Baseline>,
) -> Option<(Gesture, Option<ocr::Baseline>)> {
    match event {
        MouseEvent::Down { window, point, at } => {
            *baseline = if paused { None } else { capture(window, point, at) };
            None
        }
        MouseEvent::Up { gesture, at } => {
            let before = baseline.take().filter(|b| b.ready_by(at));
            gesture.map(|g| (g, before))
        }
    }
}

fn needs_baseline(process: &str) -> bool {
    REMOTE_APPS.contains(&process) && !remote_clipboard_works(process)
}

fn baseline_for_press(window: isize, point: POINT, at: Instant) -> Option<ocr::Baseline> {
    let window = HWND(window as *mut _);
    if unsafe { process_name(window) }.is_some_and(|p| needs_baseline(&p)) {
        ocr::Baseline::capture(window, point, at)
    } else {
        None
    }
}

fn worker(rx: Receiver<MouseEvent>, paused: Arc<AtomicBool>, on_text: impl Fn(String)) {
    unsafe {
        let _ = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let mut last = String::new();
    let mut baseline = None;
    while let Ok(event) = rx.recv() {
        let Some((mut gesture, mut before)) =
            selection_event(event, &mut baseline, paused.load(Ordering::Relaxed), baseline_for_press)
        else {
            continue;
        };
        // Give the app a moment to finish updating its selection; a burst of
        // gestures (a triple-click) collapses into the latest one.
        thread::sleep(Duration::from_millis(30));
        while let Ok(event) = rx.try_recv() {
            if let Some(latest) =
                selection_event(event, &mut baseline, paused.load(Ordering::Relaxed), baseline_for_press)
            {
                (gesture, before) = latest;
            }
        }
        if paused.load(Ordering::Relaxed) {
            log!("gesture ignored: paused");
            continue;
        }
        // A panic here must not end the worker, or every later selection is lost.
        let handled = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let Some(text) = capture(gesture, before.as_ref()) else { return };
            if text == last {
                log!("same text as last time, skipped");
                return;
            }
            last = text.clone();
            on_text(text);
        }));
        if handled.is_err() {
            log!("panic while handling a selection");
        }
    }
}

fn capture(gesture: Gesture, baseline: Option<&ocr::Baseline>) -> Option<String> {
    let start = match gesture {
        Gesture::Drag { from, .. } => from,
        Gesture::DoubleClick { at } => at,
    };
    unsafe {
        // Ignore clicks and drags on our own windows.
        let own = std::process::id();
        if window_pid(GetAncestor(WindowFromPoint(start), GA_ROOT)) == own {
            return None;
        }
        let fg = GetForegroundWindow();
        if fg.is_invalid() || window_pid(fg) == own {
            log!("gesture ignored: no foreground app, or it is this one");
            return None;
        }
        let class = class_name(fg);
        let process = process_name(fg).unwrap_or_default();
        log!("{gesture:?} in {process} [{class}]");
        if CONSOLE_CLASSES.contains(&class.as_str()) || SKIP_APPS.contains(&process.as_str()) {
            log!("skipped: console or screenshot tool");
            return None;
        }
        if class == DIALOG_CLASS && !is_text_box(WindowFromPoint(start)) {
            log!("skipped: a dialog box, and not on a text box");
            return None;
        }

        let remote = REMOTE_APPS.contains(&process.as_str());
        if !remote {
            let text = copy_selection(Duration::from_millis(450))?;
            log!("copied {} characters", text.chars().count());
            return Some(text);
        }
        if remote_clipboard_works(&process) {
            if let Some(text) = copy_selection(Duration::from_millis(1500)) {
                log!("copied {} characters", text.chars().count());
                remote_clipboard_result(&process, true);
                return Some(text);
            }
            remote_clipboard_result(&process, false);
        } else {
            log!("{process}: the clipboard hasn't come back lately, reading the screen");
        }
        let text = ocr::read_selection(gesture, baseline);
        log!("OCR read {} characters", text.as_ref().map_or(0, |t| t.chars().count()));
        text
    }
}

/// After this many misses in a row, a remote viewer's clipboard is not tried for
/// a while: each try waits 1.5 s, and sends Ctrl+C into the remote session.
const REMOTE_MISSES: u32 = 2;
const REMOTE_RETRY_AFTER: Duration = Duration::from_secs(300);

/// Per remote viewer: misses in a row, and when the last one happened.
fn remote_misses() -> &'static Mutex<HashMap<String, (u32, Instant)>> {
    static MISSES: OnceLock<Mutex<HashMap<String, (u32, Instant)>>> = OnceLock::new();
    MISSES.get_or_init(Default::default)
}

/// Whether to try the clipboard for this remote viewer (it shares the
/// clipboard, or hasn't been tried recently).
fn remote_clipboard_works(process: &str) -> bool {
    match remote_misses().lock().unwrap().get(process) {
        Some(&(misses, last)) => misses < REMOTE_MISSES || last.elapsed() > REMOTE_RETRY_AFTER,
        None => true,
    }
}

fn remote_clipboard_result(process: &str, worked: bool) {
    let mut misses = remote_misses().lock().unwrap();
    if worked {
        misses.remove(process);
    } else {
        let entry = misses.entry(process.to_string()).or_insert((0, Instant::now()));
        *entry = (entry.0 + 1, Instant::now());
    }
}

/// Sends Ctrl+C and returns the copied text, then puts the old clipboard text back.
fn copy_selection(timeout: Duration) -> Option<String> {
    if !wait_for_modifiers_released() {
        log!("no copy: a modifier key was still held");
        return None;
    }
    unsafe {
        let saved = clipboard::get_text();
        let seq = GetClipboardSequenceNumber();
        keybd_event(VK_CONTROL.0 as u8, 0, Default::default(), 0);
        keybd_event(VK_C.0 as u8, 0, Default::default(), 0);
        keybd_event(VK_C.0 as u8, 0, KEYEVENTF_KEYUP, 0);
        keybd_event(VK_CONTROL.0 as u8, 0, KEYEVENTF_KEYUP, 0);

        let deadline = Instant::now() + timeout;
        while GetClipboardSequenceNumber() == seq {
            if Instant::now() > deadline {
                log!("no copy: the clipboard did not change (nothing selected?)");
                return None;
            }
            thread::sleep(Duration::from_millis(5));
        }
        // Remote clients announce the data first and fetch it on demand; give them a moment.
        let text = (0..20).find_map(|_| {
            let text = clipboard::get_text();
            if text.is_none() {
                thread::sleep(Duration::from_millis(10));
            }
            text
        });
        if let Some(saved) = saved {
            clipboard::set_text(&saved);
        }
        text.map(|t| t.trim().to_string()).filter(|t| !t.is_empty())
    }
}

/// Waits (up to a second) until Shift, Ctrl, Alt and Win are up: held from the
/// selection, they would turn our Ctrl+C into another shortcut.
fn wait_for_modifiers_released() -> bool {
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        let held = [VK_SHIFT, VK_CONTROL, VK_MENU, VK_LWIN, VK_RWIN]
            .iter()
            .any(|k| unsafe { GetAsyncKeyState(k.0 as i32) } as u16 & 0x8000 != 0);
        if !held {
            return true;
        }
        if Instant::now() > deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

unsafe fn rect_of(hwnd: HWND) -> RECT {
    let mut r = RECT::default();
    let _ = GetWindowRect(hwnd, &mut r);
    r
}

unsafe fn class_name(hwnd: HWND) -> String {
    let mut buf = [0u16; 128];
    let len = GetClassNameW(hwnd, &mut buf) as usize;
    String::from_utf16_lossy(&buf[..len])
}

/// Edit and rich-edit controls: the only places in a dialog box that select text.
unsafe fn is_text_box(hwnd: HWND) -> bool {
    let class = class_name(hwnd).to_lowercase();
    class == "edit" || class.starts_with("richedit")
}

/// The executable's file name, lower case (e.g. "mstsc.exe").
unsafe fn process_name(hwnd: HWND) -> Option<String> {
    let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, window_pid(hwnd)).ok()?;
    let mut buf = [0u16; 512];
    let mut len = buf.len() as u32;
    let ok = QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len);
    let _ = CloseHandle(process);
    ok.ok()?;
    let path = String::from_utf16_lossy(&buf[..len as usize]);
    path.rsplit('\\').next().map(str::to_lowercase)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_event_pairs_down_and_up_during_debounce() {
        let pressed = Instant::now();
        let point = POINT { x: 15, y: 20 };
        let mut baseline = None;
        let first_ready = pressed + Duration::from_millis(1);
        let second_ready = pressed + Duration::from_millis(11);
        let down = MouseEvent::Down { window: 7, point, at: pressed };
        assert!(selection_event(down, &mut baseline, false, |window, at, time| {
            assert_eq!(window, 7);
            assert_eq!(at, point);
            assert_eq!(time, pressed);
            Some(ocr::Baseline::for_test(first_ready))
        })
        .is_none());
        let first = selection_event(
            MouseEvent::Up {
                gesture: Some(Gesture::Drag { from: point, to: POINT { x: 185, y: 20 } }),
                at: pressed + Duration::from_millis(5),
            },
            &mut baseline,
            false,
            |_, _, _| panic!("a release must not capture"),
        )
        .expect("first release");
        assert!(baseline.is_none());

        // A new press during debounce must not replace the completed gesture's baseline.
        selection_event(
            MouseEvent::Down { window: 7, point, at: pressed + Duration::from_millis(10) },
            &mut baseline,
            false,
            |_, _, _| Some(ocr::Baseline::for_test(second_ready)),
        );
        assert!(first.1.as_ref().is_some_and(|b| b.ready_by(first_ready)));
        assert!(!baseline.as_ref().expect("pending second press").ready_by(first_ready));
        let second = selection_event(
            MouseEvent::Up {
                gesture: Some(Gesture::DoubleClick { at: point }),
                at: pressed + Duration::from_millis(15),
            },
            &mut baseline,
            false,
            |_, _, _| panic!("a release must not capture"),
        )
        .expect("second release");
        assert!(matches!(second.0, Gesture::DoubleClick { .. }));
        assert!(second.1.as_ref().is_some_and(|b| b.ready_by(second_ready) && !b.ready_by(first_ready)));
        assert!(baseline.is_none());
    }

    #[test]
    fn selection_event_discards_unpaired_or_late_baselines() {
        let pressed = Instant::now();
        let point = POINT { x: 15, y: 20 };
        let mut baseline = Some(ocr::Baseline::for_test(pressed));
        assert!(selection_event(
            MouseEvent::Up { gesture: None, at: pressed },
            &mut baseline,
            false,
            |_, _, _| panic!("a release must not capture"),
        )
        .is_none());
        assert!(baseline.is_none());
        baseline = Some(ocr::Baseline::for_test(pressed + Duration::from_millis(1)));
        let late = selection_event(
            MouseEvent::Up { gesture: Some(Gesture::DoubleClick { at: point }), at: pressed },
            &mut baseline,
            false,
            |_, _, _| panic!("a release must not capture"),
        )
        .expect("gesture survives a late baseline");
        assert!(late.1.is_none());
        assert!(baseline.is_none());
        baseline = Some(ocr::Baseline::for_test(pressed));
        selection_event(MouseEvent::Down { window: 7, point, at: pressed }, &mut baseline, false, |_, _, _| None);
        let missing = selection_event(
            MouseEvent::Up { gesture: Some(Gesture::DoubleClick { at: point }), at: pressed },
            &mut baseline,
            false,
            |_, _, _| panic!("a release must not capture"),
        )
        .expect("gesture survives a failed capture");
        assert!(missing.1.is_none());
        baseline = Some(ocr::Baseline::for_test(pressed));
        selection_event(MouseEvent::Down { window: 7, point, at: pressed }, &mut baseline, true, |_, _, _| {
            panic!("a paused press must not capture")
        });
        assert!(baseline.is_none());
    }

    #[test]
    fn baseline_skips_working_clipboards_and_local_apps() {
        for process in ["mstsc.exe", "notepad.exe"] {
            remote_clipboard_result(process, true);
            assert!(!needs_baseline(process));
            remote_clipboard_result(process, false);
            assert!(!needs_baseline(process));
            remote_clipboard_result(process, false);
            assert_eq!(needs_baseline(process), process == "mstsc.exe");
            remote_clipboard_result(process, true);
            assert!(!needs_baseline(process));
        }
    }
}
