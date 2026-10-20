//! Win32 details of the cloud window: no taskbar button, never steals focus,
//! clipped to the cloud's shape, stays on screen, remembers its position.

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows::Win32::Foundation::{HWND, POINT, RECT};
use windows::Win32::Graphics::Gdi::{
    CombineRgn, CreateRectRgn, DeleteObject, GetMonitorInfoW, MonitorFromPoint, MonitorFromWindow, SetWindowRgn,
    HMONITOR, MONITORINFO, MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTONULL, MONITOR_DEFAULTTOPRIMARY, RGN_OR,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON};
use windows::Win32::UI::WindowsAndMessaging::{
    GetWindowLongPtrW, GetWindowRect, SetForegroundWindow, SetWindowLongPtrW, SetWindowPos, GWL_EXSTYLE, HWND_TOPMOST,
    SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, WS_EX_APPWINDOW, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW,
};

use crate::settings::data_file;

/// A rectangle in physical screen pixels.
#[derive(Clone, Copy, PartialEq, Default)]
pub struct Area {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl From<RECT> for Area {
    fn from(r: RECT) -> Self {
        Area { left: r.left, top: r.top, right: r.right, bottom: r.bottom }
    }
}

pub fn hwnd(window: &slint::Window) -> Option<HWND> {
    match window.window_handle().window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(h) => Some(HWND(h.hwnd.get() as *mut _)),
        _ => None,
    }
}

/// Tool window (no taskbar/Alt-Tab entry) that is never activated, so clicking
/// the cloud leaves the focus, and the selection, in the app you were using.
pub fn make_floating(hwnd: HWND) {
    unsafe {
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        let ex = (ex | WS_EX_TOOLWINDOW.0 | WS_EX_NOACTIVATE.0) & !WS_EX_APPWINDOW.0;
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex as isize);
        let flags = SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_FRAMECHANGED;
        let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), 0, 0, 0, 0, flags);
    }
}

pub fn window_rect(hwnd: HWND) -> Area {
    let mut r = RECT::default();
    unsafe {
        let _ = GetWindowRect(hwnd, &mut r);
    }
    r.into()
}

/// Work area (screen minus taskbar) of the monitor showing the window.
pub fn work_area(hwnd: HWND) -> Area {
    unsafe { monitor_work_area(MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST)) }
}

pub fn primary_work_area() -> Area {
    unsafe { monitor_work_area(MonitorFromPoint(POINT::default(), MONITOR_DEFAULTTOPRIMARY)) }
}

unsafe fn monitor_work_area(monitor: HMONITOR) -> Area {
    let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
    let _ = GetMonitorInfoW(monitor, &mut info);
    info.rcWork.into()
}

/// Clips the window to the cloud's shape, given as rectangles in logical pixels
/// (`[left, top, right, bottom]`). Windows only passes clicks through
/// transparent pixels of layered windows, which cannot show OpenGL content, so
/// the region is what lets clicks next to the cloud reach the apps behind it.
pub fn set_region(hwnd: HWND, rects: &[[i32; 4]], scale: f32) {
    let px = |v: i32| (v as f32 * scale).round() as i32;
    unsafe {
        let region = CreateRectRgn(0, 0, 0, 0);
        for &[left, top, right, bottom] in rects {
            let part = CreateRectRgn(px(left), px(top), px(right), px(bottom));
            CombineRgn(Some(region), Some(region), Some(part), RGN_OR);
            let _ = DeleteObject(part.into());
        }
        // On success the system owns the region.
        if SetWindowRgn(hwnd, Some(region), true) == 0 {
            let _ = DeleteObject(region.into());
        }
    }
}

/// Gives the window keyboard focus (the settings window needs typing).
pub fn bring_to_front(hwnd: HWND) {
    unsafe {
        let _ = SetForegroundWindow(hwnd);
    }
}

pub fn left_button_down() -> bool {
    unsafe { GetAsyncKeyState(VK_LBUTTON.0 as i32) as u16 & 0x8000 != 0 }
}

pub fn move_to(hwnd: HWND, x: i32, y: i32) {
    unsafe {
        let _ = SetWindowPos(hwnd, None, x, y, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
    }
}

pub fn save_position(x: i32, y: i32) {
    if let Some(path) = data_file("position.txt") {
        let _ = std::fs::write(path, format!("{x} {y}"));
    }
}

/// The saved position, if it is still on a connected monitor.
pub fn load_position() -> Option<(i32, i32)> {
    let text = std::fs::read_to_string(data_file("position.txt")?).ok()?;
    let mut parts = text.split_whitespace().map(|p| p.parse::<i32>());
    let (x, y) = (parts.next()?.ok()?, parts.next()?.ok()?);
    // A point well inside the cloud, not its transparent corner.
    let probe = POINT { x: x + 120, y: y + 80 };
    let on_screen = unsafe { !MonitorFromPoint(probe, MONITOR_DEFAULTTONULL).is_invalid() };
    on_screen.then_some((x, y))
}
