//! File shelf: drag files onto the notch to park them, drag them back out
//! into any folder or app later.
//!
//! Parked items are references (paths), never copies. Dragging one out is a
//! normal shell drag, so Explorer moves within a drive and copies across
//! drives exactly like it does for its own files; items whose file is gone
//! afterwards drop off the shelf. With `remember` the paths survive restarts.

use std::any::Any;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::{CardSize, Cx, Module, ModuleId, Peek, SystemEvent};
use crate::bus::Bus;
use crate::gfx::{Icon, ImageData, TextStyle};
use crate::sys::{self, dragdrop};
use crate::ui::{ButtonStyle, Ui};
use crate::util::{app_dir, palette, Color, Rect};

const ID: ModuleId = "shelf";
const MAX_ITEMS: usize = 24;
const TILE_W: f32 = 84.0;
const TILE_H: f32 = 86.0;
const GAP: f32 = 8.0;
const THUMB: f32 = 42.0;
/// Pointer travel (DIPs) that turns a press into a drag.
const DRAG_SLOP: f32 = 5.0;

struct Item {
    path: String,
    name: String,
    thumb: Option<ImageData>,
    /// When it was dropped on the shelf (it pops in for a moment), not set for saved items.
    born: Option<Instant>,
}

struct Thumb(String, Option<ImageData>);

struct Converted(Result<String, String>);

/// A file was copied to Downloads or the Desktop: (where, its name), or what went wrong.
struct Copied(Result<(String, String), String>);

/// The converter's key (a free FreeConvert account), kept like the Gemini key.
const KEY_VAR: &str = "FREECONVERT_API_KEY";
/// Bigger files are not sent.
const MAX_UPLOAD: usize = 50 * 1024 * 1024;
const API: &str = "https://api.freeconvert.com/v1/process";

/// The pointer while files are dragged over the notch (window DIPs as two f32 in one u64; MAX = none),
/// and the files being dragged.
static DRAG_PT: AtomicU64 = AtomicU64::new(u64::MAX);
static DRAG_FILES: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Files came over the notch.
pub fn drag_enter(files: Vec<String>) {
    if let Ok(mut g) = DRAG_FILES.lock() {
        *g = files;
    }
    DRAG_PT.store(u64::MAX, Ordering::Relaxed);
}

/// The pointer moved (window DIPs) while files are over the notch.
pub fn drag_pointer(x: f32, y: f32) {
    DRAG_PT.store((x.to_bits() as u64) << 32 | y.to_bits() as u64, Ordering::Relaxed);
}

fn clear_pointer() {
    DRAG_PT.store(u64::MAX, Ordering::Relaxed);
}

fn pointer() -> Option<(f32, f32)> {
    let v = DRAG_PT.load(Ordering::Relaxed);
    (v != u64::MAX).then(|| (f32::from_bits((v >> 32) as u32), f32::from_bits(v as u32)))
}

/// The formats a file can be turned into (the list that opens over the Converter).
fn formats_for(path: &str) -> &'static [&'static str] {
    let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "png" | "jpg" | "jpeg" | "webp" | "bmp" | "gif" | "tif" | "tiff" | "ico" | "heic" | "svg" | "avif" => {
            &["png", "jpg", "webp", "ico", "bmp", "gif", "tiff", "pdf"]
        }
        "mp4" | "mov" | "mkv" | "avi" | "webm" | "m4v" | "wmv" | "flv" => {
            &["mp4", "webm", "gif", "mp3", "mov", "mkv", "avi", "wav"]
        }
        "mp3" | "wav" | "flac" | "ogg" | "m4a" | "aac" | "opus" | "wma" => {
            &["mp3", "wav", "flac", "ogg", "m4a", "aac", "opus", "wma"]
        }
        "pdf" | "doc" | "docx" | "odt" | "rtf" | "txt" | "html" | "htm" | "md" | "xlsx" | "xls" | "csv" | "pptx"
        | "ppt" => &["pdf", "docx", "txt", "html", "xlsx", "odt", "png", "jpg"],
        _ => &["pdf", "png", "jpg", "mp4", "mp3", "zip", "txt", "docx"],
    }
}

fn same_format(a: &str, b: &str) -> bool {
    fn norm(s: &str) -> &str {
        match s {
            "jpeg" => "jpg",
            "tif" => "tiff",
            o => o,
        }
    }
    norm(a) == norm(b)
}

fn ext_of(path: &str) -> String {
    path.rsplit('.').next().unwrap_or("").to_ascii_lowercase()
}

fn api_error(status: u16, v: &serde_json::Value) -> String {
    match status {
        401 | 403 => "That key doesn't work".to_string(),
        402 | 429 => "Out of free minutes for today".to_string(),
        _ => v["message"]
            .as_str()
            .or_else(|| v["error"]["message"].as_str())
            .or_else(|| v["error"].as_str())
            .map(|m| m.chars().take(60).collect::<String>())
            .unwrap_or_else(|| format!("FreeConvert said {status}")),
    }
}

/// The tasks in a job's answer, whether they come as a list or as an object by name.
fn job_tasks(v: &serde_json::Value) -> Vec<serde_json::Value> {
    let t = if v["tasks"].is_null() { &v["data"]["tasks"] } else { &v["tasks"] };
    match t {
        serde_json::Value::Array(a) => a.clone(),
        serde_json::Value::Object(o) => o.values().cloned().collect(),
        _ => Vec::new(),
    }
}

fn task_by_op(tasks: &[serde_json::Value], op: &str) -> Option<serde_json::Value> {
    tasks.iter().find(|t| t["operation"] == op).cloned()
}

fn job_id(v: &serde_json::Value) -> Option<String> {
    v["id"].as_str().or_else(|| v["_id"].as_str()).or_else(|| v["data"]["id"].as_str()).map(str::to_string)
}

/// Where an export task says the finished file can be fetched.
fn result_url(t: &serde_json::Value) -> Option<String> {
    let r = &t["result"];
    r["url"].as_str().or_else(|| r["files"][0]["url"].as_str()).or_else(|| r["urls"][0].as_str()).map(str::to_string)
}

