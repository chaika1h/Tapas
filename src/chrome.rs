//! Custom borderless window chrome with native Win32 frame behavior.
//!
//! On Windows this removes the OS title bar via `WM_NCCALCSIZE` while keeping
//! `WS_OVERLAPPEDWINDOW` and `DwmExtendFrameIntoClientArea` so the window
//! retains native Windows 11 rounded corners, drop shadow, edge resize, Aero
//! Snap, and Snap Layouts (via `HTMAXBUTTON`). Slint renders the caption
//! buttons (`—`, `□/❐`, `×`) in the top-right corner of the client area.
//!
//! On non-Windows platforms this is a no-op and the window uses standard OS
//! decorations.

use crate::MainWindow;

/// Logical height of the draggable top title bar / caption button strip.
pub const TITLEBAR_HEIGHT: i32 = 32;
/// Logical width of each Windows 11 Fluent caption button (`—`, `□`, `×`).
pub const CAPTION_BTN_WIDTH: i32 = 46;
/// Total logical width of the three caption buttons (`3 * 46 = 138`).
pub const CAPTION_STRIP_WIDTH: i32 = CAPTION_BTN_WIDTH * 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HitRegion {
    Client,
    Caption,
    MaxButton,
    Top,
    Bottom,
    Left,
    Right,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

/// Pure hit-test classification in physical client coordinates so it can be
/// verified by unit tests on any OS.
#[allow(clippy::too_many_arguments)]
fn classify_hit(
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    dpi: u32,
    maximized: bool,
    fullscreen: bool,
) -> HitRegion {
    if fullscreen || w <= 0 || h <= 0 {
        return HitRegion::Client;
    }
    let scale = dpi.max(96) as f32 / 96.0;
    let px = |logical: i32| (logical as f32 * scale).round() as i32;

    let title_h = px(TITLEBAR_HEIGHT);
    let strip_w = px(CAPTION_STRIP_WIDTH);
    let btn_w = px(CAPTION_BTN_WIDTH);

    if !maximized {
        let edge = px(6);
        let corner = px(10);
        let in_caption_x = x >= w - strip_w;
        // Keep the top/right resize rim slim over the caption buttons so
        // clicking Close / Maximize / Minimize never feels finicky.
        let top_edge = if in_caption_x { px(2) } else { px(5) };
        let right_edge = if y < title_h { px(3) } else { edge };

        let on_left = x < edge;
        let on_right = x >= w - right_edge;
        let on_top = y < top_edge;
        let on_bottom = y >= h - edge;

        if on_top && (x < corner) {
            return HitRegion::TopLeft;
        }
        if on_top && (x >= w - corner && !in_caption_x) {
            return HitRegion::TopRight;
        }
        if on_bottom && x < corner {
            return HitRegion::BottomLeft;
        }
        if on_bottom && x >= w - corner {
            return HitRegion::BottomRight;
        }
        if on_top {
            return HitRegion::Top;
        }
        if on_bottom {
            return HitRegion::Bottom;
        }
        if on_left {
            return HitRegion::Left;
        }
        if on_right {
            return HitRegion::Right;
        }
    }

    if y >= 0 && y < title_h {
        let max_left = w - btn_w * 2;
        let max_right = w - btn_w;
        if x >= max_left && x < max_right {
            return HitRegion::MaxButton;
        }
        if x >= w - strip_w {
            // Minimize and Close are handled directly by Slint TouchAreas.
            return HitRegion::Client;
        }
        return HitRegion::Caption;
    }

    HitRegion::Client
}

#[cfg(windows)]
mod platform {
    use std::ffi::c_void;
    use std::ptr;
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicPtr, Ordering};

    use slint::ComponentHandle;
    use slint::winit_030::WinitWindowAccessor;
    use slint::winit_030::winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use slint::winit_030::winit::window::Theme as WinitTheme;
    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
    use windows_sys::Win32::Graphics::Dwm::{
        DWMWCP_ROUND, DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWA_WINDOW_CORNER_PREFERENCE,
        DwmExtendFrameIntoClientArea, DwmSetWindowAttribute,
    };
    use windows_sys::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromRect, ScreenToClient,
    };
    use windows_sys::Win32::UI::Controls::MARGINS;
    use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        TME_LEAVE, TME_NONCLIENT, TRACKMOUSEEVENT, TrackMouseEvent,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CallWindowProcW, GWLP_WNDPROC, GetClientRect, HTBOTTOM, HTBOTTOMLEFT, HTBOTTOMRIGHT,
        HTCAPTION, HTCLIENT, HTLEFT, HTMAXBUTTON, HTRIGHT, HTTOP, HTTOPLEFT, HTTOPRIGHT, IsZoomed,
        NCCALCSIZE_PARAMS, PostMessageW, SW_MAXIMIZE, SW_MINIMIZE, SW_RESTORE, SWP_FRAMECHANGED,
        SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SetWindowLongPtrW, SetWindowPos,
        ShowWindow, WA_INACTIVE, WM_ACTIVATE, WM_CAPTURECHANGED, WM_CLOSE, WM_MOUSEMOVE,
        WM_NCACTIVATE, WM_NCCALCSIZE, WM_NCHITTEST, WM_NCLBUTTONDOWN, WM_NCLBUTTONUP,
        WM_NCMOUSELEAVE, WM_NCMOUSEMOVE, WM_SIZE, WNDPROC,
    };

    use super::{HitRegion, classify_hit};
    use crate::{Bridge, MainWindow};

    static WINDOW_HWND: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());
    static PREV_WNDPROC: AtomicIsize = AtomicIsize::new(0);
    static FULLSCREEN: AtomicBool = AtomicBool::new(false);
    static MAXIMIZED: AtomicBool = AtomicBool::new(false);
    static ACTIVE: AtomicBool = AtomicBool::new(true);
    static MAX_HOVERED: AtomicBool = AtomicBool::new(false);
    static MAX_PRESSED: AtomicBool = AtomicBool::new(false);
    static SYSTEM_DARK: AtomicBool = AtomicBool::new(true);
    static UI_WEAK: OnceLock<slint::Weak<MainWindow>> = OnceLock::new();

    fn hwnd() -> HWND {
        WINDOW_HWND.load(Ordering::Acquire)
    }

    fn sync_ui() {
        let Some(weak) = UI_WEAK.get().cloned() else {
            return;
        };
        let maximized = MAXIMIZED.load(Ordering::Relaxed);
        let active = ACTIVE.load(Ordering::Relaxed);
        let hovered = MAX_HOVERED.load(Ordering::Relaxed);
        let pressed = MAX_PRESSED.load(Ordering::Relaxed);
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                let b = ui.global::<Bridge>();
                if b.get_window_maximized() != maximized {
                    b.set_window_maximized(maximized);
                }
                if b.get_window_active() != active {
                    b.set_window_active(active);
                }
                if b.get_max_btn_hovered() != hovered {
                    b.set_max_btn_hovered(hovered);
                }
                if b.get_max_btn_pressed() != pressed {
                    b.set_max_btn_pressed(pressed);
                }
            }
        });
    }

    fn set_max_hover(hovered: bool, pressed: Option<bool>) {
        let mut changed = MAX_HOVERED.swap(hovered, Ordering::Relaxed) != hovered;
        if let Some(p) = pressed {
            if MAX_PRESSED.swap(p, Ordering::Relaxed) != p {
                changed = true;
            }
        }
        if changed {
            sync_ui();
        }
    }

    fn apply_dark_frame(hwnd: HWND, dark: bool) {
        let value: i32 = i32::from(dark);
        unsafe {
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWA_USE_IMMERSIVE_DARK_MODE as u32,
                &value as *const i32 as *const c_void,
                std::mem::size_of::<i32>() as u32,
            );
        }
    }

    pub fn install(ui: &MainWindow, theme_index: i32) {
        let _ = UI_WEAK.set(ui.as_weak());
        ui.global::<Bridge>().set_custom_chrome(true);

        let weak = ui.as_weak();
        let _ = slint::spawn_local(async move {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let Ok(win) = ui.window().winit_window().await else {
                ui.global::<Bridge>().set_custom_chrome(false);
                return;
            };
            let sys_dark = !matches!(win.theme(), Some(WinitTheme::Light));
            SYSTEM_DARK.store(sys_dark, Ordering::Relaxed);

            let raw_hwnd = win
                .window_handle()
                .ok()
                .and_then(|h| match h.as_raw() {
                    RawWindowHandle::Win32(w) => Some(w.hwnd.get() as HWND),
                    _ => None,
                });
            // Drop the Arc<winit::window::Window> immediately so closing the
            // Slint window isn't kept alive by an extra reference.
            drop(win);

            let Some(h) = raw_hwnd else {
                ui.global::<Bridge>().set_custom_chrome(false);
                return;
            };

            WINDOW_HWND.store(h, Ordering::Release);
            unsafe {
                let prev = SetWindowLongPtrW(h, GWLP_WNDPROC, wnd_proc as *const () as isize);
                PREV_WNDPROC.store(prev, Ordering::Release);

                // Extending 1 px into the top keeps Windows 11's native drop
                // shadow, crisp 1 px outer border, and rounded corners without
                // turning the client surface transparent under OpenGL.
                let margins = MARGINS {
                    cxLeftWidth: 0,
                    cxRightWidth: 0,
                    cyTopHeight: 1,
                    cyBottomHeight: 0,
                };
                let _ = DwmExtendFrameIntoClientArea(h, &margins);

                let corner = DWMWCP_ROUND;
                let _ = DwmSetWindowAttribute(
                    h,
                    DWMWA_WINDOW_CORNER_PREFERENCE as u32,
                    &corner as *const _ as *const c_void,
                    std::mem::size_of_val(&corner) as u32,
                );
            }

            set_theme(theme_index);
            MAXIMIZED.store(unsafe { IsZoomed(h) != 0 }, Ordering::Relaxed);
            refresh_frame();
            sync_ui();
        });
    }

    pub fn set_theme(theme_index: i32) {
        let h = hwnd();
        if h.is_null() {
            return;
        }
        let dark = match theme_index {
            1 => false,
            2 => true,
            _ => SYSTEM_DARK.load(Ordering::Relaxed),
        };
        apply_dark_frame(h, dark);
    }

    pub fn set_fullscreen(on: bool) {
        FULLSCREEN.store(on, Ordering::Release);
        if on {
            set_max_hover(false, Some(false));
        }
    }

    pub fn refresh_frame() {
        let h = hwnd();
        if h.is_null() {
            return;
        }
        unsafe {
            let _ = SetWindowPos(
                h,
                ptr::null_mut(),
                0,
                0,
                0,
                0,
                SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
            );
            let zoomed = IsZoomed(h) != 0;
            if MAXIMIZED.swap(zoomed, Ordering::Relaxed) != zoomed {
                sync_ui();
            }
        }
    }

    pub fn minimize() {
        let h = hwnd();
        if !h.is_null() {
            unsafe {
                ShowWindow(h, SW_MINIMIZE);
            }
        }
    }

    pub fn toggle_maximize() {
        let h = hwnd();
        if !h.is_null() {
            unsafe {
                let cmd = if IsZoomed(h) != 0 { SW_RESTORE } else { SW_MAXIMIZE };
                ShowWindow(h, cmd);
            }
        }
    }

    pub fn close() {
        let h = hwnd();
        if !h.is_null() {
            unsafe {
                let _ = PostMessageW(h, WM_CLOSE, 0, 0);
            }
        }
    }

    unsafe extern "system" fn wnd_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        let call_prev = || unsafe {
            let prev = PREV_WNDPROC.load(Ordering::Acquire);
            if prev != 0 {
                let f: WNDPROC = std::mem::transmute(prev);
                CallWindowProcW(f, hwnd, msg, wparam, lparam)
            } else {
                0
            }
        };

        match msg {
            WM_NCCALCSIZE => {
                if wparam == 0 {
                    return 0;
                }
                if FULLSCREEN.load(Ordering::Acquire) {
                    return 0;
                }
                if unsafe { IsZoomed(hwnd) != 0 } {
                    // Clip the off-screen OS resize border when maximized so no
                    // UI pixels spill past the monitor work area.
                    let params = unsafe { &mut *(lparam as *mut NCCALCSIZE_PARAMS) };
                    let monitor = unsafe {
                        MonitorFromRect(&params.rgrc[0], MONITOR_DEFAULTTONEAREST)
                    };
                    let mut info = MONITORINFO {
                        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                        rcMonitor: RECT { left: 0, top: 0, right: 0, bottom: 0 },
                        rcWork: RECT { left: 0, top: 0, right: 0, bottom: 0 },
                        dwFlags: 0,
                    };
                    if unsafe { GetMonitorInfoW(monitor, &mut info) != 0 } {
                        let mut work = info.rcWork;
                        // Leave 1 px at the bottom when an auto-hide taskbar
                        // makes rcWork equal rcMonitor so hovering reveals it.
                        if work.left == info.rcMonitor.left
                            && work.top == info.rcMonitor.top
                            && work.right == info.rcMonitor.right
                            && work.bottom == info.rcMonitor.bottom
                        {
                            work.bottom -= 1;
                        }
                        params.rgrc[0] = work;
                    }
                }
                return 0;
            }

            WM_NCHITTEST => {
                let screen_x = (lparam as u32 & 0xFFFF) as i16 as i32;
                let screen_y = ((lparam as u32 >> 16) & 0xFFFF) as i16 as i32;
                let mut pt = POINT { x: screen_x, y: screen_y };
                let mut rc = RECT { left: 0, top: 0, right: 0, bottom: 0 };
                unsafe {
                    let _ = ScreenToClient(hwnd, &mut pt);
                    let _ = GetClientRect(hwnd, &mut rc);
                }
                let dpi = unsafe { GetDpiForWindow(hwnd) };
                let maximized = unsafe { IsZoomed(hwnd) != 0 };
                let fullscreen = FULLSCREEN.load(Ordering::Acquire);

                return match classify_hit(pt.x, pt.y, rc.right, rc.bottom, dpi, maximized, fullscreen) {
                    HitRegion::Client => HTCLIENT as LRESULT,
                    HitRegion::Caption => HTCAPTION as LRESULT,
                    HitRegion::MaxButton => HTMAXBUTTON as LRESULT,
                    HitRegion::Top => HTTOP as LRESULT,
                    HitRegion::Bottom => HTBOTTOM as LRESULT,
                    HitRegion::Left => HTLEFT as LRESULT,
                    HitRegion::Right => HTRIGHT as LRESULT,
                    HitRegion::TopLeft => HTTOPLEFT as LRESULT,
                    HitRegion::TopRight => HTTOPRIGHT as LRESULT,
                    HitRegion::BottomLeft => HTBOTTOMLEFT as LRESULT,
                    HitRegion::BottomRight => HTBOTTOMRIGHT as LRESULT,
                };
            }

            WM_NCMOUSEMOVE => {
                if wparam as u32 == HTMAXBUTTON {
                    if !MAX_HOVERED.load(Ordering::Relaxed) {
                        let mut tme = TRACKMOUSEEVENT {
                            cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                            dwFlags: TME_LEAVE | TME_NONCLIENT,
                            hwndTrack: hwnd,
                            dwHoverTime: 0,
                        };
                        unsafe {
                            let _ = TrackMouseEvent(&mut tme);
                        }
                    }
                    set_max_hover(true, None);
                } else {
                    set_max_hover(false, Some(false));
                }
            }

            WM_NCMOUSELEAVE | WM_MOUSEMOVE | WM_CAPTURECHANGED => {
                set_max_hover(false, Some(false));
            }

            WM_NCLBUTTONDOWN => {
                if wparam as u32 == HTMAXBUTTON {
                    // Swallow so DefWindowProc doesn't paint a legacy button
                    // artifact over the OpenGL surface while Snap Layouts still
                    // hooks HTMAXBUTTON on hover.
                    set_max_hover(true, Some(true));
                    return 0;
                }
            }

            WM_NCLBUTTONUP => {
                if wparam as u32 == HTMAXBUTTON {
                    let was_pressed = MAX_PRESSED.swap(false, Ordering::Relaxed);
                    sync_ui();
                    if was_pressed {
                        toggle_maximize();
                    }
                    return 0;
                }
            }

            WM_SIZE => {
                let zoomed = unsafe { IsZoomed(hwnd) != 0 };
                if MAXIMIZED.swap(zoomed, Ordering::Relaxed) != zoomed {
                    sync_ui();
                }
            }

            WM_ACTIVATE | WM_NCACTIVATE => {
                let active = (wparam & 0xFFFF) != WA_INACTIVE as usize;
                if ACTIVE.swap(active, Ordering::Relaxed) != active {
                    sync_ui();
                }
            }

            _ => {}
        }

        call_prev()
    }
}

