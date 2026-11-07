//! Locking through the ext-session-lock Wayland protocol. While locked, the
//! compositor shows nothing but our lock surfaces and sends all input to
//! them, and should we crash, the session stays locked.

use std::ffi::c_void;
use std::ptr::NonNull;

use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::reexports::calloop::EventLoop;
use smithay_client_toolkit::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay_client_toolkit::reexports::calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::reexports::client::globals::{GlobalList, registry_queue_init};
use smithay_client_toolkit::reexports::client::protocol::wl_keyboard::WlKeyboard;
use smithay_client_toolkit::reexports::client::protocol::wl_output::{self, WlOutput};
use smithay_client_toolkit::reexports::client::protocol::wl_pointer::WlPointer;
use smithay_client_toolkit::reexports::client::protocol::wl_seat::WlSeat;
use smithay_client_toolkit::reexports::client::protocol::wl_surface::WlSurface;
use smithay_client_toolkit::reexports::client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle,
};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::seat::keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers};
use smithay_client_toolkit::seat::pointer::{PointerEvent, PointerEventKind, PointerHandler};
use smithay_client_toolkit::seat::{Capability, SeatHandler, SeatState};
use smithay_client_toolkit::session_lock::{
    SessionLock, SessionLockHandler, SessionLockState, SessionLockSurface,
    SessionLockSurfaceConfigure,
};
use smithay_client_toolkit::{
    delegate_compositor, delegate_keyboard, delegate_output, delegate_pointer, delegate_registry,
    delegate_seat, delegate_session_lock, registry_handlers,
};
use wayland_protocols_wlr::output_power_management::v1::client::{
    zwlr_output_power_manager_v1::ZwlrOutputPowerManagerV1,
    zwlr_output_power_v1::{self, ZwlrOutputPowerV1},
};
use winit::dpi::PhysicalSize;

use crate::{FRAME_INTERVAL, Idle, Input, Lock, Options, Output, auth, daemon, fail, goop};

/// A connection to a compositor that can lock the session.
pub struct Session {
    conn: Connection,
    globals: GlobalList,
    queue: EventQueue<App>,
}

impl Session {
    /// Connects to the Wayland compositor, if there is one and it supports
    /// ext-session-lock.
    pub fn connect() -> Option<Self> {
        // Without these, the connection would fall back to "wayland-0",
        // which in an X11 session may well be some other compositor.
        let named = |var| std::env::var_os(var).is_some_and(|v| !v.is_empty());
        if !named("WAYLAND_DISPLAY") && !named("WAYLAND_SOCKET") {
            return None;
        }
        let conn = Connection::connect_to_env().ok()?;
        let (globals, queue) = registry_queue_init(&conn).ok()?;
        let supported = globals.contents().with_list(|list| {
            list.iter()
                .any(|global| global.interface == "ext_session_lock_manager_v1")
        });
        if !supported {
            eprintln!(
                "lavalock: the compositor doesn't support ext-session-lock-v1, so other \
                 programs may still see what's typed"
            );
            return None;
        }
        Some(Self {
            conn,
            globals,
            queue,
        })
    }

    /// The `wl_display` for renderers to connect through.
    #[cfg_attr(not(feature = "renderer-gl"), allow(dead_code))]
    pub fn display(&self) -> NonNull<c_void> {
        NonNull::new(self.conn.backend().display_ptr().cast()).expect("No Wayland display")
    }
}

/// A Wayland surface for a renderer to draw into.
#[derive(Clone, Copy)]
pub struct SurfaceHandle {
    #[cfg_attr(feature = "renderer-gl", allow(dead_code))]
    display: NonNull<c_void>,
    surface: NonNull<c_void>,
}

/// The `wl_display` as glutin and pixels take it.
#[cfg(any(feature = "renderer-gl", feature = "renderer-pixels"))]
pub fn raw_display_handle(display: NonNull<c_void>) -> raw_window_handle::RawDisplayHandle {
    let mut handle = raw_window_handle::WaylandDisplayHandle::empty();
    handle.display = display.as_ptr();
    raw_window_handle::RawDisplayHandle::Wayland(handle)
}

#[cfg(any(feature = "renderer-gl", feature = "renderer-pixels"))]
impl SurfaceHandle {
    /// The `wl_surface` as glutin and pixels take it.
    pub fn raw_window_handle(&self) -> raw_window_handle::RawWindowHandle {
        let mut handle = raw_window_handle::WaylandWindowHandle::empty();
        handle.surface = self.surface.as_ptr();
        raw_window_handle::RawWindowHandle::Wayland(handle)
    }
}