/// A multipart/form-data body: the extra fields first, then the file.
fn multipart(boundary: &str, fields: &[(String, String)], name: &str, bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + 1024);
    for (k, v) in fields {
        out.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{k}\"\r\n\r\n{v}\r\n").as_bytes());
    }
    let name = name.replace('"', "'");
    out.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{name}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
        )
        .as_bytes(),
    );
    out.extend_from_slice(bytes);
    out.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    out
}

/// FreeConvert (a free account gives some conversion minutes a day): a job with three tasks
/// (upload, convert, link), the file is sent to the address the job gives, the job is checked
/// until the link is there, and the result is saved in the Downloads folder.
fn cloud_convert(key: &str, path: &str, fmt: &str) -> Result<String, String> {
    use crate::sys::http::{self, Timeouts};
    let cancel = AtomicBool::new(false);
    let bytes = std::fs::read(path).map_err(|_| "Can't read that file".to_string())?;
    if bytes.is_empty() {
        return Err("That file is empty".to_string());
    }
    if bytes.len() > MAX_UPLOAD {
        return Err("Files over 50 MB are too big".to_string());
    }
    let auth = format!("Bearer {key}");
    let json_head = [
        ("Authorization", auth.clone()),
        ("Content-Type", "application/json".to_string()),
        ("Accept", "application/json".to_string()),
    ];
    let get_head = [("Authorization", auth.clone()), ("Accept", "application/json".to_string())];
    let mut cv = serde_json::json!({ "operation": "convert", "input": "up", "output_format": fmt });
    let have = ext_of(path);
    if !have.is_empty() {
        cv["input_format"] = serde_json::json!(if have == "jpeg" { "jpg" } else { have.as_str() });
    }
    let job = serde_json::json!({
        "tasks": {
            "up": { "operation": "import/upload" },
            "cv": cv,
            "dl": { "operation": "export/url", "input": ["cv"] }
        }
    })
    .to_string();
    let t = Timeouts(10_000, 15_000, 30_000, 60_000);
    let (st, body) = http::fetch("POST", &format!("{API}/jobs"), &json_head, job.as_bytes(), t, 1 << 20, &cancel)?;
    let mut v: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
    if !(200..300).contains(&st) {
        return Err(api_error(st, &v));
    }
    let id = job_id(&v).ok_or_else(|| "FreeConvert gave no job".to_string())?;
    let job_url = format!("{API}/jobs/{id}");
    // where the file goes: in the answer to the job, or a moment later
    let mut form = None;
    for i in 0..12 {
        if let Some(up) = task_by_op(&job_tasks(&v), "import/upload") {
            if let Some(u) = up["result"]["form"]["url"].as_str() {
                form = Some((u.to_string(), up["result"]["form"]["parameters"].clone()));
                break;
            }
        }
        if i == 11 {
            break;
        }
        std::thread::sleep(Duration::from_millis(1000));
        let (st, body) = http::fetch("GET", &job_url, &get_head, &[], Timeouts::DEFAULT, 1 << 20, &cancel)?;
        v = serde_json::from_slice(&body).unwrap_or_default();
        if !(200..300).contains(&st) {
            return Err(api_error(st, &v));
        }
    }
    let (form_url, params) = form.ok_or_else(|| "FreeConvert wouldn't take the file".to_string())?;
    let mut fields: Vec<(String, String)> = Vec::new();
    if let Some(o) = params.as_object() {
        for (k, val) in o {
            if let Some(s) = val.as_str() {
                fields.push((k.clone(), s.to_string()));
            }
        }
    }
    let boundary = "----notchfc4a91d7e2";
    let body = multipart(boundary, &fields, &file_name(path), &bytes);
    drop(bytes);
    let up_head = [("Authorization", auth), ("Content-Type", format!("multipart/form-data; boundary={boundary}"))];
    let t = Timeouts(10_000, 15_000, 120_000, 60_000);
    let (st, resp) = http::fetch("POST", &form_url, &up_head, &body, t, 1 << 16, &cancel)?;
    drop(body);
    if !(200..300).contains(&st) {
        let rv: serde_json::Value = serde_json::from_slice(&resp).unwrap_or_default();
        return Err(api_error(st, &rv));
    }
    for _ in 0..90 {
        std::thread::sleep(Duration::from_millis(1500));
        let (st, body) = http::fetch("GET", &job_url, &get_head, &[], Timeouts::DEFAULT, 1 << 20, &cancel)?;
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
        if !(200..300).contains(&st) {
            return Err(api_error(st, &v));
        }
        let tasks = job_tasks(&v);
        if let Some(link) = task_by_op(&tasks, "export/url").and_then(|t| result_url(&t)) {
            let t = Timeouts(10_000, 15_000, 60_000, 120_000);
            let (st, data) = http::fetch("GET", &link, &[], &[], t, 80 << 20, &cancel)?;
            if st != 200 || data.is_empty() {
                return Err("Couldn't download the result".to_string());
            }
            let dir = super::downloads::downloads_folder().unwrap_or_else(std::env::temp_dir);
            let stem = Path::new(path).file_stem().and_then(|s| s.to_str()).unwrap_or("converted");
            let mut out = dir.join(format!("{stem}.{fmt}"));
            let mut n = 1;
            while out.exists() {
                out = dir.join(format!("{stem} ({n}).{fmt}"));
                n += 1;
            }
            std::fs::write(&out, &data).map_err(|_| "Couldn't save the result".to_string())?;
            return Ok(out.to_string_lossy().into_owned());
        }
        if v["status"] == "failed" || tasks.iter().any(|t| t["status"] == "failed") {
            let msg = tasks
                .iter()
                .find(|t| t["status"] == "failed")
                .and_then(|t| t["message"].as_str().or_else(|| t["msg"].as_str()))
                .map(|m| m.chars().take(60).collect::<String>())
                .unwrap_or_else(|| "The conversion failed".to_string());
            return Err(msg);
        }
    }
    Err("It took too long".to_string())
}

/// Pictures this PC converts by itself with Windows' own picture codecs: no key, no upload.
fn can_local(path: &str, fmt: &str) -> bool {
    matches!(ext_of(path).as_str(), "png" | "jpg" | "jpeg" | "bmp" | "gif" | "tif" | "tiff" | "ico" | "webp")
        && matches!(fmt, "png" | "jpg" | "jpeg" | "bmp" | "tif" | "tiff" | "ico")
}

