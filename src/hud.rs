//! Always-on-top, click-through HUD drawn with a Win32 layered window.
//!
//! Three pre-rendered, transparent PNGs (from `docs/assets`) supply the panel
//! and label for each state.  A dedicated thread owns the window, pumps a
//! `GetMessageW` loop and redraws at ~30 fps: it overlays live equalizer bars
//! in the empty strip of the PNG, applies a fade, premultiplies alpha and
//! pushes the result through `UpdateLayeredWindow`.  Callers only ever touch
//! thread-safe atomics/a mutex, so no GUI work happens on their threads.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Instant;

use anyhow::{bail, Context, Result};
use image::RgbaImage;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, ReleaseDC, SelectObject,
    BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION, DIB_RGB_COLORS, HBITMAP, HDC, HGDIOBJ,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
    GetWindowLongPtrW, KillTimer, PostQuitMessage, PostThreadMessageW, RegisterClassExW, SetTimer,
    SetWindowLongPtrW, ShowWindow, SystemParametersInfoW, TranslateMessage, UpdateLayeredWindow,
    GWLP_USERDATA, SPI_GETWORKAREA, SW_HIDE, SW_SHOWNOACTIVATE,
    SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, ULW_ALPHA, WM_CLOSE, WM_DESTROY, WM_QUIT, WM_TIMER,
    WNDCLASSEXW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    WS_EX_TRANSPARENT, WS_POPUP,
};

const TIMER_ID: usize = 1;
const TIMER_MS: u32 = 33;
const FADE_STEP: u8 = 55;
const BAR_COUNT: usize = 38;
const BAR_TOP: usize = 78;
const BAR_BOTTOM: usize = 128;
const ERROR_TIMEOUT_SECS: f64 = 2.4;

/// Which pre-rendered panel to show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HudMode {
    Recording,
    Transcribing,
    Error,
}

impl HudMode {
    fn image_index(self) -> usize {
        match self {
            HudMode::Recording => 0,
            HudMode::Transcribing => 1,
            HudMode::Error => 2,
        }
    }

    fn accent(self) -> [u8; 3] {
        match self {
            HudMode::Recording => [255, 92, 108],
            HudMode::Transcribing => [255, 180, 84],
            HudMode::Error => [248, 113, 113],
        }
    }
}

struct HudState {
    mode: Option<HudMode>,
    detail: String,
    changed: Instant,
}

struct HudShared {
    /// Current microphone RMS, as `f32` bits (written by `push_level`).
    level: AtomicU32,
    state: Mutex<HudState>,
    thread_id: AtomicU32,
}

fn lock_state(shared: &HudShared) -> std::sync::MutexGuard<'_, HudState> {
    shared.state.lock().unwrap_or_else(|err| err.into_inner())
}

/// Thread-safe facade over the HUD window.
pub struct Hud {
    shared: Arc<HudShared>,
    thread: Option<JoinHandle<()>>,
}

impl Hud {
    /// Load the three HUD panels and start the window thread.
    pub fn new(root: &std::path::Path) -> Result<Self> {
        let assets = root.join("docs").join("assets");
        let listening = load_panel(&assets.join("hud-listening.png"))?;
        let transcribing = load_panel(&assets.join("hud-transcribing.png"))?;
        let error = load_panel(&assets.join("hud-error.png"))?;

        let expected = listening.dimensions();
        if transcribing.dimensions() != expected || error.dimensions() != expected {
            bail!("all HUD panels must have the same pixel dimensions");
        }
        let (width, height) = expected;
        if width == 0 || height == 0 {
            bail!("HUD panels have no pixels");
        }

        let shared = Arc::new(HudShared {
            level: AtomicU32::new(0.0_f32.to_bits()),
            state: Mutex::new(HudState {
                mode: None,
                detail: String::new(),
                changed: Instant::now(),
            }),
            thread_id: AtomicU32::new(0),
        });

        let (tx, rx) = std::sync::mpsc::channel::<Result<()>>();
        let thread_shared = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("fasttts-hud".into())
            .spawn(move || {
                hud_thread(
                    thread_shared,
                    [listening, transcribing, error],
                    width,
                    height,
                    tx,
                )
            })
            .map_err(|e| anyhow::anyhow!("could not spawn HUD thread: {e}"))?;

        match rx.recv() {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                let _ = thread.join();
                return Err(err);
            }
            Err(err) => {
                let _ = thread.join();
                bail!("HUD thread stopped before creating its window: {err}");
            }
        }

