//! FastTTS: hold Caps Lock, speak English, release. Nothing else.

#![windows_subsystem = "windows"]

mod audio;
mod cleanup;
mod config;
mod hook;
mod hud;
mod inject;
mod transcribe;
mod tray;

use anyhow::{anyhow, Result};
use audio::Recorder;
use config::Config;
use crossbeam_channel::{bounded, Receiver, Sender};
use hook::HotkeyHook;
use hud::{Hud, HudMode};
use std::{
    process::Command,
    sync::atomic::{AtomicBool, Ordering},
    sync::Arc,
    thread,
    time::Instant,
};
use tracing::info;
use transcribe::Transcriber;
use tray::{Tray, TRAY_MENU_LOG, TRAY_MENU_PAUSE, TRAY_MENU_QUIT};
use windows::{
    core::PCWSTR,
    Win32::{
        Foundation::{GetLastError, ERROR_ALREADY_EXISTS, HWND, LPARAM, LRESULT, WPARAM},
        System::Threading::CreateMutexW,
        UI::WindowsAndMessaging::{
            CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, GetWindowLongPtrW,
            PostQuitMessage, RegisterClassExW, SetTimer, SetWindowLongPtrW, TranslateMessage,
            GWLP_USERDATA, HWND_MESSAGE, MSG, WM_APP, WM_COMMAND, WM_DESTROY, WM_LBUTTONUP,
            WM_RBUTTONUP, WM_TIMER, WNDCLASSEXW,
        },
    },
};

const WM_TRAY: u32 = WM_APP + 1;
const WM_HOTKEY: u32 = WM_APP + 2;