#[cfg(feature = "renderer-pixels")]
unsafe impl raw_window_handle::HasRawWindowHandle for SurfaceHandle {
    fn raw_window_handle(&self) -> raw_window_handle::RawWindowHandle {
        SurfaceHandle::raw_window_handle(self)
    }
}

#[cfg(feature = "renderer-pixels")]
unsafe impl raw_window_handle::HasRawDisplayHandle for SurfaceHandle {
    fn raw_display_handle(&self) -> raw_window_handle::RawDisplayHandle {
        raw_display_handle(self.display)
    }
}

#[cfg(feature = "renderer-softbuffer")]
mod rwh_06 {
    use winit::raw_window_handle::{
        DisplayHandle, HandleError, HasDisplayHandle, HasWindowHandle, RawDisplayHandle,
        RawWindowHandle, WaylandDisplayHandle, WaylandWindowHandle, WindowHandle,
    };

    use super::SurfaceHandle;

    // Lock surfaces outlive the outputs drawing into them.
    impl HasWindowHandle for SurfaceHandle {
        fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
            let raw = RawWindowHandle::Wayland(WaylandWindowHandle::new(self.surface));
            Ok(unsafe { WindowHandle::borrow_raw(raw) })
        }
    }

    impl HasDisplayHandle for SurfaceHandle {
        fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
            let raw = RawDisplayHandle::Wayland(WaylandDisplayHandle::new(self.display));
            Ok(unsafe { DisplayHandle::borrow_raw(raw) })
        }
    }
}

/// Makes outputs that draw into Wayland surfaces.
pub trait Surfaces {
    fn create(&mut self, surface: SurfaceHandle, size: PhysicalSize<u32>) -> Box<dyn Output>;
}

/// Locks the session, and shows the goop on every output until unlocked.
pub fn run(
    session: Session,
    surfaces: Box<dyn Surfaces>,
    auth: auth::Authenticator,
    options: Options,
) {
    let Session {
        conn,
        globals,
        queue,
    } = session;
    let qh = queue.handle();
    let mut event_loop: EventLoop<App> = EventLoop::try_new()
        .unwrap_or_else(|err| fail(&format!("cannot create event loop: {err}")));

    let compositor = CompositorState::bind(&globals, &qh)
        .unwrap_or_else(|err| fail(&format!("no Wayland compositor: {err}")));
    let session_lock = SessionLockState::new(&globals, &qh)
        .lock(&qh)
        .unwrap_or_else(|err| fail(&format!("cannot lock the session: {err}")));

    // Turn the outputs off like wlopm does.
    let power = options.display_off.and_then(|timeout| {
        match globals.bind::<ZwlrOutputPowerManagerV1, _, _>(&qh, 1..=1, ()) {
            Ok(manager) => Some((Idle::new(timeout), manager)),
            Err(_) => {
                eprintln!(
                    "lavalock: cannot turn the displays off: the compositor doesn't \
                     support wlr-output-power-management-v1"
                );
                None
            }
        }
    });

    let mut app = App {
        screens: Vec::new(),
        surfaces,
        session_lock: Some(session_lock),
        locked: false,
        lock: Lock::new(auth),
        keyboard: None,
        pointer: None,
        registry: RegistryState::new(&globals),
        compositor,
        outputs: OutputState::new(&globals, &qh),
        seats: SeatState::new(&globals, &qh),
        conn: conn.clone(),
        qh: qh.clone(),
        power,
        exit: false,
    };

    WaylandSource::new(conn, queue)
        .insert(event_loop.handle())
        .unwrap_or_else(|err| fail(&format!("cannot watch the Wayland connection: {err}")));
    event_loop
        .handle()
        .insert_source(Timer::immediate(), |_, _, app| {
            app.tick();
            TimeoutAction::ToDuration(FRAME_INTERVAL)
        })
        .unwrap_or_else(|err| fail(&format!("cannot start the frame timer: {err}")));

    while !app.exit {
        event_loop
            .dispatch(None, &mut app)
            .unwrap_or_else(|err| fail(&format!("Wayland error: {err}")));
    }
    // In case the compositor unlocked the session while the outputs were off.
    app.wake();
    let _ = app.conn.flush();
}

/// The lock surface on one output.
struct Screen {
    // Dropped before the surface it draws into. Both are only there once
    // the compositor told us the size.
    output: Option<Box<dyn Output>>,
    scene: Option<goop::Scene>,
    surface: SessionLockSurface,
    wl_output: WlOutput,
    /// Controls the output's power while we keep it turned off.
    power: Option<ZwlrOutputPowerV1>,
}