/// A name for the converted file in Downloads that is not taken yet.
fn local_out_path(path: &str, fmt: &str) -> std::path::PathBuf {
    let dir = super::downloads::downloads_folder().unwrap_or_else(std::env::temp_dir);
    let stem = Path::new(path).file_stem().and_then(|s| s.to_str()).unwrap_or("converted");
    let mut out = dir.join(format!("{stem}.{fmt}"));
    let mut n = 1;
    while out.exists() {
        out = dir.join(format!("{stem} ({n}).{fmt}"));
        n += 1;
    }
    out
}

/// Convert a picture on this PC; the result is saved in Downloads. COM must be started on this thread.
fn local_convert(path: &str, fmt: &str) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|_| "Can't read that file".to_string())?;
    if bytes.is_empty() {
        return Err("That file is empty".to_string());
    }
    let out = local_out_path(path, fmt);
    match unsafe { wic_convert(&bytes, &out, fmt) } {
        Ok(()) => Ok(out.to_string_lossy().into_owned()),
        Err(e) => {
            let _ = std::fs::remove_file(&out);
            crate::log!("convert {path} to {fmt}: {e}");
            Err("This picture type isn't supported".to_string())
        }
    }
}

/// Decode a picture and write it again in another format (transparent areas become white for JPG and BMP).
unsafe fn wic_convert(bytes: &[u8], out: &Path, fmt: &str) -> windows::core::Result<()> {
    use windows::core::PCWSTR;
    use windows::Win32::Graphics::Imaging::*;
    use windows::Win32::System::Com::StructuredStorage::IPropertyBag2;
    use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
    let fail = || windows::core::Error::from_hresult(windows::Win32::Foundation::E_FAIL);
    let (container, want) = match fmt {
        "png" => (GUID_ContainerFormatPng, GUID_WICPixelFormat32bppBGRA),
        "jpg" | "jpeg" => (GUID_ContainerFormatJpeg, GUID_WICPixelFormat24bppBGR),
        "bmp" => (GUID_ContainerFormatBmp, GUID_WICPixelFormat24bppBGR),
        "tif" | "tiff" => (GUID_ContainerFormatTiff, GUID_WICPixelFormat32bppBGRA),
        "ico" => (GUID_ContainerFormatIco, GUID_WICPixelFormat32bppBGRA),
        _ => return Err(fail()),
    };
    let wic: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
    let stream = wic.CreateStream()?;
    stream.InitializeFromMemory(bytes)?;
    let decoder = wic.CreateDecoderFromStream(&stream, std::ptr::null(), WICDecodeMetadataCacheOnDemand)?;
    let frame = decoder.GetFrame(0)?;
    let (mut w, mut h) = (0u32, 0u32);
    frame.GetSize(&mut w, &mut h)?;
    if w == 0 || h == 0 || (w as u64) * (h as u64) > 64_000_000 {
        return Err(fail());
    }
    let mut source: IWICBitmapSource = windows::core::Interface::cast(&frame)?;
    // an icon is at most 256 pixels on a side
    if fmt == "ico" && (w > 256 || h > 256) {
        let k = 256.0 / w.max(h) as f32;
        let (nw, nh) = (((w as f32 * k).round() as u32).max(1), ((h as f32 * k).round() as u32).max(1));
        let scaler = wic.CreateBitmapScaler()?;
        scaler.Initialize(&source, nw, nh, WICBitmapInterpolationModeHighQualityCubic)?;
        source = windows::core::Interface::cast(&scaler)?;
        w = nw;
        h = nh;
    }
    let conv = wic.CreateFormatConverter()?;
    conv.Initialize(&source, &GUID_WICPixelFormat32bppBGRA, WICBitmapDitherTypeNone, None, 0.0, WICBitmapPaletteTypeCustom)?;
    let mut px = vec![0u8; w as usize * h as usize * 4];
    conv.CopyPixels(std::ptr::null(), w * 4, &mut px)?;
    let (data, stride) = if want == GUID_WICPixelFormat24bppBGR {
        let mut rgb = Vec::with_capacity(w as usize * h as usize * 3);
        for p in px.chunks_exact(4) {
            let a = p[3] as u32;
            for c in &p[..3] {
                rgb.push(((*c as u32 * a + 255 * (255 - a)) / 255) as u8);
            }
        }
        (rgb, w * 3)
    } else {
        (px, w * 4)
    };
    let wide: Vec<u16> = out.to_string_lossy().encode_utf16().chain(Some(0)).collect();
    let file = wic.CreateStream()?;
    file.InitializeFromFilename(PCWSTR(wide.as_ptr()), 0x4000_0000)?;
    let enc = wic.CreateEncoder(&container, std::ptr::null())?;
    enc.Initialize(&file, WICBitmapEncoderNoCache)?;
    let mut fr: Option<IWICBitmapFrameEncode> = None;
    let mut opts: Option<IPropertyBag2> = None;
    enc.CreateNewFrame(&mut fr, &mut opts)?;
    let fr = fr.ok_or_else(fail)?;
    fr.Initialize(opts.as_ref())?;
    fr.SetSize(w, h)?;
    let mut got = want;
    fr.SetPixelFormat(&mut got)?;
    if got != want {
        return Err(fail());
    }
    fr.WritePixels(h, stride, &data)?;
    fr.Commit()?;
    enc.Commit()?;
    Ok(())
}

/// A short form of a message for a chip.
/// Copy one file into `dir` (a number is added to the name when it is taken); gives the new name.
fn copy_one(src: &Path, dir: &Path) -> Result<String, String> {
    if !src.is_file() {
        return Err("Only files are copied".to_string());
    }
    if !dir.is_dir() {
        return Err("Can't find that folder".to_string());
    }
    let stem = src.file_stem().and_then(|s| s.to_str()).unwrap_or("file");
    let ext = src.extension().and_then(|s| s.to_str()).map(|e| format!(".{e}")).unwrap_or_default();
    let mut out = dir.join(format!("{stem}{ext}"));
    let mut n = 1;
    while out.exists() {
        out = dir.join(format!("{stem} ({n}){ext}"));
        n += 1;
    }
    std::fs::copy(src, &out).map_err(|e| e.to_string().chars().take(60).collect::<String>())?;
    Ok(file_name(&out.to_string_lossy()))
}

