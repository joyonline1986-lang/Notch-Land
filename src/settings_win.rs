//! Settings in a window of its own: the gear opens it. It is the same page the notch used to show,
//! drawn on a plain dark window with the usual title bar, so it can be moved, minimised and left
//! open next to other programs.
//!
//! Nothing runs while it is not open, and while it is open it only draws when something happens
//! (the pointer moves, a key is pressed, an animation or the caret needs another frame).

use std::time::Instant;

use windows::core::{w, Result, BOOL, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_USE_IMMERSIVE_DARK_MODE};
use windows::Win32::Graphics::Gdi::ValidateRect;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForWindow};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    ReleaseCapture, SetCapture, SetFocus, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::UI::Composition::Compositor;

use crate::bus::Bus;
use crate::config::Config;
use crate::gfx::{Gfx, Painter, Res};
use crate::host::Host;
use crate::modules::{Effects, Key, Module};
use crate::ui::{Input, Ui};
use crate::util::{Color, Rect};

const CLASS: PCWSTR = w!("DynamicNotch.Settings");
/// The size of the page area, in logical pixels (smaller screens get a smaller window).
const WIN_W: f32 = 940.0;
const WIN_H: f32 = 660.0;
/// Room around the page.
const PAD: f32 = 22.0;
const TIMER_ID: usize = 1;
const WM_MOUSELEAVE_: u32 = 0x02A3;
const STYLE: WINDOW_STYLE = WINDOW_STYLE(WS_OVERLAPPED.0 | WS_CAPTION.0 | WS_SYSMENU.0 | WS_MINIMIZEBOX.0);

pub struct SettingsWin {
    hwnd: HWND,
    host: Host,
    res: Res,
    /// Device pixels per logical pixel.
    scale: f32,
    /// The size of the drawing area in logical pixels.
    w: f32,
    h: f32,
    mouse: Option<(f32, f32)>,
    down: Option<(f32, f32)>,
    click: Option<((f32, f32), (f32, f32))>,
    wheel: f32,
    tracking: bool,
    high: Option<u16>,
    keys: Vec<Key>,
    dirty: bool,
    /// The animation timer's interval in milliseconds, 0 when it is off.
    timer: u32,
    closed: bool,
}

