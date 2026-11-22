mod capture;
mod files;
mod launcher;
mod notes;
mod render;
mod stats;
mod sway;
mod ui;

use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState, Region},
    delegate_registry,
    dispatch2::Dispatch2,
    output::{OutputHandler, OutputState},
    presentation_time::{PresentTime, PresentationTimeHandler, PresentationTimeState},
    reexports::{
        calloop::{self, EventLoop},
        calloop_wayland_source::WaylandSource,
    },
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers, RawModifiers},
        pointer::{PointerEvent, PointerEventKind, PointerHandler},
        Capability, SeatHandler, SeatState,
    },
    reexports::protocols_wlr::layer_shell::v1::client::{
        zwlr_layer_shell_v1::{Layer, ZwlrLayerShellV1},
        zwlr_layer_surface_v1::{self, Anchor, KeyboardInteractivity, ZwlrLayerSurfaceV1},
    },
    shm::{raw::RawPool, Shm, ShmHandler},
};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use wayland_client::{
    globals::registry_queue_init,
    protocol::{wl_buffer, wl_keyboard, wl_output, wl_pointer, wl_seat, wl_surface},
    Connection, Proxy, QueueHandle, WEnum,
};
use wayland_protocols::ext::image_capture_source::v1::client::ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1;
use wayland_protocols::ext::image_copy_capture::v1::client::{ext_image_copy_capture_manager_v1::{ExtImageCopyCaptureManagerV1, Options}, ext_image_copy_capture_frame_v1::ExtImageCopyCaptureFrameV1};
use wayland_protocols::wp::fractional_scale::v1::client::{wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1, wp_fractional_scale_v1::{self, WpFractionalScaleV1}};
use wayland_protocols::wp::idle_inhibit::zv1::client::{zwp_idle_inhibit_manager_v1::ZwpIdleInhibitManagerV1, zwp_idle_inhibitor_v1::ZwpIdleInhibitorV1};
use wayland_protocols::wp::presentation_time::client::wp_presentation_feedback;
use wayland_protocols::wp::single_pixel_buffer::v1::client::wp_single_pixel_buffer_manager_v1::WpSinglePixelBufferManagerV1;
use wayland_protocols::wp::viewporter::client::{wp_viewport::WpViewport, wp_viewporter::WpViewporter};

/// A press and release of the overlay key within this time is a tap (stays open).
/// Present in the state directory while swaylock is disabled.
const IDLE_INHIBIT_FILE: &str = "idle-inhibit";
const TAP_MAXIMUM: Duration = Duration::from_millis(300);
const CAPTURE_INTERVAL: Duration = Duration::from_millis(1500);
const CAPTURE_DEBOUNCE: Duration = Duration::from_millis(300);
const DIM_ALPHA: f32 = 0.45;
pub const BLUR_FADE: Duration = Duration::from_millis(140);

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Mode {
    Hidden,
    Open,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Focus {
    Workspaces,
    Files,
    Launcher,
    Notes,
}

/// The panel row, left to right: Alt+h and Alt+l walk it, Tab cycles it.
pub const PANELS: [Focus; 3] = [Focus::Files, Focus::Launcher, Focus::Notes];
/// sway binding mode while the overlay shows: only the overlay key is bound, so Alt+hjkl
/// and everything else reach the overlay (see sway/oversway.conf).
const SWAY_MODE: &str = "oversway";

#[derive(Clone, Debug)]
pub enum Action {
    Workspace(String),
    Launch(usize),
    Focus(Focus),
    ToggleIdleInhibit,
    OpenFile(usize),
}

/// Everything the UI reads. No Wayland or GPU device handles in here.
pub struct App {
    pub mode: Mode,
    pub shown_at: Instant,
    pub focus: Focus,
    pub snapshot: sway::Snapshot,
    pub stats: stats::Stats,
    pub entries: Vec<launcher::Entry>,
    pub results: Vec<usize>,
    pub selected: usize,
    pub query: String,
    pub launch_counts: HashMap<String, u32>,
    pub icons: Option<render::Image>,
    pub icon_slots: HashMap<String, u32>,
    pub notes: String,
    pub notes_cursor: usize,
    pub notes_dirty: bool,
    pub thumbnails: HashMap<(String, String), (render::Image, u64)>,
    /// Swaylock disabled: an idle inhibitor is held, so swayidle never fires.
    pub idle_inhibited: bool,
    pub files: files::Browser,
    /// Panel to return to with Alt+j from the workspace row.
    pub last_panel: Focus,
    /// Workspace under the keyboard cursor while the workspace row has focus.
    pub workspace_selected: String,
    /// Eye (inhibited) and crossed-out eye side by side, white, premultiplied.
    pub eye_icons: render::Image,
}

pub struct OutputView {
    pub output: wl_output::WlOutput,
    pub name: String,
    dim_surface: wl_surface::WlSurface,
    dim_role: Option<ZwlrLayerSurfaceV1>,
    dim_viewport: WpViewport,
    dim_size: (u32, u32),
    pub surface: wl_surface::WlSurface,
    role: Option<ZwlrLayerSurfaceV1>,
    viewport: WpViewport,
    _fractional: Option<WpFractionalScaleV1>,
    pub scale120: u32,
    pub logical: (u32, u32),
    configured: bool,
    needs_redraw: bool,
    target: render::Target,
    pub blurred: Option<(render::Image, (String, u64))>,
    pub hits: Vec<([f32; 4], Action)>,
    pub capture: capture::Capture,
}

pub struct State {
    connection: Connection,
    queue_handle: QueueHandle<State>,
    registry_state: RegistryState,
    output_state: OutputState,
    seat_state: SeatState,
    compositor: CompositorState,
    layer_shell: ZwlrLayerShellV1,
    shm: Shm,
    viewporter: WpViewporter,
    _single_pixel: WpSinglePixelBufferManagerV1,
    fractional_manager: Option<WpFractionalScaleManagerV1>,
    presentation: PresentationTimeState,
    capture_manager: Option<ExtImageCopyCaptureManagerV1>,
    capture_sources: Option<ExtOutputImageCaptureSourceManagerV1>,
    idle_inhibit_manager: Option<ZwpIdleInhibitManagerV1>,
    /// sway only honours inhibitors on visible windows or mapped layer surfaces (occluded is fine),
    /// so this 1x1 transparent background-layer surface is mapped while swaylock is disabled.
    idle_inhibit_surface: wl_surface::WlSurface,
    idle_inhibit_role: Option<ZwlrLayerSurfaceV1>,
    idle_inhibitor: Option<ZwpIdleInhibitorV1>,
    loop_handle: calloop::LoopHandle<'static, State>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,
    dim_buffer: wl_buffer::WlBuffer,
    clear_buffer: wl_buffer::WlBuffer,
    gpu: render::Gpu,
    pub outputs: Vec<OutputView>,
    app: App,
    always_mapped: bool,
    modifiers: Modifiers,
    /// The overlay key went down (press binding); its release ends a hold or keeps a tap open.
    overlay_pressed_at: Option<Instant>,
    hide_on_release: bool,
    workspace_generation: u64,
    thumbnail_serial: u64,
    next_stats: Instant,
    next_notes_save: Option<Instant>,
    rescan_at: Option<Instant>,
    latency_origin: Option<Instant>,
    latency_pending: Vec<(wl_surface::WlSurface, &'static str)>,
    jobs: std::sync::mpsc::Sender<WorkerJob>,
    /// Ordered, fire-and-forget sway commands (mode switches), off the event thread.
    sway_commands: std::sync::mpsc::Sender<String>,
}

enum WorkerJob {
    Downscale(capture::Job),
    Scan,
}

enum WorkerResult {
    Gpu(stats::GpuStats),
    Thumbnail(capture::Downscaled),
    Entries(Vec<launcher::Entry>, launcher::IconAtlas),
}

/// User data for protocol objects whose events we do not need.
pub struct Quiet;
impl<I: Proxy> Dispatch2<I, State> for Quiet {
    fn event(&self, _: &mut State, _: &I, _: I::Event, _: &Connection, _: &QueueHandle<State>) {}
}

struct FractionalData(wl_surface::WlSurface);
impl Dispatch2<WpFractionalScaleV1, State> for FractionalData {
    fn event(&self, state: &mut State, _: &WpFractionalScaleV1, event: wp_fractional_scale_v1::Event, _: &Connection, _: &QueueHandle<State>) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event {
            if let Some(view) = state.outputs.iter_mut().find(|view| view.surface == self.0) {
                view.scale120 = scale;
                view.needs_redraw = true;
            }
        }
    }
}

fn monotonic_nanoseconds() -> i128 {
    let mut time = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time) };
    time.tv_sec as i128 * 1_000_000_000 + time.tv_nsec as i128
}

