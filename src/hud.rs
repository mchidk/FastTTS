//! Click-through oscilloscope HUD above the taskbar.
//!
//! Everything is drawn in code: a rounded panel, a status dot, the label,
//! and live bars. A dedicated thread owns the layered window and redraws at
//! about 30 fps; the app only flips a mutex-protected view.

use crate::audio::Meter;
use anyhow::{anyhow, Context, Result};
use std::{
    collections::{HashMap, VecDeque},
    ffi::c_void,
    sync::{Arc, Mutex, MutexGuard},
    thread::JoinHandle,
    time::Instant,
};
use windows::{
    core::PCWSTR,
    Win32::{
        Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM},
        Graphics::Gdi::{
            CreateCompatibleDC, CreateDIBSection, CreateFontW, DeleteDC, DeleteObject, GdiFlush,
            GetDC, ReleaseDC, SelectObject, SetBkMode, SetTextCharacterExtra, SetTextColor,
            TextOutW, ANTIALIASED_QUALITY, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION,
            CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DEFAULT_PITCH, DIB_RGB_COLORS, FONT_QUALITY,
            FW_NORMAL, FW_SEMIBOLD, HBITMAP, HDC, HGDIOBJ, OUT_TT_PRECIS, TRANSPARENT,
        },
        System::{LibraryLoader::GetModuleHandleW, Threading::GetCurrentThreadId},
        UI::WindowsAndMessaging::{
            CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
            GetWindowLongPtrW, KillTimer, PostThreadMessageW, RegisterClassExW, SetTimer,
            SetWindowLongPtrW, ShowWindow, SystemParametersInfoW, UpdateLayeredWindow,
            GWLP_USERDATA, MSG, SPI_GETWORKAREA, SW_HIDE, SW_SHOWNOACTIVATE,
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, ULW_ALPHA, WM_QUIT, WM_TIMER, WNDCLASSEXW,
            WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT,
            WS_POPUP,
        },
    },
};

const WIDTH: usize = 680;
const HEIGHT: usize = 150;
const INSET: f32 = 4.0;
const RADIUS: f32 = 24.0;
const PANEL: [u8; 3] = [27, 29, 37];
const LABEL: [u8; 3] = [245, 246, 250];
const BRAND: [u8; 3] = [159, 163, 175];
const BAR_COUNT: usize = 38;
const BAR_WIDTH: f32 = 4.0;
const BAR_TOP: f32 = 78.0;
const BAR_BOTTOM: f32 = 128.0;
/// Level mapping for the bars: -54 dBFS is flat, -12 dBFS is full height.
const FLOOR_DB: f32 = -54.0;
const CEIL_DB: f32 = -12.0;
const FRAME_MS: u32 = 33;
const FADE_STEP: u8 = 51;
const ERROR_SECONDS: f32 = 2.4;

#[derive(Clone, PartialEq, Debug)]
enum View {
    Hidden,
    Listening,
    Transcribing,
    Error(String),
}

impl View {
    fn accent(&self) -> [u8; 3] {
        match self {
            View::Transcribing => [255, 180, 84],
            View::Error(_) => [248, 113, 113],
            _ => [255, 92, 108],
        }
    }

    fn label(&self) -> &str {
        match self {
            View::Hidden => "",
            View::Listening => "LISTENING",
            View::Transcribing => "TRANSCRIBING",
            View::Error(text) => text,
        }
    }
}

struct State {
    view: View,
    since: Instant,
}

struct Shared {
    state: Mutex<State>,
    meter: Arc<Meter>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|err| err.into_inner())
    }

    fn set(&self, view: View) {
        let mut state = self.lock();
        if state.view != view {
            state.view = view;
            state.since = Instant::now();
        }
    }

    /// The view to draw now; errors dismiss themselves.
    fn current(&self) -> View {
        let mut state = self.lock();
        if matches!(state.view, View::Error(_))
            && state.since.elapsed().as_secs_f32() > ERROR_SECONDS
        {
            state.view = View::Hidden;
        }
        state.view.clone()
    }
}

/// Handle to the HUD thread.
pub struct Hud {
    shared: Arc<Shared>,
    thread_id: u32,
    thread: Option<JoinHandle<()>>,
}