/// The Desktop folder (the one OneDrive keeps, when it does).
fn desktop_folder() -> Option<std::path::PathBuf> {
    let home = std::path::PathBuf::from(std::env::var_os("USERPROFILE")?);
    [home.join("OneDrive").join("Desktop"), home.join("Desktop")].into_iter().find(|p| p.is_dir())
}

fn short(s: &str) -> String {
    if s.chars().count() <= 30 {
        s.to_string()
    } else {
        format!("{}\u{2026}", s.chars().take(29).collect::<String>())
    }
}

pub struct Shelf {
    items: Vec<Item>,
    drop_hover: bool,
    remember: bool,
    bus: Option<Bus>,
    /// Alcove: which kind of item shows (0 all, 1 images, 2 files, 3 folders).
    filter: usize,
    /// The Tray and Converter tiles and the formats on show, as last drawn (for the drop).
    tiles: [Rect; 4],
    chips: Vec<(Rect, &'static str)>,
    /// Conversions on their way, the last thing the converter said, and whether a key is saved.
    busy: usize,
    status: Option<(String, Instant, bool)>,
    has_key: bool,
    hwnd: Option<crate::util::SendHwnd>,
}

impl Shelf {
    pub fn new() -> Self {
        Self {
            items: Vec::new(),
            drop_hover: false,
            remember: true,
            bus: None,
            filter: 0,
            tiles: [Rect::new(0.0, 0.0, 0.0, 0.0); 4],
            chips: Vec::new(),
            busy: 0,
            status: None,
            has_key: false,
            hwnd: None,
        }
    }

    fn add(&mut self, paths: &[String], pop: bool) {
        let mut fresh = Vec::new();
        for p in paths {
            if self.items.iter().any(|i| i.path.eq_ignore_ascii_case(p)) {
                continue;
            }
            self.items.push(Item { path: p.clone(), name: file_name(p), thumb: None, born: pop.then(Instant::now) });
            fresh.push(p.clone());
        }
        if self.items.len() > MAX_ITEMS {
            self.items.drain(..self.items.len() - MAX_ITEMS);
        }
        self.load_thumbs(fresh);
        self.save();
    }

    fn remove(&mut self, idx: usize) {
        if idx < self.items.len() {
            self.items.remove(idx);
            self.save();
        }
    }

    /// Forget items whose file was moved away or deleted.
    fn prune(&mut self) -> bool {
        let n = self.items.len();
        self.items.retain(|i| Path::new(&i.path).exists());
        let changed = self.items.len() != n;
        if changed {
            self.save();
        }
        changed
    }

    fn paths(&self) -> Vec<String> {
        self.items.iter().map(|i| i.path.clone()).collect()
    }

    fn load_thumbs(&self, paths: Vec<String>) {
        let Some(bus) = self.bus.clone() else { return };
        if paths.is_empty() {
            return;
        }
        let _ = std::thread::Builder::new().name("shelf-thumbs".into()).spawn(move || {
            use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};
            unsafe {
                let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            }
            for p in paths {
                let t = dragdrop::thumbnail(&p, 96);
                bus.to_module(ID, Thumb(p, t));
            }
            unsafe { CoUninitialize() };
        });
    }

    fn save(&self) {
        if !self.remember {
            return;
        }
        let json = serde_json::to_string_pretty(&self.paths()).unwrap_or_default();
        let _ = std::fs::write(store_path(), json);
    }
}

fn store_path() -> std::path::PathBuf {
    app_dir().join("shelf.json")
}

fn file_name(p: &str) -> String {
    let t = p.trim_end_matches(['\\', '/']);
    t.rsplit(['\\', '/']).next().filter(|s| !s.is_empty()).unwrap_or(t).to_string()
}

/// True when a press that started inside `r` has moved far enough to be a drag.
fn drag_started(ui: &Ui, r: Rect) -> bool {
    match (ui.input.down, ui.input.mouse) {
        (Some((dx, dy)), Some((mx, my))) => {
            ui.interactive && r.contains(dx, dy) && ((mx - dx).powi(2) + (my - dy).powi(2)).sqrt() > DRAG_SLOP
        }
        _ => false,
    }
}

impl Module for Shelf {
    fn id(&self) -> ModuleId {
        ID
    }
    fn title(&self) -> &str {
        "Shelf"
    }
    fn icon(&self) -> Icon {
        Icon::Package
    }

    fn start(&mut self, cx: &mut Cx) {
        self.bus = Some(cx.bus.clone());
        self.hwnd = Some(cx.hwnd);
        self.has_key = sys::dotenv::get_saved_first(KEY_VAR).is_some();
        self.remember = cx.cfg.shelf.remember;
        if self.remember && self.items.is_empty() {
            let saved: Vec<String> = std::fs::read_to_string(store_path())
                .ok()
                .and_then(|t| serde_json::from_str(&t).ok())
                .unwrap_or_default();
            let existing: Vec<String> = saved.into_iter().filter(|p| Path::new(p).exists()).collect();
            self.add(&existing, false);
        }
    }