impl Screen {
    fn power_off(&mut self, manager: &ZwlrOutputPowerManagerV1, qh: &QueueHandle<App>) {
        if self.power.is_none() {
            let power = manager.get_output_power(&self.wl_output, qh, ());
            power.set_mode(zwlr_output_power_v1::Mode::Off);
            self.power = Some(power);
        }
    }

    fn power_on(&mut self) {
        // Let go of the control again, as only one client may have it, and
        // the output keeps its mode anyway.
        if let Some(power) = self.power.take() {
            power.set_mode(zwlr_output_power_v1::Mode::On);
            power.destroy();
        }
    }
}

struct App {
    screens: Vec<Screen>,
    surfaces: Box<dyn Surfaces>,
    session_lock: Option<SessionLock>,
    /// Whether the compositor confirmed the lock.
    locked: bool,
    lock: Lock,
    keyboard: Option<WlKeyboard>,
    pointer: Option<WlPointer>,
    registry: RegistryState,
    compositor: CompositorState,
    outputs: OutputState,
    seats: SeatState,
    conn: Connection,
    qh: QueueHandle<App>,
    /// When to turn the outputs off, and what does it, if asked to.
    power: Option<(Idle, ZwlrOutputPowerManagerV1)>,
    exit: bool,
}

impl App {
    fn outputs_off(&self) -> bool {
        self.power.as_ref().is_some_and(|(idle, _)| idle.off())
    }

    /// Notes input, which turns the outputs back on.
    fn input(&mut self) {
        if let Some((idle, _)) = &mut self.power
            && idle.input()
        {
            self.wake();
        }
    }

    fn wake(&mut self) {
        for screen in &mut self.screens {
            screen.power_on();
        }
    }

    fn tick(&mut self) {
        self.lock
            .poll(self.screens.iter_mut().filter_map(|s| s.scene.as_mut()));
        // Unlocking before the compositor confirmed the lock would be a
        // protocol error.
        if self.locked
            && self
                .lock
                .unlocked(self.screens.iter().filter_map(|s| s.scene.as_ref()))
        {
            self.wake();
            // Dropping the lock unlocks the session.
            self.session_lock = None;
            let _ = self.conn.roundtrip();
            self.exit = true;
            return;
        }
        if let Some((idle, manager)) = &mut self.power
            && idle.expired()
        {
            for screen in &mut self.screens {
                screen.power_off(manager, &self.qh);
            }
        }
        // Keep the scenes going, e.g. to finish unlocking, but only draw
        // them while the outputs are on.
        let draw = !self.outputs_off();
        for screen in &mut self.screens {
            if let (Some(output), Some(scene)) = (&mut screen.output, &mut screen.scene) {
                scene.advance();
                if draw {
                    output.render(scene);
                }
            }
        }
    }
}

impl SessionLockHandler for App {
    fn locked(&mut self, _: &Connection, _: &QueueHandle<Self>, _: SessionLock) {
        self.locked = true;
        daemon::ready();
    }

    fn finished(&mut self, _: &Connection, _: &QueueHandle<Self>, _: SessionLock) {
        if !self.locked {
            fail("the compositor refused to lock the session; is another locker running?");
        }
        // The compositor unlocked the session itself.
        self.exit = true;
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        surface: SessionLockSurface,
        configure: SessionLockSurfaceConfigure,
        _: u32,
    ) {
        let Some(i) = self
            .screens
            .iter()
            .position(|s| s.surface.wl_surface() == surface.wl_surface())
        else {
            return;
        };
        // Draw at the output's resolution, like the windows do.
        let scale = self
            .outputs
            .info(&self.screens[i].wl_output)
            .map_or(1, |info| info.scale_factor.max(1));
        let (width, height) = configure.new_size;
        let size = PhysicalSize::new(width.max(1) * scale as u32, height.max(1) * scale as u32);
        let (w, h) = (size.width as f64, size.height as f64);

        // Continue what this screen showed, or else what the others show,
        // e.g. for an output plugged in while typing.
        let current = self.screens[i]
            .scene
            .as_ref()
            .or_else(|| self.screens.iter().find_map(|s| s.scene.as_ref()));
        let scene = match current {
            Some(scene) => scene.resized(w, h),
            None => goop::Scene::new(w, h),
        };

        let screen = &mut self.screens[i];
        screen.surface.wl_surface().set_buffer_scale(scale);
        let output = match &mut screen.output {
            Some(output) => {
                output.resize(size);
                output
            }
            None => {
                let handle = SurfaceHandle {
                    display: NonNull::new(self.conn.backend().display_ptr().cast())
                        .expect("No Wayland display"),
                    surface: NonNull::new(screen.surface.wl_surface().id().as_ptr().cast())
                        .expect("No Wayland surface"),
                };
                screen.output.insert(self.surfaces.create(handle, size))
            }
        };
        // Cover the output right away rather than with the next frame.
        output.render(&scene);
        screen.scene = Some(scene);
    }
}

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.outputs
    }

    fn new_output(&mut self, _: &Connection, qh: &QueueHandle<Self>, output: WlOutput) {
        let Some(session_lock) = &self.session_lock else {
            return;
        };
        let surface = self.compositor.create_surface(qh);
        let surface = session_lock.create_lock_surface(surface, &output, qh);
        let mut screen = Screen {
            output: None,
            scene: None,
            surface,
            wl_output: output,
            power: None,
        };
        // Some monitors come and go as they're turned off and on, so turn
        // new ones off too.
        if let Some((idle, manager)) = &self.power
            && idle.off()
        {
            screen.power_off(manager, qh);
        }
        self.screens.push(screen);
    }

    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlOutput) {}

    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, output: WlOutput) {
        self.screens.retain(|screen| screen.wl_output != output);
    }
}