impl Hud {
    pub fn new(meter: Arc<Meter>) -> Result<Self> {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                view: View::Hidden,
                since: Instant::now(),
            }),
            meter,
        });
        let (tx, rx) = std::sync::mpsc::channel::<Result<u32>>();
        let thread_shared = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("fasttts-hud".into())
            .spawn(move || run(thread_shared, tx))?;
        match rx.recv() {
            Ok(Ok(thread_id)) => Ok(Self {
                shared,
                thread_id,
                thread: Some(thread),
            }),
            Ok(Err(err)) => {
                let _ = thread.join();
                Err(err)
            }
            Err(_) => {
                let _ = thread.join();
                Err(anyhow!("the HUD thread stopped"))
            }
        }
    }

    pub fn listening(&self) {
        self.shared.set(View::Listening);
    }

    /// Show progress, after any error message has had its moment.
    pub fn transcribing(&self) {
        if !matches!(self.shared.current(), View::Error(_)) {
            self.shared.set(View::Transcribing);
        }
    }

    /// Show a short message such as "NO SPEECH" for a moment.
    pub fn error(&self, text: &str) {
        self.shared.set(View::Error(text.to_uppercase()));
    }

    /// Fade out, unless an error message is still on screen.
    pub fn idle(&self) {
        let mut state = self.shared.lock();
        if !matches!(state.view, View::Error(_) | View::Hidden) {
            state.view = View::Hidden;
            state.since = Instant::now();
        }
    }
}

impl Drop for Hud {
    fn drop(&mut self) {
        unsafe {
            let _ = PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Pixel coverage of `text` rendered by GDI with grayscale antialiasing.
struct TextMask {
    width: usize,
    height: usize,
    coverage: Vec<u8>,
}

impl TextMask {
    fn render(text: &str, size: i32, weight: u32, spacing: i32) -> Result<Self> {
        const CANVAS_W: usize = 640;
        const CANVAS_H: usize = 80;
        const PAD: i32 = 8;
        let mut coverage = vec![0u8; CANVAS_W * CANVAS_H];
        unsafe {
            let dc = CreateCompatibleDC(None);
            let info = bitmap_info(CANVAS_W, CANVAS_H);
            let mut bits: *mut c_void = std::ptr::null_mut();
            let bitmap = match CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut bits, None, 0)
            {
                Ok(bitmap) => bitmap,
                Err(err) => {
                    let _ = DeleteDC(dc);
                    return Err(err).context("could not create a text bitmap");
                }
            };
            let face: Vec<u16> = "Segoe UI\0".encode_utf16().collect();
            let font = CreateFontW(
                -size,
                0,
                0,
                0,
                weight as i32,
                0,
                0,
                0,
                DEFAULT_CHARSET,
                OUT_TT_PRECIS,
                CLIP_DEFAULT_PRECIS,
                FONT_QUALITY(ANTIALIASED_QUALITY.0),
                DEFAULT_PITCH.0 as u32,
                PCWSTR(face.as_ptr()),
            );
            let old_bitmap = SelectObject(dc, HGDIOBJ(bitmap.0));
            let old_font = SelectObject(dc, HGDIOBJ(font.0));
            SetTextColor(dc, COLORREF(0x00FF_FFFF));
            SetBkMode(dc, TRANSPARENT);
            SetTextCharacterExtra(dc, spacing);
            let wide: Vec<u16> = text.encode_utf16().collect();
            let _ = TextOutW(dc, PAD, PAD, &wide);
            let _ = GdiFlush();
            let pixels = std::slice::from_raw_parts(bits as *const u8, CANVAS_W * CANVAS_H * 4);
            for (dst, px) in coverage.iter_mut().zip(pixels.as_chunks::<4>().0) {
                *dst = px[1];
            }
            SelectObject(dc, old_font);
            SelectObject(dc, old_bitmap);
            let _ = DeleteObject(HGDIOBJ(font.0));
            let _ = DeleteObject(HGDIOBJ(bitmap.0));
            let _ = DeleteDC(dc);
        }
        // Crop to the ink so callers can place the glyphs exactly.
        let ink = |x: usize, y: usize| coverage[y * CANVAS_W + x] > 0;
        let rows: Vec<usize> = (0..CANVAS_H)
            .filter(|&y| (0..CANVAS_W).any(|x| ink(x, y)))
            .collect();
        let cols: Vec<usize> = (0..CANVAS_W)
            .filter(|&x| (0..CANVAS_H).any(|y| ink(x, y)))
            .collect();
        let (Some(&top), Some(&bottom), Some(&left), Some(&right)) =
            (rows.first(), rows.last(), cols.first(), cols.last())
        else {
            return Ok(Self {
                width: 0,
                height: 0,
                coverage: Vec::new(),
            });
        };
        let (width, height) = (right - left + 1, bottom - top + 1);
        let mut cropped = Vec::with_capacity(width * height);
        for y in top..=bottom {
            cropped.extend_from_slice(&coverage[y * CANVAS_W + left..=y * CANVAS_W + right]);
        }
        Ok(Self {
            width,
            height,
            coverage: cropped,
        })
    }
}

fn bitmap_info(width: usize, height: usize) -> BITMAPINFO {
    BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width as i32,
            biHeight: -(height as i32),
            biPlanes: 1,
            biBitCount: 32,
            ..Default::default()
        },
        ..Default::default()
    }
}