    fn on_message(&mut self, msg: Box<dyn Any + Send>, cx: &mut Cx) {
        let msg = match msg.downcast::<Thumb>() {
            Ok(t) => {
                if let Some(it) = self.items.iter_mut().find(|i| i.path == t.0) {
                    it.thumb = t.1;
                    cx.fx.redraw = true;
                }
                return;
            }
            Err(m) => m,
        };
        let msg = match msg.downcast::<Copied>() {
            Ok(c) => {
                match c.0 {
                    Ok((to, name)) => {
                        self.say(&format!("Copied to {to}"), false);
                        cx.fx.peek(Peek::new(Icon::Check, palette::GREEN, format!("Copied to {to}"), name).key("copy").duration_ms(3500));
                    }
                    Err(e) => {
                        self.say(&e, true);
                        cx.fx.peek(Peek::new(Icon::Close, palette::RED, "Couldn't copy", e).key("copy").duration_ms(4500));
                    }
                }
                cx.fx.redraw = true;
                return;
            }
            Err(m) => m,
        };
        if let Ok(c) = msg.downcast::<Converted>() {
            self.busy = self.busy.saturating_sub(1);
            match c.0 {
                Ok(path) => {
                    let name = file_name(&path);
                    self.add(&[path], true);
                    self.say("Saved to Downloads", false);
                    cx.fx.peek(
                        Peek::new(Icon::Check, palette::GREEN, "Converted", name).key("convert").page(ID).duration_ms(4500),
                    );
                    cx.fx.layout_changed = true;
                }
                Err(e) => {
                    self.say(&e, true);
                    cx.fx.peek(Peek::new(Icon::Close, palette::RED, "Couldn't convert", e).key("convert").duration_ms(5000));
                }
            }
            cx.fx.redraw = true;
        }
    }

    fn on_system(&mut self, ev: &SystemEvent, cx: &mut Cx) {
        match ev {
            SystemEvent::DragHover(on) => {
                if *on && !self.drop_hover {
                    self.chips.clear();
                    self.tiles = [Rect::new(0.0, 0.0, 0.0, 0.0); 4];
                }
                self.drop_hover = *on;
                cx.fx.redraw = true;
                cx.fx.layout_changed = true;
            }
            SystemEvent::Dropped(files) => {
                self.drop_hover = false;
                // dropped on a format: turn it; on the Converter between formats: nothing; else: park it
                let fmt = self.chip_at();
                let on_converter = self.over(3);
                let copy_to = if on_converter {
                    None
                } else if self.over(1) {
                    Some("Downloads")
                } else if self.over(2) {
                    Some("Desktop")
                } else {
                    None
                };
                clear_pointer();
                if let Some(fmt) = fmt {
                    self.convert(files.clone(), fmt);
                } else if on_converter {
                    self.say("Drop it on a format", true);
                } else if let Some(to) = copy_to {
                    self.copy_into(files, to);
                } else {
                    self.add(files, true);
                }
                cx.fx.layout_changed = true;
            }
            SystemEvent::DragOutDone | SystemEvent::Expanded(true) if self.prune() => {
                cx.fx.layout_changed = true;
            }
            SystemEvent::ConfigReloaded => {
                self.has_key = sys::dotenv::get_saved_first(KEY_VAR).is_some();
                self.remember = cx.cfg.shelf.remember;
                if !self.remember {
                    let _ = std::fs::remove_file(store_path());
                } else {
                    self.save();
                }
            }
            _ => {}
        }
    }

    fn card(&self) -> Option<CardSize> {
        (!self.items.is_empty()).then_some(CardSize::Small)
    }

    fn draw_card(&mut self, ui: &mut Ui, r: Rect) {
        if drag_started(ui, r) {
            ui.fx.drag_files = Some(self.paths());
        }
        if ui.card(r, true) {
            ui.fx.open_page = Some(ID);
        }
        let inner = r.inset_xy(12.0, 0.0);
        ui.p.icon(Icon::Package, Rect::new(inner.x, r.cy() - 9.0, 18.0, 18.0), palette::YELLOW);
        let label = match self.items.len() {
            1 => self.items[0].name.clone(),
            n => format!("{n} items"),
        };
        // up to three overlapping thumbnails on the right
        let mut x = inner.right();
        let s = (r.h - 12.0).min(26.0);
        for it in self.items.iter().rev().take(3) {
            x -= s * 0.72;
            let tr = Rect::new(x - s * 0.28, r.cy() - s * 0.5, s, s);
            match &it.thumb {
                Some(img) => ui.p.image(img, tr, 5.0, 1.0),
                None => ui.p.icon(Icon::File, tr, palette::TEXT_DIM),
            }
        }
        let tx = inner.x + 28.0;
        ui.p.text(&label, Rect::new(tx, r.y, x - tx - 12.0, r.h), &TextStyle::new(12.5, palette::TEXT));
    }

    fn has_page(&self) -> bool {
        true
    }

    fn page_height(&self, w: f32) -> f32 {
        if crate::ui::look_alcove() {
            return if self.drop_hover {
                132.0
            } else if self.items.is_empty() {
                120.0
            } else {
                172.0
            };
        }
        if self.items.is_empty() {
            return 120.0;
        }
        let cols = (((w + GAP) / (TILE_W + GAP)).floor() as usize).max(1);
        let rows = self.items.len().div_ceil(cols);
        28.0 + rows as f32 * (TILE_H + GAP)
    }