fn single_instance_lock() -> std::fs::File {
    use std::os::fd::AsRawFd;
    let directory = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
    let file = std::fs::File::create(format!("{directory}/oversway.lock")).expect("lock file");
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        eprintln!("oversway is already running");
        std::process::exit(1);
    }
    file
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("oversway=info,wgpu_hal=warn,wgpu_core=warn")).init();
    let _lock = single_instance_lock();
    let always_mapped = std::env::args().any(|argument| argument == "--always-mapped");

    let connection = Connection::connect_to_env().expect("no Wayland display");
    let (globals, event_queue) = registry_queue_init::<State>(&connection).expect("registry");
    let queue_handle = event_queue.handle();
    let compositor = CompositorState::bind(&globals, &queue_handle).expect("wl_compositor");
    let layer_shell: ZwlrLayerShellV1 = globals.bind(&queue_handle, 4..=4, Quiet).expect("zwlr_layer_shell_v1 version 4");
    let shm = Shm::bind(&globals, &queue_handle).expect("wl_shm");
    let viewporter: WpViewporter = globals.bind(&queue_handle, 1..=1, Quiet).expect("wp_viewporter");
    let single_pixel: WpSinglePixelBufferManagerV1 = globals.bind(&queue_handle, 1..=1, Quiet).expect("wp_single_pixel_buffer_manager_v1");
    let fractional_manager = globals.bind(&queue_handle, 1..=1, Quiet).ok();
    let capture_manager = globals.bind(&queue_handle, 1..=1, Quiet).ok();
    let capture_sources = globals.bind(&queue_handle, 1..=1, Quiet).ok();
    if capture_manager.is_none() || capture_sources.is_none() {
        log::warn!("ext-image-copy-capture not available: no thumbnails or blur");
    }
    let presentation = PresentationTimeState::bind(&globals, &queue_handle);
    let idle_inhibit_manager: Option<ZwpIdleInhibitManagerV1> = globals.bind(&queue_handle, 1..=1, Quiet).ok();
    let idle_inhibit_surface = compositor.create_surface(&queue_handle);
    let scale = |value: f32| (value.clamp(0.0, 1.0) as f64 * u32::MAX as f64) as u32;
    let dim_buffer = single_pixel.create_u32_rgba_buffer(0, 0, 0, scale(DIM_ALPHA), &queue_handle, Quiet);
    let clear_buffer = single_pixel.create_u32_rgba_buffer(0, 0, 0, 0, &queue_handle, Quiet);

    let started = Instant::now();
    let display_pointer = connection.backend().display_ptr() as *mut std::ffi::c_void;
    let probe = compositor.create_surface(&queue_handle);
    let gpu = render::Gpu::new(display_pointer, probe.id().as_ptr() as *mut _);
    probe.destroy();
    log::info!("GPU ready in {:?}", started.elapsed());
    let eye_icons = gpu.upload(2 * launcher::EYE_ICON_SIZE, launcher::EYE_ICON_SIZE, &launcher::eye_icons());

    let mut event_loop: EventLoop<'static, State> = EventLoop::try_new().expect("event loop");
    let loop_handle = event_loop.handle();
    WaylandSource::new(connection.clone(), event_queue).insert(loop_handle.clone()).expect("wayland source");

    let (sway_sender, sway_receiver) = calloop::channel::channel::<sway::Message>();
    loop_handle
        .insert_source(sway_receiver, |event, _, state: &mut State| {
            if let calloop::channel::Event::Msg(message) = event {
                state.sway_message(message);
            }
        })
        .unwrap();
    std::thread::spawn(move || sway::listen(move |message| sway_sender.send(message).is_ok()));

    let (result_sender, result_receiver) = calloop::channel::channel::<WorkerResult>();
    loop_handle
        .insert_source(result_receiver, |event, _, state: &mut State| {
            if let calloop::channel::Event::Msg(result) = event {
                state.worker_result(result);
            }
        })
        .unwrap();
    let result_sender_for_threads = result_sender.clone();
    let (jobs, job_receiver) = std::sync::mpsc::channel::<WorkerJob>();
    std::thread::spawn(move || {
        for job in job_receiver {
            let result = match job {
                WorkerJob::Downscale(job) => {
                    let image = capture::downscale(job);
                    capture::save_cached(&image);
                    WorkerResult::Thumbnail(image)
                }
                WorkerJob::Scan => {
                    let started = Instant::now();
                    let entries = launcher::scan();
                    let atlas = launcher::build_icon_atlas(&entries);
                    log::info!("{} applications, {} icons in {:?}", entries.len(), atlas.slots.len(), started.elapsed());
                    WorkerResult::Entries(entries, atlas)
                }
            };
            if result_sender.send(result).is_err() {
                return;
            }
        }
    });
    jobs.send(WorkerJob::Scan).unwrap();
    {
        let sender = result_sender_for_threads.clone();
        std::thread::spawn(move || {
            for image in capture::load_cached() {
                if sender.send(WorkerResult::Thumbnail(image)).is_err() {
                    return;
                }
            }
        });
        let sender = result_sender_for_threads;
        std::thread::spawn(move || stats::poll_gpu(|gpu| sender.send(WorkerResult::Gpu(gpu)).is_ok()));
    }
    watch_applications(&loop_handle);

    let notes = notes::load();
    let mut state = State {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &queue_handle),
        seat_state: SeatState::new(&globals, &queue_handle),
        connection,
        queue_handle,
        compositor,
        layer_shell,
        shm,
        viewporter,
        _single_pixel: single_pixel,
        fractional_manager,
        presentation,
        capture_manager,
        capture_sources,
        idle_inhibit_manager,
        idle_inhibit_surface,
        idle_inhibit_role: None,
        idle_inhibitor: None,
        loop_handle,
        keyboard: None,
        pointer: None,
        dim_buffer,
        clear_buffer,
        gpu,
        outputs: Vec::new(),
        app: App {
            mode: Mode::Hidden,
            shown_at: Instant::now(),
            focus: Focus::Launcher,
            snapshot: Default::default(),
            stats: Default::default(),
            entries: Vec::new(),
            results: Vec::new(),
            selected: 0,
            query: String::new(),
            launch_counts: launcher::load_launch_counts(),
            icons: None,
            icon_slots: HashMap::new(),
            notes_cursor: notes.len(),
            notes,
            notes_dirty: false,
            thumbnails: HashMap::new(),
            idle_inhibited: false,
            files: files::new(),
            last_panel: Focus::Launcher,
            workspace_selected: String::new(),
            eye_icons,
        },
        always_mapped,
        modifiers: Modifiers::default(),
        overlay_pressed_at: None,
        hide_on_release: false,
        workspace_generation: 0,
        thumbnail_serial: 0,
        next_stats: Instant::now(),
        next_notes_save: None,
        rescan_at: None,
        latency_origin: None,
        latency_pending: Vec::new(),
        jobs,
        sway_commands: sway::command_sender(),
    };
    // A previous instance may have died while the overlay showed.
    let _ = state.sway_commands.send("mode default".into());
    state.set_idle_inhibited(launcher::state_path(IDLE_INHIBIT_FILE).exists());
    log::info!("oversway running (always_mapped = {always_mapped})");

    loop {
        let now = Instant::now();
        let deadlines = [
            Some(state.next_stats),
            state.next_notes_save,
            state.rescan_at,
        ]
        .into_iter()
        .chain(state.outputs.iter().map(|view| if view.capture.frame.is_none() { view.capture.due } else { None }))
        .flatten()
        .min();
        let timeout = deadlines.map(|deadline| deadline.saturating_duration_since(now));
        if let Err(error) = event_loop.dispatch(timeout, &mut state) {
            log::error!("event loop: {error}");
            std::process::exit(1);
        }
        if let Some(error) = state.connection.protocol_error() {
            log::error!("Wayland protocol error: {error}");
            std::process::exit(1);
        }
        state.tick();
        state.render_dirty();
        if let Err(error) = state.connection.flush() {
            log::error!("Wayland connection lost: {error}");
            std::process::exit(1);
        }
    }
}