        Ok(Self {
            shared,
            thread: Some(thread),
        })
    }

    /// Show a state. `detail` is stored for the application; the visible label
    /// comes from the pre-rendered panel.
    pub fn set_mode(&self, mode: HudMode, detail: &str) {
        let mut state = lock_state(&self.shared);
        state.mode = Some(mode);
        state.detail = detail.to_string();
        state.changed = Instant::now();
        // A new mode starts with a fresh waveform history.
        drop(state);
        self.shared
            .level
            .store(0.0_f32.to_bits(), Ordering::Relaxed);
    }

    /// Feed a microphone RMS (0.0..=1.0). Called from the audio thread.
    pub fn push_level(&self, rms: f32) {
        let clamped = if rms.is_nan() {
            0.0
        } else {
            rms.clamp(0.0, 1.0)
        };
        self.shared
            .level
            .store(clamped.to_bits(), Ordering::Relaxed);
    }

    /// Fade the HUD out.
    pub fn hide(&self) {
        let mut state = lock_state(&self.shared);
        state.mode = None;
        state.changed = Instant::now();
    }

}

impl Drop for Hud {
    fn drop(&mut self) {
        let thread_id = self.shared.thread_id.load(Ordering::SeqCst);
        if thread_id != 0 {
            unsafe {
                let _ = PostThreadMessageW(thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
            }
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn load_panel(path: &std::path::Path) -> Result<RgbaImage> {
    let image = image::open(path)
        .with_context(|| format!("could not load HUD panel {}", path.display()))?;
    Ok(image.to_rgba8())
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Everything owned by the HUD thread. A pointer to this struct is stored in
/// `GWLP_USERDATA` so the window procedure can drive the animation.
struct RenderCtx {
    shared: Arc<HudShared>,
    images: [RgbaImage; 3],
    width: u32,
    height: u32,
    hwnd: HWND,
    position_x: i32,
    position_y: i32,
    screen_dc: HDC,
    mem_dc: HDC,
    bitmap: HBITMAP,
    old_object: HGDIOBJ,
    bits: *mut u8,
    started: Instant,
    opacity: u8,
    visible: bool,
    levels: VecDeque<f32>,
    display: Vec<f32>,
    last_mode: usize,
    cleaned: bool,
}

impl RenderCtx {
    fn create(
        shared: Arc<HudShared>,
        images: [RgbaImage; 3],
        width: u32,
        height: u32,
    ) -> Result<Self> {
        unsafe {
            let instance = GetModuleHandleW(None).context("GetModuleHandleW failed")?;
            let class_name = wide("FastTTS_HUD_Window");
            let title = wide("FastTTS HUD");

            let mut wc = WNDCLASSEXW::default();
            wc.cbSize = std::mem::size_of::<WNDCLASSEXW>() as u32;
            wc.lpfnWndProc = Some(hud_wndproc);
            wc.hInstance = instance.into();
            wc.lpszClassName = PCWSTR::from_raw(class_name.as_ptr());
            // A duplicate registration (from a previous Hud instance) is fine.
            let _ = RegisterClassExW(&wc);

            let ex_style = WS_EX_LAYERED
                | WS_EX_TRANSPARENT
                | WS_EX_TOPMOST
                | WS_EX_TOOLWINDOW
                | WS_EX_NOACTIVATE;
            let (x, y) = window_position(width as i32, height as i32);
            let hwnd = CreateWindowExW(
                ex_style,
                PCWSTR::from_raw(class_name.as_ptr()),
                PCWSTR::from_raw(title.as_ptr()),
                WS_POPUP,
                x,
                y,
                width as i32,
                height as i32,
                None,
                None,
                Some(instance.into()),
                None,
            )
            .context("could not create the HUD window")?;

            let screen_dc = GetDC(None);
            let mem_dc = CreateCompatibleDC(Some(screen_dc));

            let mut bmi = BITMAPINFO::default();
            bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
            bmi.bmiHeader.biWidth = width as i32;
            bmi.bmiHeader.biHeight = -(height as i32); // top-down DIB
            bmi.bmiHeader.biPlanes = 1;
            bmi.bmiHeader.biBitCount = 32;
            bmi.bmiHeader.biCompression = 0; // BI_RGB

            let mut raw_bits: *mut c_void = std::ptr::null_mut();
            let bitmap =
                CreateDIBSection(Some(mem_dc), &bmi, DIB_RGB_COLORS, &mut raw_bits, None, 0)
                    .context("CreateDIBSection failed")?;
            let old_object = SelectObject(mem_dc, HGDIOBJ::from(bitmap));

            Ok(Self {
                shared,
                images,
                width,
                height,
                hwnd,
                position_x: x,
                position_y: y,
                screen_dc,
                mem_dc,
                bitmap,
                old_object,
                bits: raw_bits as *mut u8,
                started: Instant::now(),
                opacity: 0,
                visible: false,
                levels: VecDeque::with_capacity(48),
                display: Vec::new(),
                last_mode: 0,
                cleaned: false,
            })
        }
    }

    fn cleanup_gdi(&mut self) {
        if self.cleaned {
            return;
        }
        self.cleaned = true;
        unsafe {
            if !self.old_object.0.is_null() {
                let _ = SelectObject(self.mem_dc, self.old_object);
            }
            if !self.bitmap.0.is_null() {
                let _ = DeleteObject(HGDIOBJ::from(self.bitmap));
            }
            if !self.mem_dc.0.is_null() {
                let _ = DeleteDC(self.mem_dc);
            }
            if !self.screen_dc.0.is_null() {
                let _ = ReleaseDC(None, self.screen_dc);
            }
        }
    }

    fn tick(&mut self) {
        let now = Instant::now();

        // Error states dismiss themselves after a short moment.
        {
            let mut state = lock_state(&self.shared);
            if state.mode == Some(HudMode::Error)
                && now.duration_since(state.changed).as_secs_f64() > ERROR_TIMEOUT_SECS
            {
                state.mode = None;
            }
        }

        let mode = lock_state(&self.shared).mode;
        let level = f32::from_bits(self.shared.level.load(Ordering::Relaxed));
        let level = if level.is_nan() { 0.0 } else { level };

        if mode == Some(HudMode::Recording) {
            self.levels.push_back(level);
            if self.levels.len() > 48 {
                self.levels.pop_front();
            }
        } else {
            self.levels.clear();
        }

        if mode.is_none() {
            if self.visible {
                self.opacity = self.opacity.saturating_sub(FADE_STEP);
                if self.opacity == 0 {
                    self.visible = false;
                    unsafe {
                        let _ = ShowWindow(self.hwnd, SW_HIDE);
                    }
                } else {
                    self.present(self.last_mode, now);
                }
            }
            return;
        }

        let index = mode.unwrap_or(HudMode::Recording).image_index();
        self.last_mode = index;
        if !self.visible {
            self.visible = true;
            self.opacity = 0;
            unsafe {
                let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
            }
        }
        self.opacity = self.opacity.saturating_add(FADE_STEP).min(255);
        self.present(index, now);
    }

    fn bar_heights(&mut self, mode: HudMode, now: Instant) -> Vec<f32> {
        let t = now.duration_since(self.started).as_secs_f64();
        let max_height = (BAR_BOTTOM - BAR_TOP) as f32;
        let mut targets = Vec::with_capacity(BAR_COUNT);

        match mode {
            HudMode::Recording => {
                let len = self.levels.len();
                for i in 0..BAR_COUNT {
                    let value = if len == 0 {
                        0.0
                    } else {
                        let src = i * (len - 1) / (BAR_COUNT - 1);
                        self.levels[src]
                    };
                    targets.push(0.06 + (value.powf(0.7).min(1.0)) * 0.94);
                }
            }
            HudMode::Transcribing => {
                let position = (t * 1.15 % 1.0) * (BAR_COUNT - 1) as f64;
                for i in 0..BAR_COUNT {
                    let distance = (i as f64 - position) / 2.6;
                    let pulse = (-distance * distance).exp();
                    targets.push(
                        (0.05 + 0.55 * pulse + 0.04 * (t * 5.0 + i as f64 * 0.5).sin()) as f32,
                    );
                }
            }
            HudMode::Error => {
                for i in 0..BAR_COUNT {
                    targets.push((0.06 + 0.04 * (t * 8.0 + i as f64).sin()) as f32);
                }
            }
        }

        if self.display.len() != BAR_COUNT {
            self.display = targets;
        } else {
            for (current, target) in self.display.iter_mut().zip(targets) {
                *current += (target - *current) * 0.42;
            }
        }

        self.display
            .iter()
            .map(|value| (value.max(0.04) * max_height).max(3.0))
            .collect()
    }

    fn present(&mut self, index: usize, now: Instant) {
        if self.bits.is_null() {
            return;
        }
        let mode = match index {
            0 => HudMode::Recording,
            1 => HudMode::Transcribing,
            _ => HudMode::Error,
        };
        let accent = mode.accent();
        let w = self.width as usize;
        let h = self.height as usize;

        let base = &self.images[index];
        let mut frame = base.as_raw().to_vec();
        if frame.len() != w * h * 4 {
            return;
        }

        let heights = self.bar_heights(mode, now);
        let center = ((BAR_TOP + BAR_BOTTOM) / 2) as isize;
        let left_edge = (w / 20).max(8);
        let span = w.saturating_sub(2 * left_edge);
        let bar_width = 3usize;

        for (i, height) in heights.iter().enumerate() {
            let cx = left_edge + (i + 1) * span / (BAR_COUNT + 1);
            let half = (*height / 2.0) as isize;
            let top = (center - half).max(BAR_TOP as isize) as usize;
            let bottom = (center + half).min((BAR_BOTTOM - 1) as isize) as usize;
            let x0 = cx.saturating_sub(bar_width / 2).min(w.saturating_sub(1));
            let x1 = (x0 + bar_width).min(w);
            for y in top..=bottom.min(h - 1) {
                for pix in (y * w + x0)..(y * w + x1) {
                    let p = pix * 4;
                    frame[p] = accent[0];
                    frame[p + 1] = accent[1];
                    frame[p + 2] = accent[2];
                    frame[p + 3] = 255;
                }
            }
        }

        // Premultiply RGB by the final alpha (per-pixel * fade) as required by
        // AC_SRC_ALPHA, then hand the DIB section to the layered window.
        let fade = self.opacity as u32;
        for pixel in frame.chunks_exact_mut(4) {
            let alpha = (pixel[3] as u32 * fade / 255) as u8;
            let a = alpha as u32;
            pixel[0] = (pixel[0] as u32 * a / 255) as u8;
            pixel[1] = (pixel[1] as u32 * a / 255) as u8;
            pixel[2] = (pixel[2] as u32 * a / 255) as u8;
            pixel[3] = alpha;
        }

        unsafe {
            std::ptr::copy_nonoverlapping(frame.as_ptr(), self.bits, frame.len());
            let size = SIZE {
                cx: self.width as i32,
                cy: self.height as i32,
            };
            let destination = POINT {
                x: self.position_x,
                y: self.position_y,
            };
            let source = POINT { x: 0, y: 0 };
            let blend = BLENDFUNCTION {
                BlendOp: 0,
                BlendFlags: 0,
                SourceConstantAlpha: 255,
                AlphaFormat: 1, // AC_SRC_ALPHA
            };
            let _ = UpdateLayeredWindow(
                self.hwnd,
                Some(self.screen_dc),
                Some(&destination),
                Some(&size),
                Some(self.mem_dc),
                Some(&source),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            );
        }
    }
}

/// Center the HUD horizontally in the work area, just above the taskbar.
fn window_position(width: i32, height: i32) -> (i32, i32) {
    let mut area = RECT::default();
    let ok = unsafe {
        SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some(&mut area as *mut RECT as *mut c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    };
    let (left, right, bottom) = if ok.is_ok() && area.right > area.left {
        (area.left, area.right, area.bottom)
    } else {
        (0, width, height + 6)
    };
    let x = left + ((right - left - width) / 2).max(0);
    let y = bottom - height - 6;
    (x, y)
}

unsafe extern "system" fn hud_wndproc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_TIMER {
        let ctx = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut RenderCtx;
        if !ctx.is_null() {
            (*ctx).tick();
        }
        return LRESULT(0);
    }
    if message == WM_CLOSE {
        let _ = DestroyWindow(hwnd);
        return LRESULT(0);
    }
    if message == WM_DESTROY {
        let _ = KillTimer(Some(hwnd), TIMER_ID);
        PostQuitMessage(0);
        return LRESULT(0);
    }
    DefWindowProcW(hwnd, message, wparam, lparam)
}

fn hud_thread(
    shared: Arc<HudShared>,
    images: [RgbaImage; 3],
    width: u32,
    height: u32,
    ready: std::sync::mpsc::Sender<Result<()>>,
) {
    let mut ctx = match RenderCtx::create(shared, images, width, height) {
        Ok(ctx) => ctx,
        Err(err) => {
            let _ = ready.send(Err(err));
            return;
        }
    };

    unsafe {
        SetWindowLongPtrW(
            ctx.hwnd,
            GWLP_USERDATA,
            (&mut ctx as *mut RenderCtx) as isize,
        );
        SetTimer(Some(ctx.hwnd), TIMER_ID, TIMER_MS, None);
    }

    ctx.shared
        .thread_id
        .store(unsafe { GetCurrentThreadId() }, Ordering::SeqCst);
    let _ = ready.send(Ok(()));

    let mut msg = windows::Win32::UI::WindowsAndMessaging::MSG::default();
    loop {
        let ret = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        if ret.0 <= 0 {
            break;
        }
        unsafe {
            let _ = TranslateMessage(&msg);
            let _ = DispatchMessageW(&msg);
        }
    }

    unsafe {
        let _ = DestroyWindow(ctx.hwnd);
    }
    ctx.cleanup_gdi();
}
