//! Win32 details of the cloud window: no taskbar button, never steals focus,
//! clipped to the cloud's shape, stays on screen, remembers its position.

use std::cell::Cell;

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows::core::BOOL;
use windows::Win32::Foundation::{HWND, POINT, RECT};
use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_CLOAK};
use windows::Win32::Graphics::Gdi::{
    CombineRgn, CreateRectRgn, DeleteObject, GetMonitorInfoW, MonitorFromPoint, MonitorFromWindow, SetWindowRgn,
    HMONITOR, MONITORINFO, MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTONULL, MONITOR_DEFAULTTOPRIMARY, RGN_OR,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON};
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, GetWindow, GetWindowLongPtrW, GetWindowRect, GetWindowThreadProcessId, IsWindowVisible,
    SetForegroundWindow, SetWindowLongPtrW, SetWindowPos, GWL_EXSTYLE, GW_HWNDPREV, HWND_TOPMOST, SWP_FRAMECHANGED,
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, WS_EX_APPWINDOW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
};

use crate::settings::data_file;

pub const GL_FALLBACK: &str = "XIAOBAIYUN_GL_FALLBACK";

pub fn wants_direct_composition() -> bool {
    std::env::var_os("SLINT_BACKEND").is_none() && std::env::var_os(GL_FALLBACK).is_none()
}

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

/// Present premultiplied alpha through DirectComposition instead of WGL, whose
/// window alpha is ignored by some Windows 11 drivers (including AMD over RDP).
pub fn init_renderer() -> Result<(), slint::PlatformError> {
    use slint::winit_030::winit::platform::windows::WindowAttributesExtWindows;

    let direct_composition = wants_direct_composition();
    let backend = slint::BackendSelector::new().with_winit_window_attributes_hook(move |attributes| {
        // ShowWindow must not activate while winit rewrites the HUD's styles.
        // Settings request focus explicitly through bring_to_front instead.
        attributes.with_active(false).with_no_redirection_bitmap(direct_composition)
    });
    // Keep explicit renderer overrides for diagnostics and older, GL-only GPUs.
    if std::env::var_os("SLINT_BACKEND").is_some() {
        return backend.select();
    }
    let renderer = if direct_composition {
        // WGPU's default HWND swap chain is opaque. A visual supports premultiplied
        // alpha; no redirection bitmap is needed behind the composition visual.
        // Set these before Slint probes adapters, while startup is single-threaded.
        std::env::set_var("WGPU_BACKEND", "dx12");
        std::env::set_var("WGPU_DX12_PRESENTATION_SYSTEM", "Visual");
        "femtovg-wgpu"
    } else {
        "femtovg"
    };
    backend.backend_name("winit".into()).renderer_name(renderer.into()).select()
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

/// Hides the window from the screen (or shows it again) while it stays open and
/// keeps rendering: DWM just doesn't compose it.
pub fn cloak(hwnd: HWND, cloaked: bool) {
    let value = BOOL::from(cloaked);
    unsafe {
        let size = std::mem::size_of::<BOOL>() as u32;
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_CLOAK, &value as *const BOOL as *const _, size);
    }
}

/// Puts the window back in front when another app's window covers it:
/// full-screen apps such as Remote Desktop are always-on-top too, and cover the
/// cloud whenever they're activated. Tool windows (tooltips, menus) and our own
/// settings window may stay in front of it.
pub fn stay_on_top(hwnd: HWND) {
    let cloud = window_rect(hwnd);
    unsafe {
        let mut above = GetWindow(hwnd, GW_HWNDPREV);
        while let Ok(w) = above {
            let r = window_rect(w);
            let overlaps = r.left < cloud.right && cloud.left < r.right && r.top < cloud.bottom && cloud.top < r.bottom;
            let tool = GetWindowLongPtrW(w, GWL_EXSTYLE) as u32 & WS_EX_TOOLWINDOW.0 != 0;
            if overlaps && !tool && IsWindowVisible(w).as_bool() && window_pid(w) != std::process::id() {
                let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
                return;
            }
            above = GetWindow(w, GW_HWNDPREV);
        }
    }
}

pub unsafe fn window_pid(hwnd: HWND) -> u32 {
    let mut pid = 0;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    pid
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
/// (`[left, top, right, bottom]`). The composition visual supplies per-pixel
/// transparency; the region lets clicks next to the cloud reach the apps behind it.
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

/// Moves a window with the mouse. It follows the cursor's screen position, not
/// offsets measured inside the moving window, so it neither lags nor jitters.
#[derive(Default)]
pub struct Drag {
    /// Cursor and window top-left when the button went down.
    start: Cell<Option<(POINT, POINT)>>,
}

impl Drag {
    pub fn start(&self, hwnd: HWND) {
        let rect = window_rect(hwnd);
        self.start.set(Some((cursor(), POINT { x: rect.left, y: rect.top })));
    }

    pub fn follow(&self, hwnd: HWND) {
        if let Some((cursor0, window0)) = self.start.get() {
            let now = cursor();
            move_to(hwnd, window0.x + now.x - cursor0.x, window0.y + now.y - cursor0.y);
        }
    }
}

fn cursor() -> POINT {
    let mut p = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut p);
    }
    p
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