fn watch_applications(loop_handle: &calloop::LoopHandle<'static, State>) {
    use inotify::{Inotify, WatchMask};
    let Ok(inotify) = Inotify::init() else { return };
    for directory in launcher::application_directories() {
        let _ = inotify.watches().add(&directory, WatchMask::CREATE | WatchMask::DELETE | WatchMask::CLOSE_WRITE | WatchMask::MOVED_TO | WatchMask::MOVED_FROM);
    }
    let source = calloop::generic::Generic::new(inotify, calloop::Interest::READ, calloop::Mode::Level);
    loop_handle
        .insert_source(source, |_, inotify, state: &mut State| {
            let mut buffer = [0u8; 4096];
            // SAFETY: we do not drop the Inotify instance.
            while let Ok(events) = unsafe { inotify.get_mut() }.read_events(&mut buffer) {
                if events.count() == 0 {
                    break;
                }
            }
            state.rescan_at = Some(Instant::now() + Duration::from_millis(500));
            Ok(calloop::PostAction::Continue)
        })
        .unwrap();
}

impl State {
    fn create_view(&mut self, output: wl_output::WlOutput) {
        let Some(info) = self.output_state.info(&output) else { return };
        let name = info.name.clone().unwrap_or_default();
        let queue_handle = &self.queue_handle;
        let dim_surface = self.compositor.create_surface(queue_handle);
        let surface = self.compositor.create_surface(queue_handle);
        let dim_viewport = self.viewporter.get_viewport(&dim_surface, queue_handle, Quiet);
        let viewport = self.viewporter.get_viewport(&surface, queue_handle, Quiet);
        let fractional = self.fractional_manager.as_ref().map(|manager| manager.get_fractional_scale(&surface, queue_handle, FractionalData(surface.clone())));
        let target = self.gpu.create_target(self.connection.backend().display_ptr() as *mut _, surface.id().as_ptr() as *mut _);
        let mut capture = capture::Capture::default();
        if let (Some(manager), Some(sources)) = (&self.capture_manager, &self.capture_sources) {
            let source = sources.create_source(&output, queue_handle, Quiet);
            capture.session = Some(manager.create_session(&source, Options::empty(), queue_handle, capture::SessionData { output: name.clone() }));
            capture.source = Some(source);
        }
        let (mut dim_role, mut role) = (None, None);
        if self.always_mapped {
            let empty = Region::new(&self.compositor).unwrap();
            // The dim role is created first so it stacks below the main surface.
            dim_role = Some(self.create_role(&dim_surface, &output, KeyboardInteractivity::None, true));
            role = Some(self.create_role(&surface, &output, KeyboardInteractivity::None, false));
            for surface in [&dim_surface, &surface] {
                surface.set_input_region(Some(empty.wl_region()));
                surface.commit();
            }
        }
        log::info!("output {name}: scale {}, logical size {:?}", info.scale_factor, info.logical_size);
        self.outputs.push(OutputView {
            output,
            name,
            dim_surface,
            dim_role,
            dim_viewport,
            dim_size: (0, 0),
            surface,
            role,
            viewport,
            _fractional: fractional,
            scale120: info.scale_factor as u32 * 120,
            logical: (0, 0),
            configured: false,
            needs_redraw: false,
            target,
            blurred: None,
            hits: Vec::new(),
            capture,
        });
    }