/// Straight-alpha BGRA canvas.
struct Canvas(Vec<u8>);

impl Canvas {
    fn blend(&mut self, x: usize, y: usize, color: [u8; 3], amount: f32) {
        if x >= WIDTH || y >= HEIGHT || amount <= 0.0 {
            return;
        }
        let p = (y * WIDTH + x) * 4;
        let a = amount.min(1.0);
        for (channel, &value) in [2usize, 1, 0].iter().zip(&color) {
            let old = self.0[p + channel] as f32;
            self.0[p + channel] = (old + (value as f32 - old) * a).round() as u8;
        }
    }

    fn text(&mut self, mask: &TextMask, left: usize, top: usize, color: [u8; 3]) {
        for y in 0..mask.height {
            for x in 0..mask.width {
                let c = mask.coverage[y * mask.width + x];
                self.blend(left + x, top + y, color, c as f32 / 255.0);
            }
        }
    }
}

/// Draws HUD frames. Has no window, so tests can render it too.
struct Painter {
    background: Vec<u8>,
    labels: HashMap<String, TextMask>,
}

impl Painter {
    fn new() -> Result<Self> {
        let mut canvas = Canvas(vec![0; WIDTH * HEIGHT * 4]);
        // Antialiased rounded panel from a signed distance field.
        let (right, bottom) = (WIDTH as f32 - INSET, HEIGHT as f32 - INSET);
        for y in 0..HEIGHT {
            for x in 0..WIDTH {
                let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
                let dx = (INSET + RADIUS - px).max(px - (right - RADIUS)).max(0.0);
                let dy = (INSET + RADIUS - py).max(py - (bottom - RADIUS)).max(0.0);
                let distance = (dx * dx + dy * dy).sqrt() - RADIUS;
                let alpha = (0.5 - distance).clamp(0.0, 1.0);
                let p = (y * WIDTH + x) * 4;
                canvas.0[p..p + 4].copy_from_slice(&[PANEL[2], PANEL[1], PANEL[0], 0]);
                canvas.0[p + 3] = (alpha * 255.0).round() as u8;
            }
        }
        let brand = TextMask::render("FastTTS", 16, FW_NORMAL.0, 0)?;
        canvas.text(&brand, 646 - brand.width.min(646), 54, BRAND);
        Ok(Self {
            background: canvas.0,
            labels: HashMap::new(),
        })
    }