#[cfg(not(windows))]
mod platform {
    use crate::MainWindow;

    pub fn install(_ui: &MainWindow, _theme_index: i32) {}
    pub fn set_theme(_theme_index: i32) {}
    pub fn set_fullscreen(_on: bool) {}
    pub fn refresh_frame() {}
    pub fn minimize() {}
    pub fn toggle_maximize() {}
    pub fn close() {}
}

pub fn install(ui: &MainWindow, theme_index: i32) {
    platform::install(ui, theme_index);
}

pub fn set_theme(theme_index: i32) {
    platform::set_theme(theme_index);
}

pub fn set_fullscreen(on: bool) {
    platform::set_fullscreen(on);
}

pub fn refresh_frame() {
    platform::refresh_frame();
}

pub fn minimize() {
    platform::minimize();
}

pub fn toggle_maximize() {
    platform::toggle_maximize();
}

pub fn close() {
    platform::close();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hit_test_classifies_caption_buttons_and_edges() {
        let (w, h, dpi) = (1280, 800, 96);
        // Drag strip across sidebar and main content
        assert_eq!(classify_hit(100, 16, w, h, dpi, false, false), HitRegion::Caption);
        assert_eq!(classify_hit(900, 16, w, h, dpi, false, false), HitRegion::Caption);
        // Caption buttons at top-right: Minimize, Maximize (HTMAXBUTTON), Close
        assert_eq!(classify_hit(w - 115, 16, w, h, dpi, false, false), HitRegion::Client);
        assert_eq!(classify_hit(w - 70, 16, w, h, dpi, false, false), HitRegion::MaxButton);
        assert_eq!(classify_hit(w - 20, 16, w, h, dpi, false, false), HitRegion::Client);
        // Resize edges and corners when restored
        assert_eq!(classify_hit(2, 2, w, h, dpi, false, false), HitRegion::TopLeft);
        assert_eq!(classify_hit(400, 2, w, h, dpi, false, false), HitRegion::Top);
        assert_eq!(classify_hit(2, 400, w, h, dpi, false, false), HitRegion::Left);
        assert_eq!(classify_hit(w - 2, 400, w, h, dpi, false, false), HitRegion::Right);
        assert_eq!(classify_hit(w - 4, h - 4, w, h, dpi, false, false), HitRegion::BottomRight);
        // Maximized disables resize borders so Fitts's law hits Close at (w-1, 0)
        assert_eq!(classify_hit(w - 1, 0, w, h, dpi, true, false), HitRegion::Client);
        assert_eq!(classify_hit(w - 70, 0, w, h, dpi, true, false), HitRegion::MaxButton);
        assert_eq!(classify_hit(100, 0, w, h, dpi, true, false), HitRegion::Caption);
        // Fullscreen always passes through to client
        assert_eq!(classify_hit(100, 16, w, h, dpi, false, true), HitRegion::Client);
        assert_eq!(classify_hit(w - 70, 16, w, h, dpi, false, true), HitRegion::Client);
    }

    #[test]
    fn hit_test_scales_with_dpi() {
        // At 150% DPI (144), 32 logical px = 48 physical px, 46 logical px = 69 px
        let (w, h, dpi) = (1920, 1080, 144);
        assert_eq!(classify_hit(500, 40, w, h, dpi, false, false), HitRegion::Caption);
        assert_eq!(classify_hit(500, 52, w, h, dpi, false, false), HitRegion::Client);
        assert_eq!(classify_hit(w - 100, 24, w, h, dpi, false, false), HitRegion::MaxButton);
    }
}