    /// A layer-shell role for `surface`. Destroying it on hide (instead of only unmapping)
    /// guarantees that a configure sent before the unmap is never acked afterwards.
    fn create_role(&self, surface: &wl_surface::WlSurface, output: &wl_output::WlOutput, interactivity: KeyboardInteractivity, dim: bool) -> ZwlrLayerSurfaceV1 {
        let namespace = if dim { "oversway-dim" } else { "oversway" };
        let role = self.layer_shell.get_layer_surface(surface, Some(output), Layer::Overlay, namespace.into(), &self.queue_handle, LayerData { surface: surface.clone(), dim });
        role.set_anchor(Anchor::Top | Anchor::Bottom | Anchor::Left | Anchor::Right);
        role.set_exclusive_zone(-1);
        role.set_keyboard_interactivity(interactivity);
        role
    }

    fn layer_configured(&mut self, surface: &wl_surface::WlSurface, dim: bool, size: (u32, u32)) {
        if surface == &self.idle_inhibit_surface {
            surface.attach(Some(&self.clear_buffer), 0, 0);
            surface.commit();
            if let (Some(manager), None) = (&self.idle_inhibit_manager, &self.idle_inhibitor) {
                self.idle_inhibitor = Some(manager.create_inhibitor(surface, &self.queue_handle, Quiet));
            }
            return;
        }
        let shown = self.app.mode != Mode::Hidden;
        let Some(view) = self.outputs.iter_mut().find(|view| &view.surface == surface || &view.dim_surface == surface) else { return };
        if dim {
            view.dim_size = size;
            let buffer = if shown { &self.dim_buffer } else { &self.clear_buffer };
            Self::attach_dim(&self.presentation, &self.queue_handle, &mut self.latency_pending, view, buffer);
        } else {
            view.logical = size;
            view.configured = true;
            view.needs_redraw = true;
        }
    }

    fn redraw_all(&mut self) {
        for view in &mut self.outputs {
            view.needs_redraw = true;
        }
    }

    fn sway_message(&mut self, message: sway::Message) {
        match message {
            sway::Message::WorkspaceFocus => {
                log::debug!("workspace focus event");
                self.workspace_generation += 1;
                for view in &mut self.outputs {
                    view.capture.frame.take().map(|frame| frame.destroy());
                    view.capture.due = Some(Instant::now() + CAPTURE_DEBOUNCE);
                }
            }
            sway::Message::WindowChange => {
                let soon = Instant::now() + CAPTURE_DEBOUNCE;
                for view in &mut self.outputs {
                    if view.capture.frame.is_none() {
                        view.capture.due = Some(view.capture.due.map_or(soon, |due| due.min(soon)));
                    }
                }
            }
            sway::Message::Binding { command, received } => {
                let command = command.trim();
                log::debug!("binding {command:?} after {:?}", received.elapsed());
                if command == "nop overlay_press" {
                    // Show at once; the release decides whether it stays.
                    self.overlay_pressed_at = Some(received);
                    self.hide_on_release = self.app.mode == Mode::Open;
                    if self.app.mode == Mode::Hidden {
                        self.show(Mode::Open, received);
                    }
                } else if command == "nop overlay_toggle" {
                    match self.overlay_pressed_at.take() {
                        // A tap opens and stays; a hold, or a press while already open, hides.
                        Some(pressed) => {
                            let held = received.saturating_duration_since(pressed);
                            if std::mem::take(&mut self.hide_on_release) || held > TAP_MAXIMUM {
                                self.hide();
                            }
                        }
                        // No press seen (send_tick, scripts): plain toggle.
                        None => match self.app.mode {
                            Mode::Hidden => self.show(Mode::Open, received),
                            _ => self.hide(),
                        },
                    }
                }
            }
            sway::Message::Snapshot(snapshot) => {
                self.app.snapshot = snapshot;
                self.redraw_all();
            }
        }
    }

    fn show(&mut self, mode: Mode, received: Instant) {
        log::debug!("show {mode:?}");
        self.app.mode = mode;
        self.app.shown_at = Instant::now();
        self.app.focus = Focus::Launcher;
        self.app.last_panel = Focus::Launcher;
        self.app.workspace_selected = self.app.snapshot.workspaces.iter().find(|workspace| workspace.focused).map_or(String::new(), |workspace| workspace.name.clone());
        files::read(&mut self.app.files);
        let _ = self.sway_commands.send(format!("mode {SWAY_MODE}"));
        self.latency_origin = Some(received);
        self.latency_pending = self.outputs.iter().flat_map(|view| [(view.dim_surface.clone(), "dim"), (view.surface.clone(), "overlay")]).collect();
        self.refilter();
        let focused_output = self.app.snapshot.focused_output().map(str::to_string);
        let single_output = self.outputs.len() == 1;
        for index in 0..self.outputs.len() {
            let view = &mut self.outputs[index];
            // Never capture while the overlay shows.
            view.capture.frame.take().map(|frame| frame.destroy());
            let focused = single_output || focused_output.as_deref() == Some(view.name.as_str());
            let interactivity = if focused { KeyboardInteractivity::Exclusive } else { KeyboardInteractivity::None };
            if self.always_mapped {
                view.role.as_ref().unwrap().set_keyboard_interactivity(interactivity);
                view.surface.set_input_region(None);
                view.dim_surface.set_input_region(None);
                Self::attach_dim(&self.presentation, &self.queue_handle, &mut self.latency_pending, view, &self.dim_buffer);
                view.needs_redraw = true;
            } else {
                // New roles and an initial commit without a buffer; the configure maps them.
                let (dim_surface, surface, output) = (view.dim_surface.clone(), view.surface.clone(), view.output.clone());
                let dim_role = self.create_role(&dim_surface, &output, KeyboardInteractivity::None, true);
                let role = self.create_role(&surface, &output, interactivity, false);
                dim_surface.commit();
                surface.commit();
                let view = &mut self.outputs[index];
                view.dim_role = Some(dim_role);
                view.role = Some(role);
            }
        }
        let _ = self.connection.flush();
    }