    /// Draw `view` with bar heights in 0..=1 into a straight-alpha BGRA frame.
    fn paint(&mut self, view: &View, bars: &[f32]) -> Result<Vec<u8>> {
        let mut canvas = Canvas(self.background.clone());
        let accent = view.accent();

        // Status dot.
        let (cx, cy, r) = (44.0f32, 60.0f32, 8.0f32);
        for y in 50..70 {
            for x in 34..54 {
                let d = ((x as f32 + 0.5 - cx).powi(2) + (y as f32 + 0.5 - cy).powi(2)).sqrt();
                canvas.blend(x, y, accent, (r + 0.5 - d).clamp(0.0, 1.0));
            }
        }

        let label = view.label().to_string();
        if !self.labels.contains_key(&label) {
            let mask = TextMask::render(&label, 30, FW_SEMIBOLD.0, 3)?;
            self.labels.insert(label.clone(), mask);
        }
        canvas.text(&self.labels[&label], 67, 51, LABEL);

        // Bars, centred vertically, with antialiased ends.
        let center = (BAR_TOP + BAR_BOTTOM) / 2.0;
        let max = BAR_BOTTOM - BAR_TOP;
        let left = (WIDTH / 20) as f32;
        let span = WIDTH as f32 - 2.0 * left;
        for (i, &value) in bars.iter().enumerate() {
            let x = left + (i + 1) as f32 * span / (BAR_COUNT + 1) as f32 - BAR_WIDTH / 2.0;
            let half = (value.clamp(0.0, 1.0) * max).max(3.0) / 2.0;
            let (top, bottom) = (center - half, center + half);
            for y in top.floor() as usize..bottom.ceil() as usize {
                let row = (y as f32 + 1.0).min(bottom) - (y as f32).max(top);
                for px in x.round() as usize..(x + BAR_WIDTH).round() as usize {
                    canvas.blend(px, y, accent, row);
                }
            }
        }
        Ok(canvas.0)
    }
}

/// Map a microphone RMS to a bar height on a decibel scale.
fn level_to_bar(rms: f32) -> f32 {
    let db = 20.0 * rms.max(1e-6).log10();
    ((db - FLOOR_DB) / (CEIL_DB - FLOOR_DB)).clamp(0.0, 1.0)
}

/// Bar animation state.
struct Bars {
    history: VecDeque<f32>,
    display: Vec<f32>,
    started: Instant,
}

impl Bars {
    fn new() -> Self {
        Self {
            history: VecDeque::from(vec![0.0; BAR_COUNT]),
            display: vec![0.0; BAR_COUNT],
            started: Instant::now(),
        }
    }

    fn step(&mut self, view: &View, level: f32) -> &[f32] {
        let t = self.started.elapsed().as_secs_f32();
        let target: Vec<f32> = match view {
            View::Listening => {
                self.history.pop_front();
                self.history.push_back(level_to_bar(level));
                self.history.iter().map(|v| 0.06 + 0.94 * v).collect()
            }
            View::Transcribing => {
                let position = (t * 1.15).fract() * (BAR_COUNT - 1) as f32;
                (0..BAR_COUNT)
                    .map(|i| {
                        let d = (i as f32 - position) / 2.6;
                        0.06 + 0.55 * (-d * d).exp() + 0.04 * (t * 5.0 + i as f32 * 0.5).sin()
                    })
                    .collect()
            }
            _ => (0..BAR_COUNT)
                .map(|i| 0.06 + 0.03 * (t * 8.0 + i as f32).sin())
                .collect(),
        };
        if !matches!(view, View::Listening) {
            self.history.iter_mut().for_each(|v| *v = 0.0);
        }
        for (shown, goal) in self.display.iter_mut().zip(target) {
            *shown += (goal - *shown) * 0.55;
        }
        &self.display
    }
}

/// Owned by the HUD thread; `GWLP_USERDATA` points at it.
struct Window {
    shared: Arc<Shared>,
    painter: Painter,
    bars: Bars,
    hwnd: HWND,
    screen: HDC,
    memory: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
    bits: *mut u8,
    opacity: u8,
    shown: View,
    position: POINT,
}