impl SettingsWin {
    pub fn new(gfx: &Gfx, compositor: Compositor) -> Result<Self> {
        let hwnd = create_window()?;
        let dpi = unsafe { GetDpiForWindow(hwnd) }.max(96);
        let scale = dpi as f32 / 96.0;
        let (sw, sh) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
        let w = WIN_W.min(sw as f32 / scale - 60.0).max(640.0);
        let h = WIN_H.min(sh as f32 / scale - 120.0).max(480.0);
        let (wpx, hpx) = ((w * scale).round() as i32, (h * scale).round() as i32);
        let mut rc = RECT { left: 0, top: 0, right: wpx, bottom: hpx };
        unsafe {
            let _ = AdjustWindowRectExForDpi(&mut rc, STYLE, false, WS_EX_NOREDIRECTIONBITMAP, dpi);
            let (ow, oh) = (rc.right - rc.left, rc.bottom - rc.top);
            let (x, y) = (((sw - ow) / 2).max(0), ((sh - oh) / 2).max(0));
            let _ = SetWindowPos(hwnd, None, x, y, ow, oh, SWP_NOZORDER | SWP_NOACTIVATE);
        }
        let host = Host::new_shared(hwnd, gfx, wpx, hpx, compositor)?;
        let res = Res::new(gfx)?;
        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOW);
            let _ = SetForegroundWindow(hwnd);
            let _ = SetFocus(Some(hwnd));
        }
        Ok(Self {
            hwnd,
            host,
            res,
            scale,
            w,
            h,
            mouse: None,
            down: None,
            click: None,
            wheel: 0.0,
            tracking: false,
            high: None,
            keys: Vec::new(),
            dirty: true,
            timer: 0,
            closed: false,
        })
    }

    /// Bring the window to the front (it is already open).
    pub fn raise(&mut self) {
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_RESTORE);
            let _ = SetForegroundWindow(self.hwnd);
        }
        self.dirty = true;
    }

    pub fn closed(&self) -> bool {
        self.closed
    }

    pub fn dirty(&self) -> bool {
        self.dirty
    }

    pub fn take_keys(&mut self) -> Vec<Key> {
        std::mem::take(&mut self.keys)
    }

    fn point(&self, lp: LPARAM) -> (f32, f32) {
        let x = (lp.0 & 0xFFFF) as u16 as i16 as f32;
        let y = ((lp.0 >> 16) & 0xFFFF) as u16 as i16 as f32;
        (x / self.scale, y / self.scale)
    }

    /// Take in one window message; the drawing happens afterwards, once, for everything that came in.
    pub fn handle(&mut self, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
        match msg {
            WM_PAINT => {
                unsafe {
                    let _ = ValidateRect(Some(self.hwnd), None);
                }
                self.dirty = true;
                Some(LRESULT(0))
            }
            WM_ERASEBKGND => Some(LRESULT(1)),
            WM_SIZE => {
                let (wpx, hpx) = ((lp.0 & 0xFFFF) as i32, ((lp.0 >> 16) & 0xFFFF) as i32);
                if wpx > 0 && hpx > 0 {
                    let _ = self.host.resize(wpx, hpx);
                    self.w = wpx as f32 / self.scale;
                    self.h = hpx as f32 / self.scale;
                    self.dirty = true;
                }
                Some(LRESULT(0))
            }
            WM_DPICHANGED => {
                let dpi = (wp.0 & 0xFFFF) as u32;
                self.scale = dpi.max(96) as f32 / 96.0;
                let r = unsafe { &*(lp.0 as *const RECT) };
                unsafe {
                    let _ = SetWindowPos(
                        self.hwnd,
                        None,
                        r.left,
                        r.top,
                        r.right - r.left,
                        r.bottom - r.top,
                        SWP_NOZORDER | SWP_NOACTIVATE,
                    );
                }
                self.dirty = true;
                Some(LRESULT(0))
            }
            WM_MOUSEMOVE => {
                if !self.tracking {
                    let mut tme = TRACKMOUSEEVENT {
                        cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE,
                        hwndTrack: self.hwnd,
                        dwHoverTime: 0,
                    };
                    unsafe {
                        let _ = TrackMouseEvent(&mut tme);
                    }
                    self.tracking = true;
                }
                self.mouse = Some(self.point(lp));
                self.dirty = true;
                Some(LRESULT(0))
            }
            WM_MOUSELEAVE_ => {
                self.tracking = false;
                if self.down.is_none() {
                    self.mouse = None;
                }
                self.dirty = true;
                Some(LRESULT(0))
            }
            WM_LBUTTONDOWN => {
                let p = self.point(lp);
                self.mouse = Some(p);
                self.down = Some(p);
                unsafe {
                    SetCapture(self.hwnd);
                }
                self.dirty = true;
                Some(LRESULT(0))
            }
            WM_LBUTTONUP => {
                let p = self.point(lp);
                self.mouse = Some(p);
                unsafe {
                    let _ = ReleaseCapture();
                }
                if let Some(d) = self.down.take() {
                    self.click = Some((d, p));
                }
                self.dirty = true;
                Some(LRESULT(0))
            }
            WM_MOUSEWHEEL => {
                let d = ((wp.0 >> 16) & 0xFFFF) as u16 as i16 as f32;
                self.wheel += d / 120.0;
                self.dirty = true;
                Some(LRESULT(0))
            }
            WM_KEYDOWN | WM_SYSKEYDOWN => {
                if let Some(k) = crate::app::translate_key(wp.0 as u32) {
                    self.keys.push(k);
                    self.dirty = true;
                    return Some(LRESULT(0));
                }
                None
            }
            WM_CHAR => {
                let u = wp.0 as u16;
                let ch = if (0xD800..0xDC00).contains(&u) {
                    self.high = Some(u);
                    None
                } else if (0xDC00..0xE000).contains(&u) {
                    self.high.take().and_then(|hi| char::decode_utf16([hi, u]).next()?.ok())
                } else {
                    char::from_u32(u as u32)
                };
                if let Some(c) = ch {
                    if !c.is_control() {
                        self.keys.push(Key::Char(c));
                        self.dirty = true;
                    }
                }
                Some(LRESULT(0))
            }
            WM_TIMER if wp.0 == TIMER_ID => {
                self.dirty = true;
                Some(LRESULT(0))
            }
            WM_CLOSE => {
                self.closed = true;
                unsafe {
                    let _ = KillTimer(Some(self.hwnd), TIMER_ID);
                    let _ = DestroyWindow(self.hwnd);
                }
                Some(LRESULT(0))
            }
            _ => None,
        }
    }

    /// Draw the page once. `fx` gets what the page asks for (saved values, a reload...).
    pub fn draw(&mut self, gfx: &Gfx, page: &mut dyn Module, cfg: &Config, bus: &Bus, fx: &mut Effects, accent: Color) {
        self.dirty = false;
        let (dc, _) = match self.host.begin_draw() {
            Ok(v) => v,
            Err(e) => {
                crate::log!("settings window: begin_draw failed: {e}");
                return;
            }
        };
        let input = Input {
            mouse: self.mouse,
            down: self.down,
            click: self.click.take(),
            right_click: None,
            wheel: std::mem::take(&mut self.wheel),
            hwheel: 0.0,
        };
        crate::ui::reset_wheel_taken();
        {
            let mut p = Painter::new(dc, gfx, &mut self.res, self.scale, 0.0, 0.0);
            p.clear();
            p.fill_rounded(Rect::new(0.0, 0.0, self.w, self.h), 0.0, Color::rgba(0.075, 0.075, 0.09, 1.0));
            let mut ui = Ui {
                p: &mut p,
                input: &input,
                fx: &mut *fx,
                cfg,
                bus,
                now: Instant::now(),
                accent,
                hot: false,
                interactive: true,
            };
            page.draw_page(&mut ui, Rect::new(PAD, PAD, (self.w - 2.0 * PAD).max(100.0), (self.h - 2.0 * PAD).max(100.0)));
        }
        if let Err(e) = self.host.end_draw() {
            crate::log!("settings window: end_draw failed: {e}");
        }
        // a frame timer only while something is moving (a slider settling, the text caret)
        let want = if fx.animate {
            16
        } else if fx.animate_slow {
            33
        } else {
            0
        };
        if want != self.timer {
            unsafe {
                if want == 0 {
                    let _ = KillTimer(Some(self.hwnd), TIMER_ID);
                } else {
                    SetTimer(Some(self.hwnd), TIMER_ID, want, None);
                }
            }
            self.timer = want;
        }
    }
}

impl Drop for SettingsWin {
    fn drop(&mut self) {
        unsafe {
            let _ = KillTimer(Some(self.hwnd), TIMER_ID);
            if IsWindow(Some(self.hwnd)).as_bool() {
                let _ = DestroyWindow(self.hwnd);
            }
        }
    }
}

fn create_window() -> Result<HWND> {
    unsafe {
        let hinst = GetModuleHandleW(None)?;
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(settings_proc),
            hInstance: hinst.into(),
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            lpszClassName: CLASS,
            ..Default::default()
        };
        RegisterClassExW(&wc);
        let hwnd = CreateWindowExW(
            WS_EX_NOREDIRECTIONBITMAP,
            CLASS,
            w!("DynamicNotch Settings"),
            STYLE,
            0,
            0,
            100,
            100,
            None,
            None,
            Some(hinst.into()),
            None,
        )?;
        let on = BOOL(1);
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, &on as *const _ as _, 4);
        Ok(hwnd)
    }
}

extern "system" fn settings_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let r = std::panic::catch_unwind(|| crate::window::with_app(|a| a.settings_message(msg, wp, lp)).flatten());
    match r {
        Ok(Some(v)) => v,
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}