    fn attach_dim(presentation: &PresentationTimeState, queue_handle: &QueueHandle<State>, latency_pending: &mut Vec<(wl_surface::WlSurface, &'static str)>, view: &mut OutputView, buffer: &wl_buffer::WlBuffer) {
        let surface = &view.dim_surface;
        surface.attach(Some(buffer), 0, 0);
        if view.dim_size.0 > 0 {
            view.dim_viewport.set_destination(view.dim_size.0 as i32, view.dim_size.1 as i32);
        }
        surface.damage_buffer(0, 0, 1, 1);
        // Until the first commit after show is presented, ask for feedback on every commit:
        // a commit superseded within the same vblank is only "discarded".
        if latency_pending.iter().any(|(pending, _)| pending == surface) {
            let _ = presentation.feedback(surface, queue_handle);
        }
        surface.commit();
    }

    /// Holds or drops the idle inhibitor and remembers the choice across restarts.
    fn set_idle_inhibited(&mut self, inhibited: bool) {
        if self.idle_inhibit_manager.is_none() {
            log::warn!("no zwp_idle_inhibit_manager_v1: cannot disable swaylock");
            return;
        }
        if let Some(inhibitor) = self.idle_inhibitor.take() {
            inhibitor.destroy();
        }
        if let Some(role) = self.idle_inhibit_role.take() {
            self.idle_inhibit_surface.attach(None, 0, 0);
            self.idle_inhibit_surface.commit();
            role.destroy();
        }
        if inhibited {
            let surface = &self.idle_inhibit_surface;
            let data = LayerData { surface: surface.clone(), dim: false };
            let role = self.layer_shell.get_layer_surface(surface, None, Layer::Background, "oversway-idle-inhibit".into(), &self.queue_handle, data);
            role.set_anchor(Anchor::Top | Anchor::Left);
            role.set_size(1, 1);
            role.set_keyboard_interactivity(KeyboardInteractivity::None);
            let empty = Region::new(&self.compositor).unwrap();
            surface.set_input_region(Some(empty.wl_region()));
            surface.commit();
            // The inhibitor itself is created in layer_configured, once the surface is mapped:
            // sway judges it on creation and does not look again when a layer surface maps.
            self.idle_inhibit_role = Some(role);
        }
        self.app.idle_inhibited = inhibited;
        let path = launcher::state_path(IDLE_INHIBIT_FILE);
        if inhibited {
            let _ = std::fs::create_dir_all(path.parent().unwrap());
            let _ = std::fs::write(&path, "");
        } else {
            let _ = std::fs::remove_file(&path);
        }
        let _ = self.connection.flush();
        log::info!("swaylock {}", if inhibited { "disabled (idle inhibited)" } else { "enabled" });
    }

    fn hide(&mut self) {
        if self.app.mode == Mode::Hidden {
            return;
        }
        self.app.mode = Mode::Hidden;
        self.app.query.clear();
        let _ = self.sway_commands.send("mode default".into());
        if self.app.notes_dirty {
            notes::save(&self.app.notes);
            self.app.notes_dirty = false;
            self.next_notes_save = None;
        }
        let empty = Region::new(&self.compositor).unwrap();
        for view in &mut self.outputs {
            if self.always_mapped {
                view.role.as_ref().unwrap().set_keyboard_interactivity(KeyboardInteractivity::None);
                view.surface.set_input_region(Some(empty.wl_region()));
                view.dim_surface.set_input_region(Some(empty.wl_region()));
                view.dim_surface.attach(Some(&self.clear_buffer), 0, 0);
                view.dim_surface.damage_buffer(0, 0, 1, 1);
                view.dim_surface.commit();
                view.needs_redraw = true;
            } else {
                // Unmap with a null buffer, then drop the role so no stale configure gets acked.
                for (surface, role) in [(&view.dim_surface, &mut view.dim_role), (&view.surface, &mut view.role)] {
                    surface.attach(None, 0, 0);
                    surface.commit();
                    role.take().map(|role| role.destroy());
                }
                view.configured = false;
            }
            view.capture.due = Some(Instant::now() + CAPTURE_DEBOUNCE);
        }
        let _ = self.connection.flush();
    }

    fn tick(&mut self) {
        let now = Instant::now();
        if now >= self.next_stats {
            stats::sample(&mut self.app.stats);
            self.next_stats = now + Duration::from_secs(1);
            if self.app.mode != Mode::Hidden {
                self.redraw_all();
            }
        }
        if self.next_notes_save.is_some_and(|deadline| now >= deadline) {
            notes::save(&self.app.notes);
            self.app.notes_dirty = false;
            self.next_notes_save = None;
        }
        if self.rescan_at.is_some_and(|deadline| now >= deadline) {
            self.rescan_at = None;
            let _ = self.jobs.send(WorkerJob::Scan);
        }
        if self.app.mode == Mode::Hidden {
            for index in 0..self.outputs.len() {
                let capture = &self.outputs[index].capture;
                if capture.frame.is_none() && capture.due.is_some_and(|due| now >= due) {
                    self.request_capture(index);
                }
            }
        }
    }

    fn request_capture(&mut self, index: usize) {
        let view = &mut self.outputs[index];
        let capture = &mut view.capture;
        let (Some(session), Some(format)) = (&capture.session, capture.format) else { return };
        let (width, height) = capture.size;
        if width == 0 || height == 0 {
            return;
        }
        let Some(layout) = capture::layout(format) else { return };
        if capture.buffer.is_none() {
            let stride = width * layout.bytes;
            let length = (stride * height) as usize;
            if capture.pool.as_ref().is_none_or(|pool| pool.len() < length) {
                capture.pool = RawPool::new(length, &self.shm).ok();
            }
            let Some(pool) = capture.pool.as_mut() else { return };
            capture.buffer = Some(pool.create_buffer(0, width as i32, height as i32, stride as i32, format, Quiet, &self.queue_handle));
        }
        log::debug!("requesting capture of {} ({width}x{height} {format:?})", view.name);
        let frame = session.create_frame(&self.queue_handle, capture::FrameData { output: view.name.clone() });
        frame.attach_buffer(capture.buffer.as_ref().unwrap());
        frame.damage_buffer(0, 0, width as i32, height as i32);
        frame.capture();
        capture.frame = Some(frame);
        capture.due = None;
        capture.frame_generation = self.workspace_generation;
        capture.frame_workspace = self.app.snapshot.visible_workspace(&view.name).map(|workspace| workspace.name.clone()).unwrap_or_default();
    }

    pub fn capture_ready(&mut self, output: &str, frame: &ExtImageCopyCaptureFrameV1) {
        let generation = self.workspace_generation;
        let mode = self.app.mode;
        let visible = self.app.snapshot.visible_workspace(output).map(|workspace| workspace.name.clone()).unwrap_or_default();
        let Some(view) = self.outputs.iter_mut().find(|view| view.name == output) else { return };
        let capture = &mut view.capture;
        if capture.frame.as_ref() != Some(frame) {
            return;
        }
        capture.frame.take().unwrap().destroy();
        capture.due = Some(Instant::now() + CAPTURE_INTERVAL);
        let overlay_visible = mode != Mode::Hidden;
        if overlay_visible || capture.frame_generation != generation || capture.frame_workspace != visible || visible.is_empty() {
            capture.due = Some(Instant::now() + CAPTURE_DEBOUNCE);
            return;
        }
        let (width, height) = capture.size;
        let Some(pool) = capture.pool.as_mut() else { return };
        let Some(layout) = capture.format.and_then(capture::layout) else { return };
        let stride = width * layout.bytes;
        let pixels = pool.mmap()[..(stride * height) as usize].to_vec();
        let _ = self.jobs.send(WorkerJob::Downscale(capture::Job { output: output.to_string(), workspace: visible, width, height, stride, layout, pixels }));
    }

    fn worker_result(&mut self, result: WorkerResult) {
        match result {
            WorkerResult::Gpu(gpu) => self.app.stats.gpu = Some(gpu),
            WorkerResult::Thumbnail(image) => {
                self.thumbnail_serial += 1;
                let key = (image.output, image.workspace);
                match self.app.thumbnails.get_mut(&key) {
                    // Same size: overwrite the texture in place instead of allocating a new one.
                    Some((existing, serial)) if (existing.width, existing.height) == (image.width, image.height) => {
                        self.gpu.write_image(existing, &image.pixels);
                        *serial = self.thumbnail_serial;
                    }
                    _ => {
                        let uploaded = self.gpu.upload(image.width, image.height, &image.pixels);
                        self.app.thumbnails.insert(key, (uploaded, self.thumbnail_serial));
                    }
                }
                self.gpu.flush();
            }
            WorkerResult::Entries(entries, atlas) => {
                let width = launcher::ATLAS_COLUMNS * launcher::ICON_SIZE;
                self.app.icons = Some(self.gpu.upload(width, atlas.rows * launcher::ICON_SIZE, &atlas.pixels));
                self.gpu.flush();
                self.app.icon_slots = atlas.slots;
                self.app.entries = entries;
                self.refilter();
                self.redraw_all();
            }
        }
    }

    fn refilter(&mut self) {
        let mut matcher = launcher::new_matcher();
        self.app.results = launcher::filter(&self.app.entries, &self.app.query, &self.app.launch_counts, &mut matcher);
        self.app.selected = 0;
    }

    fn render_dirty(&mut self) {
        let now = Instant::now();
        let State { gpu, outputs, app, queue_handle, presentation, latency_pending, always_mapped, .. } = self;
        for view in outputs.iter_mut() {
            if !view.needs_redraw || !view.configured || view.logical.0 == 0 {
                continue;
            }
            if app.mode == Mode::Hidden && !*always_mapped {
                continue;
            }
            view.needs_redraw = false;
            let scale = view.scale120 as f32 / 120.0;
            let physical = ((view.logical.0 as f32 * scale).round() as u32, (view.logical.1 as f32 * scale).round() as u32);
            gpu.resize(&mut view.target, physical.0, physical.1);
            view.viewport.set_destination(view.logical.0 as i32, view.logical.1 as i32);

            // Blur source: the newest capture of the workspace now visible on this output.
            if let Some(workspace) = app.snapshot.visible_workspace(&view.name) {
                let key = (view.name.clone(), workspace.name.clone());
                if let Some((image, serial)) = app.thumbnails.get(&key) {
                    if view.blurred.as_ref().is_none_or(|(_, existing)| existing != &(workspace.name.clone(), *serial)) {
                        view.blurred = Some((gpu.blur(image), (workspace.name.clone(), *serial)));
                    }
                }
            }

            let (list, hits, animating) = if app.mode == Mode::Hidden { (render::DrawList::default(), Vec::new(), false) } else { ui::draw(gpu, app, view, physical, scale, now) };
            view.hits = hits;
            let surface = view.surface.clone();
            if animating {
                surface.frame(queue_handle, smithay_client_toolkit::compositor::FrameCallbackData(surface.clone()));
            }
            let measuring = latency_pending.iter().any(|(pending, _)| pending == &surface);
            let presented = gpu.render(&mut view.target, &list, || {
                if measuring {
                    let _ = presentation.feedback(&surface, queue_handle);
                }
            });
            if !presented {
                view.needs_redraw = true;
            }
        }
    }

    fn key(&mut self, event: KeyEvent) {
        let app = &mut self.app;
        if app.mode == Mode::Hidden {
            return;
        }
        let control = self.modifiers.ctrl;
        let keysym = event.keysym;
        if keysym == Keysym::Escape {
            self.hide();
            return;
        }
        // Alt: navigation between panels, and Alt+digit switches workspace (sway's own
        // bindings are off while the overlay shows).
        if self.modifiers.alt {
            let panel_index = PANELS.iter().position(|panel| *panel == app.focus);
            match keysym {
                Keysym::h | Keysym::Left | Keysym::l | Keysym::Right if app.focus == Focus::Workspaces => {
                    let forward = matches!(keysym, Keysym::l | Keysym::Right);
                    self.move_workspace_selection(forward);
                }
                Keysym::h | Keysym::Left => app.focus = PANELS[panel_index.map_or(1, |index| index.saturating_sub(1))],
                Keysym::l | Keysym::Right => app.focus = PANELS[panel_index.map_or(1, |index| (index + 1).min(PANELS.len() - 1))],
                Keysym::k | Keysym::Up => {
                    if app.focus != Focus::Workspaces {
                        app.last_panel = app.focus;
                    }
                    app.focus = Focus::Workspaces;
                }
                Keysym::j | Keysym::Down => app.focus = app.last_panel,
                _ => {
                    if let Some(digit) = keysym.key_char().and_then(|character| character.to_digit(10)) {
                        let number = if digit == 0 { 10 } else { digit };
                        self.hide();
                        sway::run_command(&format!("workspace number {number}"));
                        return;
                    }
                }
            }
            self.redraw_all();
            return;
        }
        if keysym == Keysym::Tab || keysym == Keysym::ISO_Left_Tab {
            let index = PANELS.iter().position(|panel| *panel == app.focus).unwrap_or(1);
            let step = if keysym == Keysym::ISO_Left_Tab { PANELS.len() - 1 } else { 1 };
            app.focus = PANELS[(index + step) % PANELS.len()];
            self.redraw_all();
            return;
        }
        let text = event.utf8.filter(|text| !text.is_empty() && !control && !self.modifiers.alt && !text.chars().any(char::is_control));
        match app.focus {
            Focus::Workspaces => match keysym {
                Keysym::h | Keysym::Left => self.move_workspace_selection(false),
                Keysym::l | Keysym::Right => self.move_workspace_selection(true),
                Keysym::j | Keysym::Down => app.focus = app.last_panel,
                Keysym::Return | Keysym::KP_Enter => {
                    let name = app.workspace_selected.clone();
                    self.act(Action::Workspace(name));
                    return;
                }
                _ => {}
            },
            Focus::Files => {
                let browser = &mut app.files;
                let page = 10;
                match keysym {
                    Keysym::j | Keysym::Down => files::move_selection(browser, 1),
                    Keysym::k | Keysym::Up => files::move_selection(browser, -1),
                    Keysym::d if control => files::move_selection(browser, page),
                    Keysym::u if control => files::move_selection(browser, -page),
                    Keysym::g => browser.selected = 0,
                    Keysym::G => browser.selected = browser.entries.len().saturating_sub(1),
                    Keysym::h | Keysym::Left | Keysym::BackSpace => files::parent(browser),
                    Keysym::l | Keysym::Right | Keysym::Return | Keysym::KP_Enter => {
                        let index = browser.selected;
                        self.act(Action::OpenFile(index));
                        return;
                    }
                    Keysym::period => {
                        browser.show_hidden = !browser.show_hidden;
                        files::read(browser);
                    }
                    Keysym::asciitilde => files::change_directory(browser, files::home(), None),
                    _ => {}
                }
            }
            Focus::Launcher => {
                let count = app.results.len().min(ui::VISIBLE_RESULTS);
                match keysym {
                    Keysym::Return | Keysym::KP_Enter => {
                        self.launch_selected();
                        return;
                    }
                    Keysym::Up => app.selected = app.selected.saturating_sub(1),
                    Keysym::Down => app.selected = (app.selected + 1).min(count.saturating_sub(1)),
                    Keysym::k | Keysym::p if control => app.selected = app.selected.saturating_sub(1),
                    Keysym::j | Keysym::n if control => app.selected = (app.selected + 1).min(count.saturating_sub(1)),
                    Keysym::u if control => {
                        app.query.clear();
                        self.refilter();
                    }
                    Keysym::BackSpace | Keysym::w if keysym == Keysym::BackSpace || control => {
                        if control {
                            let cut = notes::previous_word(&app.query, app.query.len());
                            app.query.truncate(cut);
                        } else {
                            app.query.pop();
                        }
                        self.refilter();
                    }
                    _ => {
                        if let Some(text) = text {
                            app.query.push_str(&text);
                            self.refilter();
                        }
                    }
                }
            }
            Focus::Notes => {
                let cursor = app.notes_cursor;
                let notes_text = &mut app.notes;
                let mut edited = true;
                match keysym {
                    Keysym::Return | Keysym::KP_Enter => {
                        notes_text.insert(cursor, '\n');
                        app.notes_cursor += 1;
                    }
                    Keysym::BackSpace => {
                        let start = if control { notes::previous_word(notes_text, cursor) } else { notes::previous_boundary(notes_text, cursor) };
                        notes_text.replace_range(start..cursor, "");
                        app.notes_cursor = start;
                    }
                    Keysym::Delete => {
                        let end = notes::next_boundary(notes_text, cursor);
                        notes_text.replace_range(cursor..end, "");
                    }
                    _ => {
                        edited = false;
                        match keysym {
                            Keysym::Left => app.notes_cursor = notes::previous_boundary(notes_text, cursor),
                            Keysym::Right => app.notes_cursor = notes::next_boundary(notes_text, cursor),
                            Keysym::Up => app.notes_cursor = notes::vertical(notes_text, cursor, false),
                            Keysym::Down => app.notes_cursor = notes::vertical(notes_text, cursor, true),
                            Keysym::Home => app.notes_cursor = notes::line_start(notes_text, cursor),
                            Keysym::End => app.notes_cursor = notes::line_end(notes_text, cursor),
                            _ => {
                                if let Some(text) = text {
                                    notes_text.insert_str(cursor, &text);
                                    app.notes_cursor += text.len();
                                    edited = true;
                                }
                            }
                        }
                    }
                }
                if edited {
                    app.notes_dirty = true;
                    self.next_notes_save = Some(Instant::now() + Duration::from_secs(2));
                }
            }
        }
        self.redraw_all();
    }

    /// Steps through the focused output's workspaces, in the order the row shows them.
    fn move_workspace_selection(&mut self, forward: bool) {
        let snapshot = &self.app.snapshot;
        let output = snapshot.focused_output().unwrap_or_default();
        let names: Vec<&str> = snapshot.workspaces.iter().filter(|workspace| workspace.output == output).map(|workspace| workspace.name.as_str()).collect();
        let Some(current) = names.iter().position(|name| *name == self.app.workspace_selected).or((!names.is_empty()).then_some(0)) else { return };
        let next = if forward { (current + 1).min(names.len() - 1) } else { current.saturating_sub(1) };
        self.app.workspace_selected = names[next].to_string();
    }

    fn launch(&mut self, index: usize) {
        let Some(entry) = self.app.entries.get(index).cloned() else { return };
        *self.app.launch_counts.entry(entry.name.clone()).or_default() += 1;
        launcher::save_launch_counts(&self.app.launch_counts);
        self.hide();
        launcher::launch(&entry);
    }

    fn launch_selected(&mut self) {
        if let Some(&index) = self.app.results.get(self.app.selected) {
            self.launch(index);
        }
    }

    fn act(&mut self, action: Action) {
        match action {
            Action::Workspace(name) => {
                self.hide();
                sway::run_command(&format!("workspace \"{}\"", name.replace('"', "\\\"")));
            }
            Action::Launch(index) => self.launch(index),
            Action::Focus(focus) => {
                self.app.focus = focus;
                self.redraw_all();
            }
            Action::OpenFile(index) => {
                let browser = &mut self.app.files;
                browser.selected = index.min(browser.entries.len().saturating_sub(1));
                match files::enter(browser) {
                    // sway spawns it, so it is not our child.
                    Some(path) => {
                        self.hide();
                        sway::run_command(&format!("exec xdg-open {}", files::shell_quote(&path.to_string_lossy())));
                    }
                    None => self.redraw_all(),
                }
            }
            Action::ToggleIdleInhibit => {
                self.set_idle_inhibited(!self.app.idle_inhibited);
                self.redraw_all();
            }
        }
    }
}

impl CompositorHandler for State {
    fn scale_factor_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, surface: &wl_surface::WlSurface, factor: i32) {
        // Integer scale only matters without fractional-scale.
        if self.fractional_manager.is_none() {
            if let Some(view) = self.outputs.iter_mut().find(|view| &view.surface == surface) {
                view.scale120 = factor as u32 * 120;
                view.needs_redraw = true;
            }
        }
    }
    fn transform_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: wl_output::Transform) {}
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, surface: &wl_surface::WlSurface, _: u32) {
        if let Some(view) = self.outputs.iter_mut().find(|view| &view.surface == surface) {
            view.needs_redraw = true;
        }
    }
    fn surface_enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
    fn surface_leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
}