impl Window {
    fn create(shared: Arc<Shared>) -> Result<Box<Self>> {
        let painter = Painter::new()?;
        unsafe {
            let instance = GetModuleHandleW(None)?;
            let class: Vec<u16> = "FastTTSHud\0".encode_utf16().collect();
            let wc = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(window_proc),
                hInstance: instance.into(),
                lpszClassName: PCWSTR(class.as_ptr()),
                ..Default::default()
            };
            RegisterClassExW(&wc);
            let hwnd = CreateWindowExW(
                WS_EX_LAYERED
                    | WS_EX_TRANSPARENT
                    | WS_EX_TOPMOST
                    | WS_EX_TOOLWINDOW
                    | WS_EX_NOACTIVATE,
                PCWSTR(class.as_ptr()),
                PCWSTR(class.as_ptr()),
                WS_POPUP,
                0,
                0,
                WIDTH as i32,
                HEIGHT as i32,
                None,
                None,
                Some(instance.into()),
                None,
            )
            .context("could not create the HUD window")?;
            let screen = GetDC(None);
            let memory = CreateCompatibleDC(Some(screen));
            let info = bitmap_info(WIDTH, HEIGHT);
            let mut bits: *mut c_void = std::ptr::null_mut();
            let bitmap = CreateDIBSection(Some(memory), &info, DIB_RGB_COLORS, &mut bits, None, 0)
                .context("could not create the HUD bitmap")?;
            let previous = SelectObject(memory, HGDIOBJ(bitmap.0));
            Ok(Box::new(Self {
                shared,
                painter,
                bars: Bars::new(),
                hwnd,
                screen,
                memory,
                bitmap,
                previous,
                bits: bits as *mut u8,
                opacity: 0,
                shown: View::Hidden,
                position: POINT::default(),
            }))
        }
    }

    fn tick(&mut self) {
        let view = self.shared.current();
        let level = self.shared.meter.take();
        if view == View::Hidden {
            if self.opacity == 0 {
                return;
            }
            self.opacity = self.opacity.saturating_sub(FADE_STEP);
            if self.opacity == 0 {
                unsafe {
                    let _ = ShowWindow(self.hwnd, SW_HIDE);
                }
                return;
            }
        } else {
            if self.opacity == 0 {
                self.position = window_position();
                self.bars = Bars::new();
                unsafe {
                    let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
                }
            }
            self.opacity = self.opacity.saturating_add(FADE_STEP);
            self.shown = view;
        }
        let shown = self.shown.clone();
        let bars = self.bars.step(&shown, level).to_vec();
        match self.painter.paint(&shown, &bars) {
            Ok(frame) => self.present(frame),
            Err(err) => tracing::warn!("could not draw the HUD: {err:#}"),
        }
    }

    fn present(&mut self, mut frame: Vec<u8>) {
        let fade = self.opacity as u32;
        for px in frame.as_chunks_mut::<4>().0 {
            let alpha = px[3] as u32 * fade / 255;
            for channel in &mut px[..3] {
                *channel = (*channel as u32 * alpha / 255) as u8;
            }
            px[3] = alpha as u8;
        }
        unsafe {
            std::ptr::copy_nonoverlapping(frame.as_ptr(), self.bits, frame.len());
            let size = SIZE {
                cx: WIDTH as i32,
                cy: HEIGHT as i32,
            };
            let blend = BLENDFUNCTION {
                BlendOp: 0,
                BlendFlags: 0,
                SourceConstantAlpha: 255,
                AlphaFormat: 1, // AC_SRC_ALPHA
            };
            let _ = UpdateLayeredWindow(
                self.hwnd,
                Some(self.screen),
                Some(&self.position),
                Some(&size),
                Some(self.memory),
                Some(&POINT::default()),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            );
        }
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        unsafe {
            SetWindowLongPtrW(self.hwnd, GWLP_USERDATA, 0);
            let _ = KillTimer(Some(self.hwnd), 1);
            let _ = DestroyWindow(self.hwnd);
            SelectObject(self.memory, self.previous);
            let _ = DeleteObject(HGDIOBJ(self.bitmap.0));
            let _ = DeleteDC(self.memory);
            ReleaseDC(None, self.screen);
        }
    }
}

