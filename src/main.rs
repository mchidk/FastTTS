//! FastTTS: hold Caps Lock, speak English, let go. The words appear.

#![windows_subsystem = "windows"]

mod audio;
#[cfg(test)]
mod bench;
mod cleanup;
mod config;
mod hook;
mod hud;
mod inject;
mod stream;
mod transcribe;
mod tray;

use anyhow::{anyhow, Result};
use audio::Recorder;
use config::Config;
use hook::{HotkeyHook, WM_CAPSLOCK};
use hud::Hud;
use std::{
    cell::RefCell,
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{channel, Receiver, Sender},
    },
    thread,
    time::{Duration, Instant},
};
use stream::{Chunk, Chunker};
use tracing::{error, info, warn};
use tray::{Command, Tray, WM_TRAY};
use windows::{
    core::{w, PCWSTR},
    Win32::{
        Foundation::{GetLastError, ERROR_ALREADY_EXISTS, HWND, LPARAM, LRESULT, WPARAM},
        System::{LibraryLoader::GetModuleHandleW, Threading::CreateMutexW},
        UI::WindowsAndMessaging::{
            CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, MessageBoxW,
            PostQuitMessage, RegisterClassExW, RegisterWindowMessageW, SetTimer, MB_ICONERROR,
            MB_ICONINFORMATION, MESSAGEBOX_STYLE, MSG, WM_LBUTTONUP, WM_RBUTTONUP, WM_TIMER,
            WNDCLASSEXW, WS_EX_TOOLWINDOW, WS_OVERLAPPED,
        },
    },
};

const TICK_MS: u32 = 33;
const REOPEN_EVERY: Duration = Duration::from_secs(2);
/// Once key repeat has been seen, this much quiet means the key-up was lost
/// (for example Ctrl+Alt+Del or a UAC prompt while Caps Lock was held).
const REPEAT_TIMEOUT: Duration = Duration::from_millis(1500);

/// Audio for the model thread.
struct Job {
    take: u64,
    audio: Vec<f32>,
    continues: bool,
}

/// News from the model and input threads.
enum Event {
    ModelReady,
    ModelFailed(String),
    Text {
        take: u64,
        continues: bool,
        result: Result<String, String>,
    },
    InputFailed(String),
}

/// One Caps Lock hold, from key down until its text is delivered.
struct Take {
    id: u64,
    started: Instant,
    chunker: Chunker,
    parts: Vec<(String, bool)>,
    pending: usize,
    failed: bool,
}

#[derive(PartialEq)]
enum Model {
    Loading,
    Ready,
    Failed,
}

struct App {
    config: Config,
    recorder: Recorder,
    hud: Option<Hud>,
    tray: Tray,
    jobs: Sender<Job>,
    input: Sender<String>,
    events: Receiver<Event>,
    model: Model,
    paused: bool,
    key_held: bool,
    active: Option<Take>,
    finishing: VecDeque<Take>,
    next_take: u64,
    last_reopen: Instant,
    mic_warned: bool,
    /// Last Caps Lock event while held, and whether key repeat is active.
    last_key: Instant,
    repeating: bool,
}

impl App {
    fn key(&mut self, down: bool) {
        // Holding a key repeats key-down; only the first one counts.
        if down && self.key_held {
            self.repeating = true;
            self.last_key = Instant::now();
            return;
        }
        if down == self.key_held {
            return;
        }
        self.key_held = down;
        self.repeating = false;
        self.last_key = Instant::now();
        if down {
            self.start_take();
        } else {
            self.end_take();
        }
    }

    fn start_take(&mut self) {
        if self.paused || self.active.is_some() {
            return;
        }
        if self.model == Model::Failed {
            self.flash("MODEL ERROR");
            return;
        }
        if self.recorder.is_lost() {
            self.flash("NO MICROPHONE");
            return;
        }
        self.recorder.start();
        self.next_take += 1;
        self.active = Some(Take {
            id: self.next_take,
            started: Instant::now(),
            chunker: Chunker::new(
                self.config.chunk_seconds,
                self.config.silence_ms,
                stream::voice_threshold(self.recorder.noise_floor()),
            ),
            parts: Vec::new(),
            pending: 0,
            failed: false,
        });
        self.refresh();
    }