impl OutputHandler for State {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, output: wl_output::WlOutput) {
        self.create_view(output);
    }
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, output: wl_output::WlOutput) {
        self.outputs.retain(|view| view.output != output);
    }
}

struct LayerData {
    surface: wl_surface::WlSurface,
    dim: bool,
}

impl Dispatch2<ZwlrLayerSurfaceV1, State> for LayerData {
    fn event(&self, state: &mut State, role: &ZwlrLayerSurfaceV1, event: zwlr_layer_surface_v1::Event, _: &Connection, _: &QueueHandle<State>) {
        match event {
            zwlr_layer_surface_v1::Event::Configure { serial, width, height } => {
                role.ack_configure(serial);
                state.layer_configured(&self.surface, self.dim, (width, height));
            }
            zwlr_layer_surface_v1::Event::Closed => {
                log::warn!("layer surface closed by the compositor");
                if state.idle_inhibit_role.as_ref() == Some(role) {
                    // Its output went away: map again on whichever output sway picks now.
                    state.set_idle_inhibited(state.app.idle_inhibited);
                    return;
                }
                for view in &mut state.outputs {
                    if view.role.as_ref() == Some(role) || view.dim_role.as_ref() == Some(role) {
                        view.configured = false;
                    }
                }
            }
            _ => {}
        }
    }
}

