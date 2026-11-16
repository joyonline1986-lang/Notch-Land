//! Three-finger swipe on a precision touchpad, read straight from the raw HID reports.
//!
//! Windows only sends these reports to a window that asked for them (usage page 0x0D, usage 0x05) and the
//! reports have no gesture in them, just the fingers' places. So: the fingers are counted and averaged, and
//! a sideways move of three of them is turned into steps of the window switcher, lifting them chooses.
//! Windows' own three-finger gestures must be off (Settings > Bluetooth & devices > Touchpad > Three-finger
//! gestures > Nothing), or Windows swipes as well. Nothing is blocked or recorded.
//!
//! The raw input and HID parser calls are declared by hand here, so nothing depends on how a crate spells them.

use std::ffi::c_void;
use std::mem::size_of;
use std::sync::Mutex;

#[repr(C)]
struct RawDev {
    page: u16,
    usage: u16,
    flags: u32,
    target: isize,
}

#[repr(C)]
struct RawHdr {
    kind: u32,
    size: u32,
    device: isize,
    wparam: usize,
}

#[link(name = "user32")]
extern "system" {
    fn RegisterRawInputDevices(devs: *const RawDev, n: u32, size: u32) -> i32;
    fn GetRawInputData(h: isize, cmd: u32, data: *mut c_void, size: *mut u32, hdr: u32) -> u32;
    fn GetRawInputDeviceInfoW(dev: isize, cmd: u32, data: *mut c_void, size: *mut u32) -> u32;
}

#[link(name = "hid")]
extern "system" {
    fn HidP_GetCaps(pre: isize, caps: *mut u8) -> i32;
    fn HidP_GetValueCaps(kind: i32, caps: *mut u8, len: *mut u16, pre: isize) -> i32;
    fn HidP_GetUsageValue(
        kind: i32,
        page: u16,
        link: u16,
        usage: u16,
        value: *mut u32,
        pre: isize,
        report: *const u8,
        len: u32,
    ) -> i32;
    fn HidP_GetUsages(
        kind: i32,
        page: u16,
        link: u16,
        list: *mut u16,
        len: *mut u32,
        pre: isize,
        report: *const u8,
        rlen: u32,
    ) -> i32;
}

const RID_HEADER: u32 = 0x1000_0005;
const RID_INPUT: u32 = 0x1000_0003;
const RIM_TYPEHID: u32 = 2;
const RIDI_PREPARSEDDATA: u32 = 0x2000_0005;
const RIDEV_INPUTSINK: u32 = 0x100;
/// HidP_Input
const INPUT: i32 = 0;
/// A HIDP_VALUE_CAPS is 72 bytes; the few fields used are read at their offsets.
const VCAP: usize = 72;
const PAGE_DIGITIZER: u16 = 0x0D;
const PAGE_DESKTOP: u16 = 0x01;
const USAGE_X: u16 = 0x30;
const USAGE_Y: u16 = 0x31;
const USAGE_TIP: u16 = 0x42;
const USAGE_COUNT: u16 = 0x54;
/// Fingers that make the gesture.
const FINGERS: u32 = 3;
/// How far (share of the pad's width) the fingers must go for one step, and to decide the direction.
const STEP: f32 = 0.07;
const DECIDE: f32 = 0.03;

/// What one raw input message turned out to be.
pub struct Out {
    /// It was a touchpad report (so the keyboard code must not look at it).
    pub hid: bool,
    /// Steps of the switcher to take (negative: back).
    pub steps: i32,
    /// The fingers were lifted after the switcher was moved: choose.
    pub end: bool,
}

struct Dev {
    handle: isize,
    pre: Vec<u64>,
    /// Link collection of each finger slot.
    links: Vec<u16>,
    count_link: Option<u16>,
    x_span: f32,
    y_span: f32,
}

struct Pad {
    dev: Option<Dev>,
    buf: Vec<u64>,
    active: bool,
    /// 0 not decided yet, 1 sideways (ours), 2 something else (not ours).
    mode: u8,
    sx: f32,
    sy: f32,
    last_x: f32,
    moved: bool,
}