    fn end_take(&mut self) {
        let Some(mut take) = self.active.take() else {
            return;
        };
        let tail = self.recorder.stop();
        let held = take.started.elapsed();
        if held < Duration::from_millis(self.config.min_hold_ms) {
            info!("take {} ignored: {:.2}s tap", take.id, held.as_secs_f32());
            self.refresh();
            return;
        }
        let mut chunks = Vec::new();
        take.chunker.push(&tail, &mut chunks);
        chunks.extend(take.chunker.finish());
        let db = |rms: f32| 20.0 * rms.max(1e-6).log10();
        info!(
            "take {} recorded {:.2}s: loudest {:.0} dBFS, voice threshold {:.0} dBFS, {} ms voiced",
            take.id,
            held.as_secs_f32(),
            db(take.chunker.loudest),
            db(take.chunker.threshold()),
            take.chunker.voiced_total * 20,
        );
        for chunk in chunks {
            self.send(&mut take, chunk);
        }
        self.finishing.push_back(take);
        self.deliver_ready();
        self.refresh();
    }

    fn send(&self, take: &mut Take, chunk: Chunk) {
        let job = Job {
            take: take.id,
            audio: chunk.audio,
            continues: chunk.continues,
        };
        if self.jobs.send(job).is_ok() {
            take.pending += 1;
        } else {
            take.failed = true;
        }
    }

    fn tick(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            self.handle(event);
        }
        if self.recorder.is_lost() && self.last_reopen.elapsed() >= REOPEN_EVERY {
            self.last_reopen = Instant::now();
            if self.active.is_some() {
                // Keep what was said before the microphone dropped out.
                self.end_take();
                self.flash("NO MICROPHONE");
            }
            match self.recorder.reopen() {
                Ok(()) => {
                    info!("microphone open again");
                    self.mic_warned = false;
                }
                Err(err) if !self.mic_warned => {
                    warn!("microphone unavailable, retrying: {err:#}");
                    self.mic_warned = true;
                }
                Err(_) => {}
            }
            self.refresh();
        }
        if self.key_held && self.repeating && self.last_key.elapsed() > REPEAT_TIMEOUT {
            warn!("Caps Lock release was not seen; ending the take");
            self.key(false);
        }
        if let Some(mut take) = self.active.take() {
            let mut chunks = Vec::new();
            take.chunker.push(&self.recorder.drain(), &mut chunks);
            for chunk in chunks {
                self.send(&mut take, chunk);
            }
            self.active = Some(take);
            if self.recorder.is_full() {
                info!("max_record_seconds reached");
                self.end_take();
            }
        }
    }

    fn handle(&mut self, event: Event) {
        match event {
            Event::ModelReady => {
                info!("model ready");
                self.model = Model::Ready;
            }
            Event::ModelFailed(err) => {
                error!("model failed to load: {err}");
                self.model = Model::Failed;
                if self.active.take().is_some() {
                    self.recorder.stop();
                }
                self.finishing.clear();
                self.flash("MODEL ERROR");
            }
            Event::Text {
                take,
                continues,
                result,
            } => {
                let target = self
                    .active
                    .iter_mut()
                    .chain(self.finishing.iter_mut())
                    .find(|t| t.id == take);
                if let Some(target) = target {
                    target.pending -= 1;
                    match result {
                        Ok(text) => target.parts.push((text, continues)),
                        Err(err) => {
                            error!("transcription failed: {err}");
                            target.failed = true;
                        }
                    }
                }
                self.deliver_ready();
            }
            Event::InputFailed(err) => {
                error!("could not insert text: {err}");
                self.flash("INPUT BLOCKED");
            }
        }
        self.refresh();
    }

    /// Hand finished takes to the input thread, oldest first.
    fn deliver_ready(&mut self) {
        if MENU_OPEN.load(Ordering::Relaxed) {
            return;
        }
        while self.finishing.front().is_some_and(|take| take.pending == 0) {
            let take = self.finishing.pop_front().unwrap();
            if take.failed {
                self.flash("TRANSCRIPTION ERROR");
                continue;
            }
            let joined = stream::join(&take.parts);
            let text = if self.config.cleanup {
                cleanup::clean_transcript(&joined)
            } else {
                joined
            };
            info!(
                "take {} done: {} chunk(s), {} characters",
                take.id,
                take.parts.len(),
                text.chars().count()
            );
            if text.is_empty() {
                self.flash("NO SPEECH");
            } else if self.config.inject != "none" {
                let _ = self.input.send(text);
            }
        }
    }

    fn flash(&self, message: &str) {
        if let Some(hud) = &self.hud {
            hud.error(message);
        }
    }

    fn refresh(&mut self) {
        let status = if self.model == Model::Failed {
            "Model failed to load - see the log"
        } else if self.paused {
            "Paused - Caps Lock works normally"
        } else if self.recorder.is_lost() {
            "No microphone - retrying"
        } else if self.active.is_some() {
            "Listening"
        } else if !self.finishing.is_empty() {
            "Transcribing"
        } else if self.model == Model::Loading {
            "Loading the speech model"
        } else {
            "Ready - hold Caps Lock to dictate"
        };
        self.tray.set_status(status, self.paused);
        if let Some(hud) = &self.hud {
            if self.active.is_some() {
                hud.listening();
            } else if !self.finishing.is_empty() {
                hud.transcribing();
            } else {
                hud.idle();
            }
        }
    }

    fn command(&mut self, command: Command) {
        match command {
            Command::TogglePause => {
                self.paused = !self.paused;
                hook::set_paused(self.paused);
                if self.paused {
                    if self.active.take().is_some() {
                        self.recorder.stop();
                    }
                    self.key_held = false;
                } else {
                    hook::release_caps_lock();
                }
                info!("paused: {}", self.paused);
                self.refresh();
            }
            Command::OpenLog => {
                let _ = std::process::Command::new("explorer")
                    .arg(config::log_dir())
                    .spawn();
            }
            Command::Quit => unsafe { PostQuitMessage(0) },
        }
    }
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