impl SeatHandler for State {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }
    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
    fn new_capability(&mut self, _: &Connection, queue_handle: &QueueHandle<Self>, seat: wl_seat::WlSeat, capability: Capability) {
        if capability == Capability::Keyboard && self.keyboard.is_none() {
            self.keyboard = self
                .seat_state
                .get_keyboard_with_repeat(queue_handle, &seat, None, self.loop_handle.clone(), Box::new(|state: &mut State, _, event| state.key(event)))
                .ok();
        }
        if capability == Capability::Pointer && self.pointer.is_none() {
            self.pointer = self.seat_state.get_pointer(queue_handle, &seat).ok();
        }
    }
    fn remove_capability(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat, capability: Capability) {
        if capability == Capability::Keyboard {
            self.keyboard.take().map(|keyboard| keyboard.release());
        }
        if capability == Capability::Pointer {
            self.pointer.take().map(|pointer| pointer.release());
        }
    }
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl KeyboardHandler for State {
    fn enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: &wl_surface::WlSurface, _: u32, _: &[u32], _: &[Keysym]) {
    }
    fn leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: &wl_surface::WlSurface, _: u32) {
    }
    fn press_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, event: KeyEvent) {
        self.key(event);
    }
    fn repeat_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, _: KeyEvent) {}
    fn release_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, _: KeyEvent) {}
    fn update_modifiers(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, modifiers: Modifiers, _: RawModifiers, _: u32) {
        self.modifiers = modifiers;
    }
}