    fn draw_page(&mut self, ui: &mut Ui, r: Rect) {
        if crate::ui::look_alcove() {
            return self.draw_alcove(ui, r);
        }
        let (head, body) = r.split_top(24.0, 4.0);
        ui.caption("SHELF", Rect::new(head.x, head.y, 200.0, head.h));
        let accent = palette::YELLOW;

        // drop zone highlight while files hover over the notch
        if self.drop_hover {
            let zone = Rect::new(body.x, body.y, body.w, body.h - GAP * 0.5);
            // monochrome look: a quiet white fill and small white dashes instead of a solid coloured line
            let tint = if crate::ui::mono_icons() { Color::white(1.0) } else { accent };
            ui.p.fill_rounded(zone, 14.0, tint.with_a(0.05));
            ui.p.dashed_rounded(zone.inset(1.5), 12.5, 1.6, tint.with_a(0.75));
            ui.ticks_sweep(Rect::new(zone.x + 16.0, zone.bottom() - 18.0, zone.w - 32.0, 10.0), accent);
        }
        if self.items.is_empty() {
            let msg =
                if self.drop_hover { "Drop to park files here" } else { "Drag files onto the notch to park them" };
            ui.empty_state(body, Icon::Package, msg);
            return;
        }

        // header actions
        let bs = ButtonStyle::filled(Color::white(0.08)).fg(palette::TEXT_DIM);
        let clear = Rect::new(head.right() - 64.0, head.y + 1.0, 64.0, 22.0);
        if ui.pill_button(clear, "Clear", None, bs) {
            self.items.clear();
            self.save();
            ui.fx.layout_changed = true;
            return;
        }
        let fill = self.items.len() as f32 / MAX_ITEMS as f32;
        let meter_right = if self.items.len() > 1 { clear.x - 96.0 - 10.0 } else { clear.x - 10.0 };
        ui.ticks(Rect::new(meter_right - 72.0, head.cy() - 5.0, 72.0, 10.0), fill, accent);
        if self.items.len() > 1 {
            let all = Rect::new(clear.x - 96.0, head.y + 1.0, 90.0, 22.0);
            if drag_started(ui, all) {
                ui.fx.drag_files = Some(self.paths());
            }
            ui.pill_button(all, "Drag all", Some(Icon::Copy), bs);
        }

        let cols = (((body.w + GAP) / (TILE_W + GAP)).floor() as usize).max(1);
        let mut remove = None;
        for (i, it) in self.items.iter().enumerate() {
            let (c, row) = (i % cols, i / cols);
            let tile =
                Rect::new(body.x + c as f32 * (TILE_W + GAP), body.y + row as f32 * (TILE_H + GAP), TILE_W, TILE_H);
            let hover = ui.hovered(tile);
            if hover {
                ui.hot = true;
                ui.p.fill_rounded(tile, 12.0, palette::CARD_HOVER);
            }
            let th = Rect::new(tile.cx() - THUMB * 0.5, tile.y + 10.0, THUMB, THUMB);
            match &it.thumb {
                Some(img) => {
                    // keep the aspect ratio inside the thumbnail square
                    let k = (THUMB / img.width.max(1) as f32).min(THUMB / img.height.max(1) as f32);
                    let (w, h) = (img.width as f32 * k, img.height as f32 * k);
                    ui.p.image(img, Rect::new(th.cx() - w * 0.5, th.cy() - h * 0.5, w, h), 6.0, 1.0);
                }
                None => ui.p.icon(Icon::File, th.inset(4.0), palette::TEXT_DIM),
            }
            let name = Rect::new(tile.x + 4.0, th.bottom() + 6.0, tile.w - 8.0, 18.0);
            ui.p.text(&it.name, name, &TextStyle::new(11.5, palette::TEXT).center());

            let x = Rect::new(tile.right() - 22.0, tile.y + 4.0, 18.0, 18.0);
            let mut on_x = false;
            if hover {
                on_x = ui.hovered(x);
                if ui.icon_button(x, Icon::Close, ButtonStyle::filled(Color::white(0.14)).scale(0.6)) {
                    remove = Some(i);
                }
            }
            if drag_started(ui, tile) && !on_x {
                ui.fx.drag_files = Some(vec![it.path.clone()]);
            } else if !on_x && ui.clicked(tile) {
                sys::shell_open(&it.path);
            }
        }
        if let Some(i) = remove {
            self.remove(i);
            ui.fx.layout_changed = true;
        }
    }
}