/// Centre the HUD just above the taskbar of the primary screen.
fn window_position() -> POINT {
    let mut area = RECT::default();
    let ok = unsafe {
        SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some(&mut area as *mut RECT as *mut c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    }
    .is_ok();
    if !ok || area.right <= area.left {
        return POINT { x: 0, y: 0 };
    }
    POINT {
        x: area.left + ((area.right - area.left - WIDTH as i32) / 2).max(0),
        y: area.bottom - HEIGHT as i32 - 6,
    }
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_TIMER {
        let window = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Window;
        if let Some(window) = window.as_mut() {
            window.tick();
        }
        return LRESULT(0);
    }
    DefWindowProcW(hwnd, message, wparam, lparam)
}

fn run(shared: Arc<Shared>, ready: std::sync::mpsc::Sender<Result<u32>>) {
    let mut window = match Window::create(shared) {
        Ok(window) => window,
        Err(err) => {
            let _ = ready.send(Err(err));
            return;
        }
    };
    unsafe {
        SetWindowLongPtrW(
            window.hwnd,
            GWLP_USERDATA,
            &mut *window as *mut Window as isize,
        );
        SetTimer(Some(window.hwnd), 1, FRAME_MS, None);
    }
    let _ = ready.send(Ok(unsafe { GetCurrentThreadId() }));
    let mut msg = MSG::default();
    while unsafe { GetMessageW(&mut msg, None, 0, 0) }.0 > 0 {
        unsafe {
            DispatchMessageW(&msg);
        }
    }
    drop(window);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bgra_at(frame: &[u8], x: usize, y: usize) -> [u8; 4] {
        let p = (y * WIDTH + x) * 4;
        [frame[p], frame[p + 1], frame[p + 2], frame[p + 3]]
    }

    #[test]
    fn speech_levels_fill_the_bars() {
        assert_eq!(level_to_bar(0.0), 0.0);
        assert_eq!(level_to_bar(0.000_03), 0.0, "-90 dBFS room is flat");
        assert!(
            level_to_bar(0.03) > 0.5,
            "normal speech reaches half height"
        );
        assert!(level_to_bar(0.1) > 0.75);
        assert_eq!(level_to_bar(0.5), 1.0);
    }

    #[test]
    fn paints_panel_label_and_bars_in_bgra() {
        let mut painter = Painter::new().unwrap();
        let frame = painter.paint(&View::Listening, &[1.0; BAR_COUNT]).unwrap();
        assert_eq!(bgra_at(&frame, 0, 0)[3], 0, "corner is transparent");
        assert_eq!(
            bgra_at(&frame, 340, 20),
            [37, 29, 27, 255],
            "panel colour in BGRA"
        );
        assert_eq!(
            bgra_at(&frame, 44, 60),
            [108, 92, 255, 255],
            "red dot, not blue"
        );
        let label_ink = (51..73)
            .flat_map(|y| (67..240).map(move |x| (x, y)))
            .filter(|&(x, y)| bgra_at(&frame, x, y)[1] > 200)
            .count();
        assert!(label_ink > 300, "label is drawn");
        let error = painter
            .paint(&View::Error("INPUT BLOCKED".into()), &[0.0; BAR_COUNT])
            .unwrap();
        assert_ne!(frame, error);
    }

    /// Regenerate the README images: `cargo test -- --ignored readme_images`.
    #[test]
    #[ignore]
    fn readme_images() {
        let mut painter = Painter::new().unwrap();
        let speech = crate::transcribe::tests::speech();
        let mut bars = Bars::new();
        let mut levels = Vec::new();
        for slice in speech.chunks(528).skip(40).take(BAR_COUNT) {
            let rms = (slice.iter().map(|x| x * x).sum::<f32>() / slice.len() as f32).sqrt();
            levels = bars.step(&View::Listening, rms * 0.4).to_vec();
        }
        let shots = [
            ("hud-listening", View::Listening, levels),
            ("hud-transcribing", View::Transcribing, {
                let mut bars = Bars::new();
                bars.started -= std::time::Duration::from_millis(300);
                (0..12)
                    .map(|_| bars.step(&View::Transcribing, 0.0).to_vec())
                    .last()
                    .unwrap()
            }),
            (
                "hud-error",
                View::Error("NO SPEECH".into()),
                vec![0.06; BAR_COUNT],
            ),
        ];
        for (name, view, bars) in shots {
            let mut frame = painter.paint(&view, &bars).unwrap();
            for px in frame.as_chunks_mut::<4>().0 {
                px.swap(0, 2); // BGRA to RGBA
            }
            let path = format!("{}/docs/assets/{name}.png", env!("CARGO_MANIFEST_DIR"));
            let file = std::io::BufWriter::new(std::fs::File::create(&path).unwrap());
            let mut encoder = png::Encoder::new(file, WIDTH as u32, HEIGHT as u32);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            encoder
                .write_header()
                .unwrap()
                .write_image_data(&frame)
                .unwrap();
        }
    }
}