static PAD: Mutex<Pad> =
    Mutex::new(Pad { dev: None, buf: Vec::new(), active: false, mode: 0, sx: 0.0, sy: 0.0, last_x: 0.0, moved: false });

/// Ask Windows for the touchpad reports, also while this window is not in front.
pub fn register(hwnd: isize) -> bool {
    let d = RawDev { page: PAGE_DIGITIZER, usage: 0x05, flags: RIDEV_INPUTSINK, target: hwnd };
    unsafe { RegisterRawInputDevices(&d, 1, size_of::<RawDev>() as u32) != 0 }
}

fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}

fn i32_at(b: &[u8], o: usize) -> i32 {
    i32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

fn bytes(v: &[u64]) -> &[u8] {
    unsafe { std::slice::from_raw_parts(v.as_ptr() as *const u8, v.len() * 8) }
}

/// Read what the pad's reports are made of (once per pad).
fn open_dev(handle: isize) -> Option<Dev> {
    unsafe {
        let mut size = 0u32;
        GetRawInputDeviceInfoW(handle, RIDI_PREPARSEDDATA, std::ptr::null_mut(), &mut size);
        if size == 0 {
            return None;
        }
        let mut pre = vec![0u64; (size as usize).div_ceil(8)];
        if GetRawInputDeviceInfoW(handle, RIDI_PREPARSEDDATA, pre.as_mut_ptr() as *mut c_void, &mut size) == u32::MAX {
            return None;
        }
        let p = pre.as_ptr() as isize;
        let mut caps = [0u8; 64];
        if HidP_GetCaps(p, caps.as_mut_ptr()) < 0 {
            return None;
        }
        let n = u16_at(&caps, 48) as usize;
        if n == 0 {
            return None;
        }
        let mut vc = vec![[0u8; VCAP]; n];
        let mut len = n as u16;
        if HidP_GetValueCaps(INPUT, vc.as_mut_ptr() as *mut u8, &mut len, p) < 0 {
            return None;
        }
        let (mut links, mut count_link) = (Vec::new(), None);
        let (mut x_span, mut y_span) = (0.0f32, 0.0f32);
        for c in vc.iter().take(len as usize) {
            let (page, link, usage) = (u16_at(c, 0), u16_at(c, 6), u16_at(c, 56));
            let span = (i32_at(c, 44) - i32_at(c, 40)).max(0) as f32;
            if page == PAGE_DESKTOP && usage == USAGE_X {
                links.push(link);
                x_span = x_span.max(span);
            } else if page == PAGE_DESKTOP && usage == USAGE_Y {
                y_span = y_span.max(span);
            } else if page == PAGE_DIGITIZER && usage == USAGE_COUNT {
                count_link = Some(link);
            }
        }
        if links.is_empty() || x_span <= 0.0 {
            return None;
        }
        if y_span <= 0.0 {
            y_span = x_span;
        }
        Some(Dev { handle, pre, links, count_link, x_span, y_span })
    }
}

/// One report: the fingers down, their average place, and the number the pad says it has (0: a continuation).
fn read_report(d: &Dev, rep: &[u8]) -> (u32, f32, f32, u32) {
    let p = d.pre.as_ptr() as isize;
    let (mut tips, mut sx, mut sy) = (0u32, 0.0f32, 0.0f32);
    for &l in &d.links {
        let mut list = [0u16; 16];
        let mut len = 16u32;
        let ok = unsafe {
            HidP_GetUsages(INPUT, PAGE_DIGITIZER, l, list.as_mut_ptr(), &mut len, p, rep.as_ptr(), rep.len() as u32)
        };
        if ok < 0 || !list[..(len as usize).min(16)].contains(&USAGE_TIP) {
            continue;
        }
        let (mut x, mut y) = (0u32, 0u32);
        let rx =
            unsafe { HidP_GetUsageValue(INPUT, PAGE_DESKTOP, l, USAGE_X, &mut x, p, rep.as_ptr(), rep.len() as u32) };
        let ry =
            unsafe { HidP_GetUsageValue(INPUT, PAGE_DESKTOP, l, USAGE_Y, &mut y, p, rep.as_ptr(), rep.len() as u32) };
        if rx < 0 || ry < 0 {
            continue;
        }
        tips += 1;
        sx += x as f32;
        sy += y as f32;
    }
    let mut count = tips;
    if let Some(l) = d.count_link {
        let mut c = 0u32;
        let ok = unsafe {
            HidP_GetUsageValue(INPUT, PAGE_DIGITIZER, l, USAGE_COUNT, &mut c, p, rep.as_ptr(), rep.len() as u32)
        };
        if ok >= 0 {
            count = c;
        }
    }
    if tips > 0 {
        sx /= tips as f32;
        sy /= tips as f32;
    }
    (tips, sx, sy, count)
}

impl Pad {
    fn update(&mut self, fingers: u32, x: f32, y: f32, xs: f32, ys: f32, out: &mut Out) {
        if fingers != FINGERS {
            if self.active {
                self.active = false;
                out.end |= self.moved;
            }
            return;
        }
        if !self.active {
            self.active = true;
            self.mode = 0;
            self.sx = x;
            self.sy = y;
            self.last_x = x;
            self.moved = false;
            return;
        }
        if self.mode == 2 {
            return;
        }
        if self.mode == 0 {
            let (dx, dy) = ((x - self.sx) / xs, (y - self.sy) / ys);
            if dx.abs().max(dy.abs()) < DECIDE {
                return;
            }
            self.mode = if dx.abs() > dy.abs() * 1.3 { 1 } else { 2 };
            if self.mode == 2 {
                return;
            }
        }
        let th = xs * STEP;
        let mut d = x - self.last_x;
        while d >= th {
            out.steps += 1;
            self.last_x += th;
            d -= th;
            self.moved = true;
        }
        while d <= -th {
            out.steps -= 1;
            self.last_x -= th;
            d += th;
            self.moved = true;
        }
    }
}

/// Look at a WM_INPUT message (`lp` is its LPARAM). Touchpad reports are used up here (`hid` is set).
pub fn feed(lp: isize) -> Out {
    let mut out = Out { hid: false, steps: 0, end: false };
    let mut hdr = RawHdr { kind: 0, size: 0, device: 0, wparam: 0 };
    let mut hs = size_of::<RawHdr>() as u32;
    let n = unsafe {
        GetRawInputData(lp, RID_HEADER, &mut hdr as *mut RawHdr as *mut c_void, &mut hs, size_of::<RawHdr>() as u32)
    };
    if n == u32::MAX || hdr.kind != RIM_TYPEHID {
        return out;
    }
    out.hid = true;
    let Ok(mut pad) = PAD.lock() else { return out };
    let mut size = hdr.size;
    let words = (size as usize).div_ceil(8).max(8);
    if pad.buf.len() < words {
        pad.buf.resize(words, 0);
    }
    let got = unsafe {
        GetRawInputData(lp, RID_INPUT, pad.buf.as_mut_ptr() as *mut c_void, &mut size, size_of::<RawHdr>() as u32)
    };
    if got == u32::MAX || (got as usize) < size_of::<RawHdr>() + 8 {
        return out;
    }
    if pad.dev.as_ref().map(|d| d.handle) != Some(hdr.device) {
        pad.dev = open_dev(hdr.device);
    }
    let Pad { dev, buf, .. } = &mut *pad;
    let Some(dev) = dev.as_ref() else { return out };
    let raw = &bytes(buf)[..(got as usize).min(buf.len() * 8)];
    let head = size_of::<RawHdr>();
    let (each, count) = (u32_at(raw, head) as usize, u32_at(raw, head + 4) as usize);
    if each == 0 {
        return out;
    }
    let (xs, ys) = (dev.x_span, dev.y_span);
    let mut reports = Vec::new();
    for i in 0..count {
        let a = head + 8 + i * each;
        if a + each > raw.len() {
            break;
        }
        reports.push(read_report(dev, &raw[a..a + each]));
    }
    for (tips, x, y, c) in reports {
        // a continuation (the pad has more fingers than one report holds) says nothing new
        if c == 0 && tips > 0 {
            continue;
        }
        // only act when this report holds every finger
        if tips != c && c != 0 {
            continue;
        }
        pad.update(c, x, y, xs, ys, &mut out);
    }
    out
}