impl Shelf {
    /// Alcove: filter chips on top and a row of files below, each a thumbnail (or a plain icon)
    /// with its name under it; drag them out, click to open.
    fn draw_alcove(&mut self, ui: &mut Ui, r: Rect) {
        if self.drop_hover {
            return self.draw_drop(ui, r);
        }
        let (head, body) = r.split_top(32.0, 12.0);
        crate::alcove::title(ui, head, Icon::Package, palette::YELLOW, "Shelf", "");
        // the converter: what it is doing, or the way to set it up
        let mut lead = 0.0;
        let note = match &self.status {
            Some((m, at, bad)) if self.busy > 0 || at.elapsed() < Duration::from_secs(8) => Some((short(m), *bad)),
            _ => None,
        };
        let tx0 = head.x + crate::alcove::title_w(ui, head, "Shelf") + 8.0;
        if let Some((msg, bad)) = note {
            let w = crate::alcove::chip_w(ui, &msg, None) + 6.0;
            crate::alcove::chip(ui, Rect::new(tx0, head.y + 2.0, w, 28.0), &msg, None, bad);
            lead = w + 8.0;
        } else if !self.has_key {
            let label = "Set up converter";
            let w = crate::alcove::chip_w(ui, label, Some(Icon::Refresh)) + 6.0;
            if crate::alcove::chip(ui, Rect::new(tx0, head.y + 2.0, w, 28.0), label, Some(Icon::Refresh), false) {
                self.paste_key();
            }
            lead = w + 8.0;
        }
        let below = Rect::new(r.x, head.bottom() + 6.0, r.w, r.bottom() - head.bottom() - 6.0);
        if self.drop_hover {
            let tint = if crate::ui::mono_icons() { Color::white(1.0) } else { palette::YELLOW };
            ui.p.fill_rounded(below, 26.0, tint.with_a(0.05));
            ui.p.dashed_rounded(below.inset(1.5), 24.5, 1.6, tint.with_a(0.75));
            ui.p.text("Drop to park files here", below, &TextStyle::new(15.0, tint).semibold().center());
            return;
        }
        if self.items.is_empty() {
            crate::alcove::empty(ui, below, Icon::Package, "Drag files onto the notch to park them");
            return;
        }
        // 1 = picture, 3 = folder (no extension), 2 = any other file
        let kind = |it: &Item| -> usize {
            if it.thumb.is_some() {
                1
            } else if std::path::Path::new(&it.path).extension().is_none() {
                3
            } else {
                2
            }
        };
        let mut counts = [self.items.len(), 0, 0, 0];
        for it in &self.items {
            counts[kind(it)] += 1;
        }
        if counts[self.filter] == 0 {
            self.filter = 0;
        }
        let mut x = head.x + crate::alcove::title_w(ui, head, "Shelf") + lead;
        for (k, name) in ["All", "Images", "Files", "Folders"].iter().enumerate() {
            if k > 0 && counts[k] == 0 {
                continue;
            }
            let w = crate::alcove::chip_w(ui, name, None) + 6.0;
            if crate::alcove::chip(ui, Rect::new(x, head.y + 2.0, w, 28.0), name, None, self.filter == k) {
                self.filter = k;
            }
            x += w + 8.0;
        }
        // actions on the right
        let cw = crate::alcove::chip_w(ui, "Clear", None);
        let clear = Rect::new(head.right() - cw, head.y + 2.0, cw, 28.0);
        if crate::alcove::chip(ui, clear, "Clear", None, false) {
            self.items.clear();
            self.save();
            ui.fx.layout_changed = true;
            return;
        }
        if self.items.len() > 1 {
            let aw = crate::alcove::chip_w(ui, "Drag all", Some(Icon::Copy));
            let all = Rect::new(clear.x - 8.0 - aw, head.y + 2.0, aw, 28.0);
            if drag_started(ui, all) {
                ui.fx.drag_files = Some(self.paths());
            }
            crate::alcove::chip(ui, all, "Drag all", Some(Icon::Copy), false);
        }

        let shown: Vec<usize> =
            (0..self.items.len()).filter(|&i| self.filter == 0 || kind(&self.items[i]) == self.filter).collect();
        let tw = 98.0;
        let gap = 8.0;
        let content_w = shown.len() as f32 * (tw + gap) - gap;
        let off = crate::alcove::scroll_x(ui, body, content_w, "shelf-scroll");
        let mut remove = None;
        ui.p.push_clip(body);
        for (j, &i) in shown.iter().enumerate() {
            let it = &self.items[i];
            let t = Rect::new(body.x + j as f32 * (tw + gap) - off, body.y, tw, body.h);
            if t.right() < body.x - 4.0 || t.x > body.right() + 4.0 {
                continue;
            }
            // a new file rises into place with a little give and fades in
            let pop = it.born.map(|b| ui.now.saturating_duration_since(b).as_secs_f32() / 0.6).filter(|u| *u < 1.0);
            let (lift, fade) = match pop {
                Some(u) => {
                    let c = 1.3_f32;
                    let e = 1.0 + (c + 1.0) * (u - 1.0).powi(3) + c * (u - 1.0).powi(2);
                    ui.animate();
                    ((1.0 - e) * 18.0, (u / 0.35).min(1.0))
                }
                None => (0.0, 1.0),
            };
            let t = t.translate(0.0, lift);
            let alpha_before = ui.p.alpha;
            ui.p.alpha = alpha_before * fade;
            let hover = ui.hovered(t) && ui.hovered(body);
            if hover {
                ui.hot = true;
                ui.p.fill_rounded(t, 20.0, Color::white(0.08));
            }
            let th = Rect::new(t.x + 11.0, t.y + 8.0, t.w - 22.0, t.w - 22.0);
            match &it.thumb {
                Some(img) => {
                    let k = (th.w / img.width.max(1) as f32).min(th.h / img.height.max(1) as f32);
                    let (w, h) = (img.width as f32 * k, img.height as f32 * k);
                    ui.p.image(img, Rect::new(th.cx() - w * 0.5, th.cy() - h * 0.5, w, h), 12.0, 1.0);
                }
                None => {
                    let label = crate::alcove::badge_label(&it.path, kind(it) == 3);
                    crate::alcove::file_badge(ui, th.center_square(54.0), &label);
                }
            }
            ui.p.push_clip(Rect::new(t.x + 2.0, th.bottom() + 4.0, t.w - 4.0, t.bottom() - th.bottom() - 6.0));
            ui.p.text(
                &it.name,
                Rect::new(t.x + 4.0, th.bottom() + 6.0, t.w - 8.0, 36.0),
                &TextStyle::new(12.0, Color::white(0.9)).medium().center().wrap().top(),
            );
            ui.p.pop_clip();
            let xb = Rect::new(t.right() - 26.0, t.y + 4.0, 20.0, 20.0);
            let mut on_x = false;
            if hover {
                on_x = ui.hovered(xb);
                if crate::alcove::round_button(ui, xb, Icon::Close, false) {
                    remove = Some(i);
                }
            }
            if drag_started(ui, t) && !on_x {
                ui.fx.drag_files = Some(vec![it.path.clone()]);
            } else if !on_x && ui.clicked(t) {
                sys::shell_open(&it.path);
            }
            ui.p.alpha = alpha_before;
        }
        ui.p.pop_clip();
        if let Some(i) = remove {
            self.remove(i);
            ui.fx.layout_changed = true;
        }
    }
}


impl Shelf {
    fn say(&mut self, msg: &str, bad: bool) {
        self.status = Some((msg.to_string(), Instant::now(), bad));
    }

    /// Put a copy of the files in Downloads or on the Desktop (off the UI thread: big files take a while).
    fn copy_into(&mut self, files: &[String], to: &'static str) {
        let Some(bus) = self.bus.clone() else { return };
        let dir = if to == "Desktop" { desktop_folder() } else { super::downloads::downloads_folder() };
        let Some(dir) = dir else {
            self.say("Can't find that folder", true);
            return;
        };
        self.say(&format!("Copying to {to}..."), false);
        let files = files.to_vec();
        let _ = std::thread::Builder::new().name("shelf-copy".into()).spawn(move || {
            for f in files {
                let r = copy_one(Path::new(&f), &dir).map(|name| (to.to_string(), name));
                bus.to_module(ID, Copied(r));
            }
        });
    }

    /// Whether the pointer (while dragging) is over tile `i`.
    fn over(&self, i: usize) -> bool {
        pointer().is_some_and(|(x, y)| self.tiles[i].contains(x, y))
    }