/// Drop leading/trailing quiet frames so chunk boundaries do not carry noise.
fn trim_silence(samples: &[f32]) -> &[f32] {
    const FRAME: usize = 160; // 10 ms at 16 kHz
    const THRESHOLD: f32 = 0.006;
    if samples.len() < FRAME * 2 {
        return samples;
    }
    let rms = |part: &[f32]| (part.iter().map(|x| x * x).sum::<f32>() / part.len() as f32).sqrt();
    let mut start = 0;
    while start + FRAME <= samples.len() && rms(&samples[start..start + FRAME]) < THRESHOLD {
        start += FRAME;
    }
    let mut end = samples.len();
    while end >= start + FRAME * 2 && rms(&samples[end - FRAME..end]) < THRESHOLD {
        end -= FRAME;
    }
    if end > start {
        &samples[start..end]
    } else {
        samples
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    Loading,
    Ready,
    Recording,
    Transcribing,
    Paused,
    Error,
}

enum WorkerEvent {
    Ready,
    Transcript {
        generation: u64,
        result: Result<String, String>,
    },
}

struct TranscribeJob {
    generation: u64,
    audio: Vec<f32>,
}

struct App {
    config: Config,
    recorder: Arc<Recorder>,
    hud: Arc<Hud>,
    tray: Tray,
    paused: Arc<AtomicBool>,
    jobs: Sender<TranscribeJob>,
    results: Receiver<WorkerEvent>,
    state: State,
    started_at: Instant,
    model_ready: bool,
    generation: u64,
    pending: usize,
    released: bool,
    partials: Vec<String>,
    chunk: Vec<f32>,
    cursor: usize,
    heard_speech: bool,
    silence_at: Option<Instant>,
}

impl App {
    fn set_state(&mut self, state: State, detail: &str) {
        self.state = state;
        let status = match state {
            State::Loading => "Loading English model...".to_string(),
            State::Ready => "Ready - hold Caps Lock to dictate".to_string(),
            State::Recording => "Listening...".to_string(),
            State::Transcribing => "Transcribing...".to_string(),
            State::Paused => "Paused - Caps Lock works normally".to_string(),
            State::Error => detail.to_string(),
        };
        self.tray
            .set_status(&status, self.paused.load(Ordering::Acquire));
        match state {
            State::Recording => self.hud.set_mode(HudMode::Recording, ""),
            State::Transcribing => self.hud.set_mode(HudMode::Transcribing, ""),
            State::Error => self.hud.set_mode(HudMode::Error, detail),
            State::Loading | State::Ready | State::Paused => self.hud.hide(),
        }
    }

    fn caps_down(&mut self) {
        if self.paused.load(Ordering::Acquire) || self.recorder.is_active() {
            return;
        }
        self.reset_take();
        self.recorder.start_take();
        self.started_at = Instant::now();
        self.set_state(State::Recording, "");
        info!("recording started");
    }

    fn reset_take(&mut self) {
        self.generation += 1;
        self.pending = 0;
        self.released = false;
        self.partials.clear();
        self.chunk.clear();
        self.cursor = 0;
        self.heard_speech = false;
        self.silence_at = None;
    }

    fn flush_chunk(&mut self) {
        let audio = trim_silence(&self.chunk).to_vec();
        self.chunk.clear();
        self.silence_at = None;
        if audio.len() < 1600 {
            return;
        }
        let job = TranscribeJob {
            generation: self.generation,
            audio,
        };
        if self.jobs.send(job).is_ok() {
            self.pending += 1;
        }
    }

    /// Transcribe complete phrases while the key is still held, so release
    /// only has to finish the short tail.
    fn tick_streaming(&mut self) {
        if !self.recorder.is_active() || !self.model_ready {
            return;
        }
        let new_samples = self.recorder.new_chunk(&mut self.cursor);
        self.chunk.extend_from_slice(&new_samples);

        let level = self.recorder.level();
        if level > 0.012 {
            self.heard_speech = true;
            self.silence_at = None;
        } else if self.heard_speech && self.silence_at.is_none() {
            self.silence_at = Some(Instant::now());
        }

        let quiet_long_enough = self
            .silence_at
            .map(|at| at.elapsed().as_millis() >= self.config.silence_ms as u128)
            .unwrap_or(false);
        let min_ok =
            self.chunk.len() >= (self.config.min_chunk_seconds * 16_000.0) as usize;
        let max_reached =
            self.chunk.len() >= (self.config.chunk_seconds * 16_000.0) as usize;
        if (quiet_long_enough && min_ok) || max_reached {
            self.flush_chunk();
        }
    }

    fn caps_up(&mut self) {
        if !self.recorder.is_active() {
            return;
        }
        let tail = self.recorder.stop_take_from(self.cursor);
        self.chunk.extend_from_slice(&tail);
        let held = self.started_at.elapsed();
        info!("recording stopped: {:.2}s", held.as_secs_f32());
        if held.as_millis() < self.config.min_hold_ms as u128 || self.chunk.len() < 1600 {
            self.reset_take();
            self.set_state(
                if self.model_ready {
                    State::Ready
                } else {
                    State::Loading
                },
                "",
            );
            return;
        }
        self.flush_chunk();
        self.released = true;
        if self.pending == 0 {
            self.finish_take();
        } else {
            self.set_state(State::Transcribing, "");
        }
    }

    fn finish_take(&mut self) {
        let joined = crate::cleanup::clean_transcript(&self.partials.join(" "));
        self.reset_take();
        if joined.trim().is_empty() {
            self.set_state(State::Error, "NO SPEECH");
            return;
        }
        if self.config.inject != "none" {
            if let Err(err) = inject::insert_text(
                &joined,
                &self.config.inject,
                self.config.char_delay_ms,
                self.config.paste_delay_ms,
                self.config.append_space,
            ) {
                tracing::error!("could not insert transcript: {err}");
                self.set_state(State::Error, "INPUT BLOCKED");
                return;
            }
        }
        self.set_state(State::Ready, "");
    }

    fn poll_results(&mut self) {
        while let Ok(event) = self.results.try_recv() {
            match event {
                WorkerEvent::Ready => {
                    self.model_ready = true;
                    if self.state == State::Loading {
                        self.set_state(State::Ready, "");
                    }
                }
                WorkerEvent::Transcript { generation, .. } if generation != self.generation => {
                    // Stale result from a previous take; ignore it.
                    continue;
                }
                WorkerEvent::Transcript { result, .. } => {
                    self.pending = self.pending.saturating_sub(1);
                    match result {
                        Ok(text) => {
                            if !text.trim().is_empty() {
                                self.partials.push(text);
                            }
                        }
                        Err(err) => {
                            tracing::error!("transcription failed: {err}");
                            self.reset_take();
                            self.set_state(State::Error, "TRANSCRIPTION ERROR");
                            return;
                        }
                    }
                    if self.released && self.pending == 0 {
                        self.finish_take();
                    }
                }
            }
        }
    }

    fn toggle_pause(&mut self) {
        let now_paused = !self.paused.load(Ordering::Acquire);
        self.paused.store(now_paused, Ordering::Release);
        if now_paused {
            self.recorder.cancel();
            self.reset_take();
            self.set_state(State::Paused, "");
        } else {
            self.set_state(
                if self.model_ready {
                    State::Ready
                } else {
                    State::Loading
                },
                "",
            );
        }
    }

    fn shutdown(&mut self) {
        self.recorder.cancel();
        self.recorder.close();
        self.hud.hide();
        self.tray.remove();
    }
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    message: u32,
    w_param: WPARAM,
    l_param: LPARAM,
) -> LRESULT {
    if message == WM_HOTKEY {
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut App;
        if !ptr.is_null() {
            let app = &mut *ptr;
            if w_param.0 == 1 {
                app.caps_down();
            } else {
                app.caps_up();
            }
        }
        return LRESULT(0);
    }
    if message == WM_TRAY {
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut App;
        if !ptr.is_null() {
            let msg = l_param.0 as u32 & 0xffff;
            if msg == WM_RBUTTONUP || msg == WM_LBUTTONUP {
                (&mut *ptr).tray.show_menu();
            }
        }
        return LRESULT(0);
    }
    if message == WM_COMMAND {
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut App;
        if !ptr.is_null() {
            let app = &mut *ptr;
            match w_param.0 as u32 & 0xffff {
                id if id == TRAY_MENU_PAUSE as u32 => app.toggle_pause(),
                id if id == TRAY_MENU_LOG as u32 => {
                    let _ = Command::new("explorer").arg(config::log_dir()).spawn();
                }
                id if id == TRAY_MENU_QUIT as u32 => {
                    app.shutdown();
                    PostQuitMessage(0);
                }
                _ => {}
            }
        }
        return LRESULT(0);
    }
    if message == WM_TIMER {
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut App;
        if !ptr.is_null() {
            let app = &mut *ptr;
            app.poll_results();
            app.tick_streaming();
            if app.recorder.is_active() {
                app.hud.push_level(app.recorder.level());
            }
        }
        return LRESULT(0);
    }
    if message == WM_DESTROY {
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut App;
        if !ptr.is_null() {
            let app = &mut *ptr;
            app.shutdown();
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            drop(Box::from_raw(ptr));
        }
        PostQuitMessage(0);
        return LRESULT(0);
    }
    DefWindowProcW(hwnd, message, w_param, l_param)
}

fn create_window() -> Result<HWND> {
    let class_name: Vec<u16> = "FastTTSHiddenWindow\0".encode_utf16().collect();
    let instance = unsafe { windows::Win32::System::LibraryLoader::GetModuleHandleW(None)? };
    let mut class: WNDCLASSEXW = unsafe { std::mem::zeroed() };
    class.cbSize = std::mem::size_of::<WNDCLASSEXW>() as u32;
    class.style = windows::Win32::UI::WindowsAndMessaging::WNDCLASS_STYLES(0);
    class.lpfnWndProc = Some(wnd_proc);
    class.hInstance = instance.into();
    class.lpszClassName = windows::core::PCWSTR(class_name.as_ptr());
    let atom = unsafe { RegisterClassExW(&class) };
    if atom == 0 {
        let error = unsafe { GetLastError() };
        if error != windows::Win32::Foundation::ERROR_CLASS_ALREADY_EXISTS {
            return Err(anyhow!("could not register window class: {error:?}"));
        }
    }
    let hwnd = unsafe {
        CreateWindowExW(
            Default::default(),
            windows::core::PCWSTR(class_name.as_ptr()),
            PCWSTR::null(),
            Default::default(),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(instance.into()),
            None,
        )?
    };
    Ok(hwnd)
}

fn setup_logging() {
    let dir = config::log_dir();
    let _ = std::fs::create_dir_all(&dir);
    let appender = tracing_appender::rolling::daily(dir, "fasttts.log");
    tracing_subscriber::fmt()
        .with_writer(appender)
        .with_ansi(false)
        .init();
}

fn single_instance() -> bool {
    unsafe {
        let name: Vec<u16> = "Local\\FastTTS-rust-single-instance\0"
            .encode_utf16()
            .collect();
        let handle = CreateMutexW(None, false, PCWSTR(name.as_ptr()));
        match handle {
            Ok(handle) => {
                let _ = handle;
                !matches!(GetLastError(), ERROR_ALREADY_EXISTS)
            }
            Err(_) => false,
        }
    }
}

fn run() -> Result<()> {
    setup_logging();
    info!("FastTTS starting");
    if !single_instance() {
        return Err(anyhow!("FastTTS is already running"));
    }
    let config = Config::load()?;
    let root = config::project_root();
    let model = config.model_path();
    anyhow::ensure!(
        model.is_file(),
        "the bundled English model is missing: {}",
        model.display()
    );

    let hwnd = create_window()?;
    let recorder = Arc::new(Recorder::new(config.max_record_seconds));
    if config.hot_mic {
        recorder.open(&config.mic_device)?;
    }
    let hud = Arc::new(Hud::new(&root)?);
    let tray = Tray::new(hwnd, &root.join("assets/fasttts.ico"))?;
    let paused = Arc::new(AtomicBool::new(false));
    let _hook = HotkeyHook::start(hwnd, paused.clone())?;

    let (job_tx, job_rx) = bounded::<TranscribeJob>(16);
    let (result_tx, result_rx) = bounded::<WorkerEvent>(8);
    let (model_tx, model_rx) = bounded::<Result<Transcriber, String>>(1);

    let model_path = model.clone();
    let threads = config.threads;
    let prompt = config.initial_prompt.clone();
    let cleanup = config.cleanup;
    thread::spawn(move || {
        let outcome = Transcriber::new(&model_path, threads, &prompt, cleanup)
            .map_err(|err| err.to_string());
        let _ = model_tx.send(outcome);
    });

    let result_worker = result_tx.clone();
    thread::spawn(move || {
        let mut transcriber = match model_rx.recv() {
            Ok(Ok(transcriber)) => transcriber,
            Ok(Err(err)) => {
                let _ = result_worker.send(WorkerEvent::Transcript {
                    generation: 0,
                    result: Err(err),
                });
                return;
            }
            Err(_) => return,
        };
        info!("English model ready");
        if result_worker
            .send(WorkerEvent::Ready)
            .is_err()
        {
            return;
        }
        for job in job_rx.iter() {
            let result = transcriber
                .transcribe(&job.audio)
                .map_err(|err| err.to_string());
            if result_worker
                .send(WorkerEvent::Transcript {
                    generation: job.generation,
                    result,
                })
                .is_err()
            {
                break;
            }
        }
    });

    let mut app = Box::new(App {
        config,
        recorder,
        hud,
        tray,
        paused,
        jobs: job_tx,
        results: result_rx,
        state: State::Loading,
        started_at: Instant::now(),
        model_ready: false,
        generation: 0,
        pending: 0,
        released: false,
        partials: Vec::new(),
        chunk: Vec::new(),
        cursor: 0,
        heard_speech: false,
        silence_at: None,
    });
    app.set_state(State::Loading, "");
    unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, (&mut *app as *mut App) as isize) };
    unsafe { SetTimer(Some(hwnd), 1, 33, None) };

    let mut message = MSG::default();
    while unsafe { GetMessageW(&mut message, None, 0, 0) }.as_bool() {
        unsafe {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
    // The window's WM_DESTROY handler already freed App.
    Ok(())
}

fn main() {
    if let Err(err) = run() {
        tracing::error!("FastTTS could not start: {err:#}");
        let text = format!("FastTTS could not start:\n\n{err:#}");
        let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        let caption: Vec<u16> = "FastTTS\0".encode_utf16().collect();
        unsafe {
            windows::Win32::UI::WindowsAndMessaging::MessageBoxW(
                None,
                PCWSTR(wide.as_ptr()),
                PCWSTR(caption.as_ptr()),
                windows::Win32::UI::WindowsAndMessaging::MB_ICONERROR,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::trim_silence;

    #[test]
    fn trim_silence_keeps_the_spoken_middle() {
        let mut samples = vec![0.0; 1600];
        samples.extend((0..3200).map(|i| (i as f32 / 3200.0).sin() * 0.2));
        samples.extend(vec![0.0; 1600]);
        let trimmed = trim_silence(&samples);
        assert!(trimmed.len() >= 3000);
        assert!(trimmed.len() < samples.len());
    }
}
