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
    Transcript(Result<String, String>),
}

struct App {
    config: Config,
    recorder: Arc<Recorder>,
    hud: Arc<Hud>,
    tray: Tray,
    paused: Arc<AtomicBool>,
    jobs: Sender<Vec<f32>>,
    results: Receiver<WorkerEvent>,
    state: State,
    started_at: Instant,
    model_ready: bool,
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
        self.recorder.start_take();
        self.started_at = Instant::now();
        self.set_state(State::Recording, "");
        info!("recording started");
    }

    fn caps_up(&mut self) {
        if !self.recorder.is_active() {
            return;
        }
        let audio = self.recorder.stop_take();
        let held = self.started_at.elapsed();
        info!("recording stopped: {:.2}s", audio.len() as f32 / 16_000.0);
        if held.as_millis() < self.config.min_hold_ms as u128 || audio.len() < 1600 {
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
        if self.jobs.send(audio).is_ok() {
            self.set_state(State::Transcribing, "");
        }
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
                WorkerEvent::Transcript(Ok(message)) if message.trim().is_empty() => {
                    self.set_state(State::Error, "NO SPEECH");
                }
                WorkerEvent::Transcript(Ok(message)) => {
                    if self.config.inject != "none" {
                        if let Err(err) = inject::insert_text(
                            &message,
                            &self.config.inject,
                            self.config.char_delay_ms,
                            self.config.paste_delay_ms,
                            self.config.append_space,
                        ) {
                            tracing::error!("could not insert transcript: {err}");
                            self.set_state(State::Error, "INPUT BLOCKED");
                            continue;
                        }
                    }
                    self.set_state(State::Ready, "");
                }
                WorkerEvent::Transcript(Err(err)) => {
                    tracing::error!("transcription failed: {err}");
                    self.set_state(State::Error, "TRANSCRIPTION ERROR");
                }
            }
        }
    }

    fn toggle_pause(&mut self) {
        let now_paused = !self.paused.load(Ordering::Acquire);
        self.paused.store(now_paused, Ordering::Release);
        if now_paused {
            self.recorder.cancel();
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

    let (job_tx, job_rx) = bounded::<Vec<f32>>(8);
    let (result_tx, result_rx) = bounded::<WorkerEvent>(8);
    let (model_tx, model_rx) = bounded::<Result<Arc<Transcriber>, String>>(1);

    let model_path = model.clone();
    let threads = config.threads;
    let prompt = config.initial_prompt.clone();
    let cleanup = config.cleanup;
    thread::spawn(move || {
        let outcome = Transcriber::new(&model_path, threads, &prompt, cleanup)
            .map(Arc::new)
            .map_err(|err| err.to_string());
        let _ = model_tx.send(outcome);
    });

    let result_worker = result_tx.clone();
    thread::spawn(move || {
        let transcriber = match model_rx.recv() {
            Ok(Ok(transcriber)) => transcriber,
            Ok(Err(err)) => {
                let _ = result_worker.send(WorkerEvent::Transcript(Err(err)));
                return;
            }
            Err(_) => return,
        };
        info!("English model ready");
        if result_worker.send(WorkerEvent::Ready).is_err() {
            return;
        }
        for audio in job_rx.iter() {
            let text = transcriber
                .transcribe(&audio)
                .map_err(|err| err.to_string());
            if result_worker.send(WorkerEvent::Transcript(text)).is_err() {
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