    /// The format under the pointer, if the formats are open.
    fn chip_at(&self) -> Option<&'static str> {
        let (x, y) = pointer()?;
        self.chips.iter().find(|(r, _)| r.contains(x, y)).map(|c| c.1)
    }

    /// Take the key from the clipboard (copied from the FreeConvert account page).
    fn paste_key(&mut self) {
        let text = self.hwnd.and_then(|h| sys::clip::get_text(h.hwnd())).unwrap_or_default();
        let key = text.trim().to_string();
        if key.len() < 20 || key.chars().any(char::is_whitespace) {
            self.say("Copy your key first", true);
            return;
        }
        if sys::dotenv::set_saved(KEY_VAR, &key) {
            self.has_key = true;
            self.say("Converter is ready", false);
        } else {
            self.say("Couldn't save the key", true);
        }
    }

    /// Send the files to be turned into `fmt`; every result comes back as a `Converted` message.
    fn convert(&mut self, files: Vec<String>, fmt: &'static str) {
        let key = sys::dotenv::get_saved_first(KEY_VAR);
        self.has_key = key.is_some();
        // pictures this PC can convert itself need no key; the rest go to FreeConvert
        let (files, parked): (Vec<String>, Vec<String>) =
            files.into_iter().partition(|f| key.is_some() || can_local(f, fmt));
        if !parked.is_empty() {
            self.say("Set up the converter first", true);
            self.add(&parked, true);
        }
        let Some(bus) = self.bus.clone() else { return };
        if files.is_empty() {
            return;
        }
        self.busy += files.len();
        self.say(&format!("Converting to {}...", fmt.to_ascii_uppercase()), false);
        let _ = std::thread::Builder::new().name("shelf-convert".into()).spawn(move || {
            use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};
            unsafe {
                let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            }
            for f in files {
                let r = if can_local(&f, fmt) {
                    // a picture type Windows can't read falls back to FreeConvert when there is a key
                    local_convert(&f, fmt).or_else(|e| match &key {
                        Some(k) => cloud_convert(k, &f, fmt),
                        None => Err(e),
                    })
                } else {
                    match &key {
                        Some(k) => cloud_convert(k, &f, fmt),
                        None => Err("Set up the converter first".to_string()),
                    }
                };
                bus.to_module(ID, Converted(r));
            }
            unsafe { CoUninitialize() };
        });
    }

    /// Files are being dragged over the notch: four drop tiles. Tray keeps them on the shelf, Downloads and
    /// Desktop put a copy there, and the Converter (at the right) opens up into the formats it can turn them
    /// into; the file is dropped on the one wanted.
    fn draw_drop(&mut self, ui: &mut Ui, r: Rect) {
        self.chips.clear();
        let gap = 10.0;
        let tw = (r.w - 3.0 * gap) / 4.0;
        let h = r.h.min(116.0);
        let top = r.cy() - h * 0.5;
        let mut tiles = [0usize, 1, 2, 3].map(|i| Rect::new(r.x + i as f32 * (tw + gap), top, tw, h));
        let ptr = pointer();
        // the open Converter keeps the pointer: it widens over the tiles beside it
        let conv_hot = ptr.is_some_and(|(x, y)| self.tiles[3].contains(x, y));
        if conv_hot {
            let ew = (tw * 2.7 + gap * 1.7).min(r.w);
            tiles[3] = Rect::new(r.right() - ew, top, ew, h);
        }
        self.tiles = tiles;
        let hot = [0usize, 1, 2, 3].map(|i| ptr.is_some_and(|(x, y)| tiles[i].contains(x, y)) && (i == 3 || !conv_hot));
        let files = DRAG_FILES.lock().map(|g| g.clone()).unwrap_or_default();
        let first = files.first().cloned().unwrap_or_default();
        let have = ext_of(&first);
        // without a key only the pictures Windows converts itself are offered
        let fmts: Vec<&'static str> = formats_for(&first)
            .iter()
            .copied()
            .filter(|f| !same_format(f, &have) && (self.has_key || can_local(&first, f)))
            .collect();
        let names = ["Tray", "Downloads", "Desktop", "Converter"];
        let icons = [Icon::Package, Icon::Download, Icon::Folder, Icon::Refresh];
        for i in 0..4 {
            let tint = if i == 3 { palette::GREEN } else { Color::white(1.0) };
            let tile = tiles[i];
            let on = hot[i];
            ui.p.fill_rounded(tile, 22.0, Color::white(if on { 0.17 } else { 0.07 }));
            ui.p.stroke_rounded(tile.inset(0.5), 21.5, 1.0, if on { tint.with_a(0.75) } else { Color::white(0.10) });
            if i == 3 && on {
                if fmts.is_empty() {
                    ui.p.text(
                        "Tap Set up converter on the Shelf page first",
                        Rect::new(tile.x + 12.0, tile.y, tile.w - 24.0, tile.h),
                        &TextStyle::new(12.5, Color::white(0.8)).medium().center().wrap(),
                    );
                } else {
                    let inner = tile.inset_xy(12.0, 10.0);
                    let (cw, g) = (56.0_f32, 6.0_f32);
                    let cols = (((inner.w + g) / (cw + g)).floor() as usize).clamp(1, fmts.len());
                    let rows = fmts.len().div_ceil(cols);
                    let ch = ((inner.h - (rows - 1) as f32 * g) / rows as f32).clamp(18.0, 30.0);
                    let gw = cols as f32 * (cw + g) - g;
                    let gh = rows as f32 * (ch + g) - g;
                    let (x0, y0) = (inner.cx() - gw * 0.5, inner.cy() - gh * 0.5);
                    for (j, f) in fmts.iter().enumerate() {
                        let cr = Rect::new(x0 + (j % cols) as f32 * (cw + g), y0 + (j / cols) as f32 * (ch + g), cw, ch);
                        self.chips.push((cr, *f));
                        let over = ptr.is_some_and(|(x, y)| cr.contains(x, y));
                        let fill = if over { palette::GREEN.with_a(0.9) } else { Color::white(0.09) };
                        ui.p.fill_rounded(cr, ch * 0.5, fill);
                        let txt = if over { Color::black(1.0) } else { Color::white(0.88) };
                        ui.p.text(&f.to_ascii_uppercase(), cr, &TextStyle::new(12.0, txt).semibold().center());
                    }
                }
            } else {
                let ic = Rect::new(tile.cx() - 15.0, tile.cy() - 28.0, 30.0, 30.0);
                ui.p.icon(icons[i], ic, tint.with_a(if on { 1.0 } else { 0.9 }));
                ui.p.text(
                    names[i],
                    Rect::new(tile.x, tile.cy() + 10.0, tile.w, 22.0),
                    &TextStyle::new(13.0, Color::white(if on { 1.0 } else { 0.88 })).medium().center(),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::file_name;

    #[test]
    fn names() {
        assert_eq!(file_name(r"C:\Users\me\report.pdf"), "report.pdf");
        assert_eq!(file_name(r"C:\Users\me\Photos\"), "Photos");
        assert_eq!(file_name(r"D:\"), "D:");
    }
}