impl PointerHandler for State {
    fn pointer_frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_pointer::WlPointer, events: &[PointerEvent]) {
        for event in events {
            let PointerEventKind::Press { button: 0x110, .. } = event.kind else { continue };
            let Some(view) = self.outputs.iter().find(|view| view.surface == event.surface) else { continue };
            let scale = view.scale120 as f32 / 120.0;
            let (x, y) = (event.position.0 as f32 * scale, event.position.1 as f32 * scale);
            let hit = view.hits.iter().rev().find(|(rect, _)| x >= rect[0] && y >= rect[1] && x < rect[0] + rect[2] && y < rect[1] + rect[3]).map(|(_, action)| action.clone());
            match hit {
                Some(action) => self.act(action),
                None if self.app.mode == Mode::Open => self.hide(),
                None => {}
            }
        }
    }
}

impl PresentationTimeHandler for State {
    fn presentation_time_state(&mut self) -> &mut PresentationTimeState {
        &mut self.presentation
    }
    fn presented(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wp_presentation_feedback::WpPresentationFeedback, surface: &wl_surface::WlSurface, _: Vec<wl_output::WlOutput>, time: PresentTime, _: u32, _: u64, flags: WEnum<wp_presentation_feedback::Kind>) {
        let Some(origin) = self.latency_origin else { return };
        let Some(position) = self.latency_pending.iter().position(|(pending, _)| pending == surface) else { return };
        let (_, label) = self.latency_pending.remove(position);
        if time.clk_id != libc::CLOCK_MONOTONIC as u32 {
            return;
        }
        let origin_nanoseconds = monotonic_nanoseconds() - origin.elapsed().as_nanos() as i128;
        let presented = time.tv_sec as i128 * 1_000_000_000 + time.tv_nsec as i128;
        let zero_copy = matches!(flags, WEnum::Value(kind) if kind.contains(wp_presentation_feedback::Kind::ZeroCopy));
        log::info!("latency {label}: {:.1} ms from IPC event to scanout ({:?} mode, always_mapped = {}, zero_copy = {zero_copy})", (presented - origin_nanoseconds) as f64 / 1e6, self.app.mode, self.always_mapped);
    }
    fn discarded(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wp_presentation_feedback::WpPresentationFeedback, surface: &wl_surface::WlSurface) {
        log::debug!("presentation discarded for {:?}", surface.id());
    }
}

impl ShmHandler for State {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ProvidesRegistryState for State {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}

delegate_registry!(State);
smithay_client_toolkit::delegate_dispatch2!(State);