/// Run `f` on the app unless it is already in use further up the stack.
fn with_app<T>(f: impl FnOnce(&mut App) -> T) -> Option<T> {
    APP.with(|cell| cell.try_borrow_mut().ok()?.as_mut().map(f))
}

static TASKBAR_CREATED: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
static MENU_OPEN: AtomicBool = AtomicBool::new(false);

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_TIMER => {
            with_app(App::tick);
        }
        WM_CAPSLOCK => {
            with_app(|app| app.key(wparam.0 == 1));
        }
        WM_TRAY => {
            let mouse = (lparam.0 & 0xffff) as u32;
            if mouse == WM_LBUTTONUP || mouse == WM_RBUTTONUP {
                // The menu runs its own message loop, so keep the app free
                // while it is open: Caps Lock and the timer keep working.
                let state = with_app(|app| {
                    let (status, paused) = app.tray.status();
                    (status.to_string(), paused)
                });
                if let Some((status, paused)) = state {
                    // FastTTS owns the foreground while the menu is open, so
                    // a paste now would land nowhere; hold finished text.
                    MENU_OPEN.store(true, Ordering::Relaxed);
                    let command = Tray::menu(hwnd, &status, paused);
                    MENU_OPEN.store(false, Ordering::Relaxed);
                    with_app(|app| {
                        if let Some(command) = command {
                            app.command(command);
                        }
                        app.deliver_ready();
                        app.refresh();
                    });
                }
            }
        }
        _ if Some(&message) == TASKBAR_CREATED.get() => {
            // Explorer restarted; put the icon back.
            with_app(|app| app.tray.restore());
        }
        _ => return DefWindowProcW(hwnd, message, wparam, lparam),
    }
    LRESULT(0)
}

fn create_window() -> Result<HWND> {
    unsafe {
        let instance = GetModuleHandleW(None)?;
        let class = w!("FastTTS");
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(window_proc),
            hInstance: instance.into(),
            lpszClassName: class,
            ..Default::default()
        };
        if RegisterClassExW(&wc) == 0 {
            return Err(anyhow!("could not register the window class"));
        }
        TASKBAR_CREATED.get_or_init(|| RegisterWindowMessageW(w!("TaskbarCreated")));
        // A hidden top-level window: unlike a message-only window it also
        // hears the TaskbarCreated broadcast.
        Ok(CreateWindowExW(
            WS_EX_TOOLWINDOW,
            class,
            class,
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance.into()),
            None,
        )?)
    }
}

fn setup_logging() {
    let appender = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("fasttts")
        .filename_suffix("log")
        .max_log_files(7)
        .build(config::log_dir());
    if let Ok(appender) = appender {
        tracing_subscriber::fmt()
            .with_writer(appender)
            .with_max_level(tracing::Level::INFO)
            .with_ansi(false)
            .init();
    }
    std::panic::set_hook(Box::new(|panic| {
        error!("crashed: {panic}");
        let text = format!("FastTTS stopped because of an internal error:\n\n{panic}");
        message_box(&text, MB_ICONERROR);
    }));
}