impl SeatHandler for App {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seats
    }

    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlSeat) {}

    fn new_capability(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        seat: WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard && self.keyboard.is_none() {
            self.keyboard = self.seats.get_keyboard(qh, &seat, None).ok();
        }
        if capability == Capability::Pointer && self.pointer.is_none() {
            self.pointer = self.seats.get_pointer(qh, &seat).ok();
        }
    }

    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard
            && let Some(keyboard) = self.keyboard.take()
        {
            keyboard.release();
        }
        if capability == Capability::Pointer
            && let Some(pointer) = self.pointer.take()
        {
            pointer.release();
        }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlSeat) {}
}

impl KeyboardHandler for App {
    fn press_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        self.input();
        let input = match event.keysym {
            Keysym::Return | Keysym::KP_Enter => Input::Submit,
            Keysym::Escape | Keysym::BackSpace => Input::Clear,
            _ => event.utf8.as_deref().map_or(Input::Other, Input::Text),
        };
        let scenes = self.screens.iter_mut().filter_map(|s| s.scene.as_mut());
        self.lock.input(input, scenes);
    }

    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlKeyboard,
        _: &WlSurface,
        _: u32,
        _: &[u32],
        _: &[Keysym],
    ) {
    }

    fn leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlKeyboard,
        _: &WlSurface,
        _: u32,
    ) {
    }

    fn release_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlKeyboard,
        _: u32,
        _: KeyEvent,
    ) {
    }

    fn update_modifiers(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlKeyboard,
        _: u32,
        _: Modifiers,
    ) {
    }
}

impl PointerHandler for App {
    fn pointer_frame(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        pointer: &WlPointer,
        events: &[PointerEvent],
    ) {
        for event in events {
            match event.kind {
                // Hide the cursor, like the windows do.
                PointerEventKind::Enter { serial } => pointer.set_cursor(serial, None, 0, 0),
                PointerEventKind::Leave { .. } => {}
                _ => self.input(),
            }
        }
    }
}

impl CompositorHandler for App {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlSurface,
        _: i32,
    ) {
    }

    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlSurface,
        _: wl_output::Transform,
    ) {
    }

    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlSurface, _: u32) {}
}

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }

    registry_handlers![OutputState, SeatState];
}

impl Dispatch<ZwlrOutputPowerManagerV1, ()> for App {
    fn event(
        _: &mut Self,
        _: &ZwlrOutputPowerManagerV1,
        _: <ZwlrOutputPowerManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrOutputPowerV1, ()> for App {
    fn event(
        app: &mut Self,
        power: &ZwlrOutputPowerV1,
        event: zwlr_output_power_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // E.g. the output can't be turned off, or another client controls
        // its power. Nothing to do but leave it on.
        if let zwlr_output_power_v1::Event::Failed = event {
            for screen in &mut app.screens {
                if screen.power.as_ref() == Some(power) {
                    screen.power = None;
                }
            }
            power.destroy();
        }
    }
}

delegate_compositor!(App);
delegate_output!(App);
delegate_seat!(App);
delegate_keyboard!(App);
delegate_pointer!(App);
delegate_session_lock!(App);
delegate_registry!(App);