/// False when another FastTTS is already running.
fn first_instance() -> bool {
    // The handle is deliberately kept open for the life of the process.
    let mutex = unsafe { CreateMutexW(None, false, w!("Local\\FastTTS")) };
    !(mutex.is_ok() && unsafe { GetLastError() } == ERROR_ALREADY_EXISTS)
}

fn spawn_workers(
    config: &Config,
    jobs: Receiver<Job>,
    input: Receiver<String>,
) -> Result<Receiver<Event>> {
    let (events, received) = channel();
    let model_events = events.clone();
    thread::Builder::new()
        .name("fasttts-model".into())
        .spawn(move || {
            let mut model = match transcribe::Transcriber::load() {
                Ok(model) => model,
                Err(err) => {
                    let _ = model_events.send(Event::ModelFailed(format!("{err:#}")));
                    return;
                }
            };
            let _ = model_events.send(Event::ModelReady);
            for job in jobs {
                let result = model
                    .transcribe(&job.audio)
                    .map_err(|err| format!("{err:#}"));
                let event = Event::Text {
                    take: job.take,
                    continues: job.continues,
                    result,
                };
                if model_events.send(event).is_err() {
                    break;
                }
            }
        })?;
    let config = config.clone();
    thread::Builder::new()
        .name("fasttts-input".into())
        .spawn(move || {
            let injector = match inject::Injector::new() {
                Ok(injector) => injector,
                Err(err) => {
                    let _ = events.send(Event::InputFailed(format!("{err:#}")));
                    return;
                }
            };
            for text in input {
                let inserted = injector.insert(
                    &text,
                    &config.inject,
                    config.char_delay_ms,
                    config.paste_timeout_ms,
                    config.append_space,
                );
                if let Err(err) = inserted {
                    if events.send(Event::InputFailed(format!("{err:#}"))).is_err() {
                        break;
                    }
                }
            }
        })?;
    Ok(received)
}

fn run() -> Result<()> {
    // Compile the cleanup patterns now: a broken build fails at launch,
    // not after the user has spoken.
    cleanup::clean_transcript("um, ready");
    let config = Config::load()?;
    let hwnd = create_window()?;
    let recorder = Recorder::new(&config.mic_device, config.max_record_seconds);
    let hud = if config.hud {
        Hud::new(recorder.meter())
            .inspect_err(|err| warn!("running without the HUD: {err:#}"))
            .ok()
    } else {
        None
    };
    let tray = Tray::new(hwnd)?;
    let (jobs, job_queue) = channel();
    let (input, input_queue) = channel();
    let events = spawn_workers(&config, job_queue, input_queue)?;
    let _hook = HotkeyHook::start(hwnd)?;
    hook::release_caps_lock();

    APP.with(|cell| {
        let mut app = App {
            config,
            recorder,
            hud,
            tray,
            jobs,
            input,
            events,
            model: Model::Loading,
            paused: false,
            key_held: false,
            active: None,
            finishing: VecDeque::new(),
            next_take: 0,
            last_reopen: Instant::now(),
            mic_warned: false,
            last_key: Instant::now(),
            repeating: false,
        };
        app.refresh();
        *cell.borrow_mut() = Some(app);
    });
    unsafe { SetTimer(Some(hwnd), 1, TICK_MS, None) };

    let mut message = MSG::default();
    while unsafe { GetMessageW(&mut message, None, 0, 0) }.0 > 0 {
        unsafe { DispatchMessageW(&message) };
    }
    info!("FastTTS quitting");
    APP.with(|cell| cell.borrow_mut().take());
    Ok(())
}

fn message_box(text: &str, style: MESSAGEBOX_STYLE) {
    let text: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
    unsafe { MessageBoxW(None, PCWSTR(text.as_ptr()), w!("FastTTS"), style) };
}

fn main() {
    setup_logging();
    info!("FastTTS {} starting", env!("CARGO_PKG_VERSION"));
    if !first_instance() {
        message_box(
            "FastTTS is already running. Look for its icon in the tray.",
            MB_ICONINFORMATION,
        );
        return;
    }
    if let Err(err) = run() {
        error!("could not start: {err:#}");
        message_box(
            &format!("FastTTS could not start:\n\n{err:#}"),
            MB_ICONERROR,
        );
    }
}
