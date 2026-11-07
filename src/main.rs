use std::rc::Rc;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

use winit::dpi::{LogicalSize, PhysicalSize};
use winit::event::{DeviceEvent, ElementState, Event, KeyEvent, StartCause, WindowEvent};
use winit::event_loop::{ControlFlow, DeviceEvents, EventLoop, EventLoopWindowTarget};
use winit::keyboard::{Key, NamedKey};
use winit::monitor::MonitorHandle;
use winit::window::{Fullscreen, Window, WindowBuilder};

#[cfg(all(
    feature = "renderer-gl",
    any(feature = "renderer-softbuffer", feature = "renderer-pixels")
))]
compile_error!("Select only one renderer feature at a time.");
#[cfg(all(feature = "renderer-softbuffer", feature = "renderer-pixels"))]
compile_error!("Select only one renderer feature at a time.");
#[cfg(not(any(
    feature = "renderer-gl",
    feature = "renderer-softbuffer",
    feature = "renderer-pixels"
)))]
compile_error!("Enable one of: renderer-gl, renderer-softbuffer, renderer-pixels.");

const FRAME_INTERVAL: Duration = Duration::from_millis(33);
const INITIAL_WIDTH: f64 = 600.0;
const INITIAL_HEIGHT: f64 = 400.0;

#[cfg(target_os = "linux")]
mod session_lock;

mod rng {
    use std::cell::Cell;
    use std::time::{SystemTime, UNIX_EPOCH};

    thread_local! {
        static STATE: Cell<u64> = Cell::new(initial_seed());
    }

    fn initial_seed() -> u64 {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9E37_79B9_7F4A_7C15);
        nanos.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1
    }

    fn next() -> u64 {
        STATE.with(|s| {
            let mut x = s.get();
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            s.set(x);
            x
        })
    }

    pub fn unit() -> f64 {
        ((next() >> 11) as f64) / ((1u64 << 53) as f64)
    }

    pub fn range(n: u64) -> u64 {
        if n == 0 { 0 } else { next() % n }
    }

    pub fn sign() -> f64 {
        if next() & 1 == 0 { 1.0 } else { -1.0 }
    }
}

mod goop {
    use std::f64::consts::TAU;

    use super::rng;

    pub struct Blob {
        pub x: f64,
        pub y: f64,
        dx: f64,
        dy: f64,
        torque: f64,
        th: f64,
        elasticity: f64,
        max_velocity: f64,
        min_r: f64,
        max_r: f64,
        r: Vec<f64>,
        /// Drawing size relative to the blob's natural size.
        scale: f64,
        /// Position within the current formation.
        slot: usize,
        /// Offsets the blob's wobble while in formation.
        phase: f64,
    }

    impl Blob {
        pub fn new(maxx: f64, maxy: f64, size: f64) -> Self {
            let max_r = size / 2.0;
            let min_r = (size / 10.0).max(5.0);
            let mid = (min_r + max_r) / 2.0;

            let elasticity = 1.8;
            let max_velocity = 1.2;
            let torque = 0.0075;
            let npoints = 5 + rng::range(5) as usize;

            let r = (0..npoints)
                .map(|_| (rng::unit() * mid + mid / 2.0) * rng::sign())
                .collect();

            Self {
                x: rng::unit() * maxx,
                y: rng::unit() * maxy,
                dx: rng::unit() * max_velocity * rng::sign(),
                dy: rng::unit() * max_velocity * rng::sign(),
                torque,
                th: TAU * rng::unit() * rng::sign(),
                elasticity,
                max_velocity,
                min_r,
                max_r,
                r,
                scale: 1.0,
                slot: 0,
                phase: TAU * rng::unit(),
            }
        }

        fn nominal_radius(&self) -> f64 {
            (self.min_r + self.max_r) / 2.0
        }

        fn extent(&self) -> f64 {
            self.r.iter().fold(0.0, |m: f64, r| m.max(r.abs())) * self.scale
        }

        /// Moves a fraction `k` of the way toward the given position and scale.
        fn follow(&mut self, x: f64, y: f64, scale: f64, k: f64) {
            self.x += (x - self.x) * k;
            self.y += (y - self.y) * k;
            self.scale += (scale - self.scale) * k;
        }

        pub fn throb(&mut self) {
            for slot in self.r.iter_mut() {
                let r_old = *slot;
                let dir_strict = if r_old > 0.0 { 1.0 } else { -1.0 };
                let dir_nonneg = if r_old >= 0.0 { 1.0 } else { -1.0 };

                let mut ra = r_old.abs();
                ra += rng::unit() * self.elasticity * dir_strict;
                let mut r = ra * dir_nonneg;

                if (ra > self.max_r && r >= 0.0) || (ra < self.min_r && r < 0.0) {
                    r = -r;
                } else if rng::range(50) == 0 {
                    r = -r;
                }

                *slot = r;
            }
        }

        pub fn step(&mut self, maxx: f64, maxy: f64) {
            self.drift(maxx, maxy);
            self.spin();
        }

        fn drift(&mut self, maxx: f64, maxy: f64) {
            self.x += self.dx;
            self.y += self.dy;

            if (self.x > maxx && self.dx >= 0.0) || (self.x < 0.0 && self.dx < 0.0) {
                self.dx = -self.dx;
            }
            if (self.y > maxy && self.dy >= 0.0) || (self.y < 0.0 && self.dy < 0.0) {
                self.dy = -self.dy;
            }

            if rng::range(10) == 0 {
                self.dx += rng::unit() * (self.max_velocity / 2.0) * rng::sign();
                self.dy += rng::unit() * (self.max_velocity / 2.0) * rng::sign();
                if self.dx.abs() > self.max_velocity {
                    self.dx /= 2.0;
                }
                if self.dy.abs() > self.max_velocity {
                    self.dy /= 2.0;
                }
            }
        }

        fn spin(&mut self) {
            let d = self.torque * rng::unit();
            let mut th = if self.th < 0.0 {
                -(self.th + d)
            } else {
                self.th + d
            };
            if th > TAU {
                th -= TAU;
            } else if th < 0.0 {
                th += TAU;
            }
            self.th = if self.th > 0.0 { th } else { -th };

            if rng::range(100) == 0 {
                self.th = -self.th;
            }
        }

        pub fn control_points(&self) -> Vec<(f64, f64)> {
            let n = self.r.len();
            let frac = TAU / n as f64;
            let th = self.th.abs();
            (0..n)
                .map(|i| {
                    let ra = self.r[i].abs() * self.scale;
                    let a = i as f64 * frac + th;
                    (self.x + ra * a.cos(), self.y + ra * a.sin())
                })
                .collect()
        }
    }

    pub struct Layer {
        pub blobs: Vec<Blob>,
        pub color: [u8; 3],
    }

    /// How the blobs are arranged.
    #[derive(Clone, Copy, PartialEq, Eq)]
    pub enum Formation {
        /// Regular goop: the blobs roam the whole screen.
        Free,
        /// Small blobs in a circle around the center, while typing.
        Ring,
        /// Small blobs in a horizontal line that shakes its head, then goes
        /// back to `Free` by itself.
        Line,
        /// The blobs fly outward while the background fades away.
        Burst,
    }

    // Formation geometry, relative to the shorter side of the screen.
    const BALL_RADIUS: f64 = 0.055;
    const RING_RADIUS: f64 = 0.22;
    const WOBBLE: f64 = 0.012;
    const SHAKE_AMPLITUDE: f64 = 0.08;

    // Formation timing, in frames.
    const LINE_FORM_FRAMES: u32 = 8;
    const SHAKE_FRAMES: u32 = 18;
    const SHAKES: f64 = 3.0;
    /// Each turn of the ring takes 3 frames, plus one to tell turns apart.
    const TURN_FRAMES: u32 = 4;
    const FADE_FRAMES: u32 = 10;
    const MAX_BURST_FRAMES: u32 = 60;

    pub struct Scene {
        pub layers: Vec<Layer>,
        pub width: f64,
        pub height: f64,
        formation: Formation,
        /// Frames since entering the current formation.
        frame: u32,
        /// Frames since the scene was created.
        clock: u64,
        /// Angle of the ring, chasing `rotation_target`.
        rotation: f64,
        rotation_target: f64,
        /// Whether the ring keeps turning on its own.
        spinning: bool,
        /// Turns still to be made, one after the other.
        pending_turns: u32,
        /// Frames until the next pending turn may start.
        turn_cooldown: u32,
    }

    impl Scene {
        pub fn new(width: f64, height: f64) -> Self {
            // xenocara default on a 24-bit visual: 2 + LRAND() % (depth - 2).
            let nlayers = 2 + rng::range(22) as usize;

            // Integer-truncated sizes matching the C math.
            let blob_max = (width.min(height) * 0.5).floor();
            let blob_min = (blob_max * 2.0 / 3.0).floor();
            let span = (blob_max - blob_min + 1.0).max(1.0);

            // Distribute DEF_COUNT=12 blobs round-robin across layers, exactly
            // as the C does when count <= 0.
            let mut blobs_per_layer = vec![0usize; nlayers];
            let mut remaining = 12usize;
            while remaining > 0 {
                for slot in blobs_per_layer.iter_mut() {
                    if remaining == 0 {
                        break;
                    }
                    *slot += 1;
                    remaining -= 1;
                }
            }

            let layers = (0..nlayers)
                .map(|i| {
                    let n = blobs_per_layer[i];
                    let blobs = (0..n)
                        .map(|_| {
                            let size = (rng::range(span as u64) as f64) + blob_min;
                            Blob::new(width, height, size)
                        })
                        .collect();
                    Layer {
                        blobs,
                        color: random_layer_color(),
                    }
                })
                .collect();

            Self {
                layers,
                width,
                height,
                formation: Formation::Free,
                frame: 0,
                clock: 0,
                rotation: 0.0,
                rotation_target: 0.0,
                spinning: false,
                pending_turns: 0,
                turn_cooldown: 0,
            }
        }

        /// A new scene of the given size in the same formation as this one.
        pub fn resized(&self, width: f64, height: f64) -> Self {
            let mut scene = Self::new(width, height);
            scene.spinning = self.spinning;
            scene.set_formation(self.formation);
            scene
        }

        fn blobs(&self) -> impl Iterator<Item = &Blob> {
            self.layers.iter().flat_map(|layer| layer.blobs.iter())
        }

        fn blobs_mut(&mut self) -> impl Iterator<Item = &mut Blob> {
            self.layers
                .iter_mut()
                .flat_map(|layer| layer.blobs.iter_mut())
        }

        fn center(&self) -> (f64, f64) {
            (self.width / 2.0, self.height / 2.0)
        }

        fn unit(&self) -> f64 {
            self.width.min(self.height)
        }

        /// Numbers the blobs in the order of `key`.
        fn assign_slots(&mut self, key: impl Fn(&Blob) -> f64) {
            let mut blobs: Vec<&mut Blob> = self.blobs_mut().collect();
            blobs.sort_by(|a, b| key(a).total_cmp(&key(b)));
            for (slot, blob) in blobs.into_iter().enumerate() {
                blob.slot = slot;
            }
        }

        pub fn formation(&self) -> Formation {
            self.formation
        }

        pub fn set_formation(&mut self, formation: Formation) {
            if formation == self.formation {
                return;
            }
            let (cx, cy) = self.center();
            match formation {
                Formation::Free => {}
                Formation::Ring => {
                    // Keep the order the blobs already have around the
                    // center, and turn the ring so they travel the least.
                    let angle = |b: &Blob| (b.y - cy).atan2(b.x - cx);
                    self.assign_slots(angle);
                    let step = TAU / self.blobs().count().max(1) as f64;
                    let (sx, sy) = self.blobs().fold((0.0, 0.0), |(sx, sy), b| {
                        let a = angle(b) - b.slot as f64 * step;
                        (sx + a.cos(), sy + a.sin())
                    });
                    self.rotation = sy.atan2(sx);
                    self.rotation_target = self.rotation;
                }
                Formation::Line => self.assign_slots(|b| b.x),
                Formation::Burst => {
                    let speed = 0.02 * self.unit();
                    for blob in self.blobs_mut() {
                        let (dx, dy) = (blob.x - cx, blob.y - cy);
                        let d = dx.hypot(dy);
                        let a = if d > 1e-6 {
                            dy.atan2(dx)
                        } else {
                            TAU * rng::unit()
                        };
                        blob.dx = a.cos() * speed;
                        blob.dy = a.sin() * speed;
                    }
                }
            }
            self.formation = formation;
            self.frame = 0;
            self.pending_turns = 0;
        }

        /// Turns the ring a bit further, `turns` times in a row.
        pub fn nudge(&mut self, turns: u32) {
            self.pending_turns += turns;
        }

        pub fn set_spinning(&mut self, spinning: bool) {
            self.spinning = spinning;
        }

        /// Opacity of the black background.
        #[cfg_attr(not(feature = "renderer-gl"), allow(dead_code))]
        pub fn background_alpha(&self) -> f64 {
            match self.formation {
                Formation::Burst => (1.0 - self.frame as f64 / FADE_FRAMES as f64).max(0.0),
                _ => 1.0,
            }
        }

        /// Whether the burst is over: the background is gone and so are the
        /// blobs.
        pub fn finished(&self) -> bool {
            if self.formation != Formation::Burst {
                return false;
            }
            let gone = |b: &Blob| {
                let e = b.extent();
                b.x + e < 0.0 || b.x - e > self.width || b.y + e < 0.0 || b.y - e > self.height
            };
            self.frame >= MAX_BURST_FRAMES || (self.frame >= FADE_FRAMES && self.blobs().all(gone))
        }

        pub fn advance(&mut self) {
            self.frame = self.frame.saturating_add(1);
            self.clock += 1;
            if self.formation == Formation::Line && self.frame > LINE_FORM_FRAMES + SHAKE_FRAMES {
                self.set_formation(Formation::Free);
            }

            let (width, height) = (self.width, self.height);
            let (cx, cy) = self.center();
            let unit = self.unit();
            let n = self.blobs().count().max(1) as f64;
            let ball = BALL_RADIUS * unit;
            let clock = self.clock as f64;
            let wobble = |b: &Blob| {
                (
                    WOBBLE * unit * (clock * 0.05 + b.phase).sin(),
                    WOBBLE * unit * (clock * 0.037 + 1.7 * b.phase).cos(),
                )
            };

            match self.formation {
                Formation::Free => {
                    for blob in self.blobs_mut() {
                        blob.throb();
                        blob.step(width, height);
                        let (x, y) = (blob.x, blob.y);
                        blob.follow(x, y, 1.0, 0.1);
                    }
                }
                Formation::Ring => {
                    if self.spinning {
                        self.rotation_target += 0.1;
                    }
                    self.turn_cooldown = self.turn_cooldown.saturating_sub(1);
                    if self.pending_turns > 0 && self.turn_cooldown == 0 {
                        self.pending_turns -= 1;
                        self.turn_cooldown = TURN_FRAMES;
                        self.rotation_target += 0.12 + 0.18 * rng::unit();
                    }
                    // Settles within ~3 frames (0.1 s).
                    let turn = (self.rotation_target - self.rotation) * 0.78;
                    self.rotation += turn;
                    let rotation = self.rotation;
                    let radius = RING_RADIUS * unit;
                    let (sin, cos) = turn.sin_cos();
                    for blob in self.blobs_mut() {
                        // Turn the blobs along with the ring right away, so
                        // following their slots below doesn't slow it down.
                        let (dx, dy) = (blob.x - cx, blob.y - cy);
                        blob.x = cx + dx * cos - dy * sin;
                        blob.y = cy + dx * sin + dy * cos;

                        let a = rotation + blob.slot as f64 * TAU / n;
                        let (wx, wy) = wobble(blob);
                        let scale = ball / blob.nominal_radius();
                        blob.throb();
                        blob.spin();
                        // Gathers within ~9 frames (0.3 s).
                        blob.follow(
                            cx + radius * a.cos() + wx,
                            cy + radius * a.sin() + wy,
                            scale,
                            0.4,
                        );
                    }
                }
                Formation::Line => {
                    let shaking = self.frame.saturating_sub(LINE_FORM_FRAMES);
                    let (offset, k) = if shaking > 0 {
                        let p = shaking as f64 / SHAKE_FRAMES as f64;
                        let amplitude = SHAKE_AMPLITUDE * unit * (1.0 - 0.5 * p);
                        (amplitude * (TAU * SHAKES * p).sin(), 0.7)
                    } else {
                        (0.0, 0.35)
                    };
                    let spacing = (2.2 * ball).min(0.9 * width / n);
                    for blob in self.blobs_mut() {
                        let (wx, wy) = wobble(blob);
                        let x = cx + (blob.slot as f64 - (n - 1.0) / 2.0) * spacing + offset;
                        let scale = ball / blob.nominal_radius();
                        blob.throb();
                        blob.spin();
                        blob.follow(x + wx / 2.0, cy + wy / 2.0, scale, k);
                    }
                }
                Formation::Burst => {
                    for blob in self.blobs_mut() {
                        blob.x += blob.dx;
                        blob.y += blob.dy;
                        blob.dx *= 1.25;
                        blob.dy *= 1.25;
                        blob.scale *= 1.05;
                        blob.throb();
                        blob.spin();
                    }
                }
            }
        }
    }

    fn random_layer_color() -> [u8; 3] {
        // Match the xlockmore palette: smooth hue ramp at full saturation and
        // value, picked at random per layer.
        hsl_to_rgb(rng::unit(), 1.0, 0.5)
    }

    fn hsl_to_rgb(h: f64, s: f64, l: f64) -> [u8; 3] {
        let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
        let h_prime = (h * 6.0).rem_euclid(6.0);
        let x = c * (1.0 - (h_prime.rem_euclid(2.0) - 1.0).abs());
        let (r1, g1, b1) = match h_prime as u32 {
            0 => (c, x, 0.0),
            1 => (x, c, 0.0),
            2 => (0.0, c, x),
            3 => (0.0, x, c),
            4 => (x, 0.0, c),
            _ => (c, 0.0, x),
        };
        let m = l - c / 2.0;
        [
            ((r1 + m) * 255.0).clamp(0.0, 255.0) as u8,
            ((g1 + m) * 255.0).clamp(0.0, 255.0) as u8,
            ((b1 + m) * 255.0).clamp(0.0, 255.0) as u8,
        ]
    }

    pub struct BezierSection {
        pub p0: (f64, f64),
        pub p1: (f64, f64),
        pub p2: (f64, f64),
        pub p3: (f64, f64),
    }

    pub fn closed_spline_sections(c: &[(f64, f64)]) -> Vec<BezierSection> {
        let n = c.len();
        if n < 3 {
            return Vec::new();
        }
        let n_i = n as isize;
        let cp = |i: isize| {
            let idx = ((i % n_i) + n_i) as usize % n;
            c[idx]
        };
        (0..n)
            .map(|i| {
                let cm1 = cp(i as isize - 1);
                let c0 = cp(i as isize);
                let c1 = cp(i as isize + 1);
                let c2 = cp(i as isize + 2);
                let p1 = third(c0, c1);
                let p2 = third(c1, c0);
                let t1 = third(c0, cm1);
                let p0 = mid(t1, p1);
                let t2 = third(c1, c2);
                let p3 = mid(t2, p2);
                BezierSection { p0, p1, p2, p3 }
            })
            .collect()
    }

    fn third(a: (f64, f64), b: (f64, f64)) -> (f64, f64) {
        ((2.0 * a.0 + b.0) / 3.0, (2.0 * a.1 + b.1) / 3.0)
    }

    fn mid(a: (f64, f64), b: (f64, f64)) -> (f64, f64) {
        ((a.0 + b.0) / 2.0, (a.1 + b.1) / 2.0)
    }
}

#[cfg(not(any(target_os = "linux", target_os = "openbsd")))]
compile_error!("Unlocking is only implemented with Linux-PAM and BSD Authentication.");

mod auth {
    use std::ffi::CStr;
    use std::sync::atomic::{Ordering, compiler_fence};
    use std::sync::mpsc::{self, Receiver};
    use std::{ptr, thread};

    #[cfg(target_os = "openbsd")]
    use bsd_auth::Backend;
    #[cfg(target_os = "linux")]
    use pam::Backend;

    /// Longest password we accept, in bytes. Allocating it all up front means
    /// the buffer never moves, so there are no stray copies to wipe.
    const MAX_PASSWORD_LEN: usize = 255;

    /// A password that is wiped from memory when cleared or dropped.
    pub struct Password(Vec<u8>);

    impl Password {
        pub fn new() -> Self {
            Self(Vec::with_capacity(MAX_PASSWORD_LEN))
        }

        /// Appends a character, unless the password is already full.
        pub fn push(&mut self, ch: char) -> bool {
            let mut buf = [0u8; 4];
            let bytes = ch.encode_utf8(&mut buf).as_bytes();
            let fits = self.0.len() + bytes.len() <= MAX_PASSWORD_LEN;
            if fits {
                self.0.extend_from_slice(bytes);
            }
            wipe(&mut buf);
            fits
        }

        pub fn is_empty(&self) -> bool {
            self.0.is_empty()
        }

        pub fn clear(&mut self) {
            wipe(&mut self.0);
            self.0.clear();
        }

        /// Moves the password out, leaving this one empty.
        pub fn take(&mut self) -> Self {
            std::mem::replace(self, Self::new())
        }
    }

    impl Drop for Password {
        fn drop(&mut self) {
            self.clear();
        }
    }

    fn wipe(bytes: &mut [u8]) {
        for b in bytes {
            unsafe { ptr::write_volatile(b, 0) };
        }
        compiler_fence(Ordering::SeqCst);
    }

    /// Checks passwords for the user running us.
    pub struct Authenticator {
        backend: Backend,
    }

    impl Authenticator {
        /// Fails if we couldn't check passwords, which is better found out
        /// before locking the screen.
        pub fn new() -> Result<Self, String> {
            let pw = unsafe { libc::getpwuid(libc::getuid()) };
            if pw.is_null() {
                return Err("cannot look up the current user".into());
            }
            let user = unsafe { CStr::from_ptr((*pw).pw_name) }.to_owned();
            Ok(Self {
                backend: Backend::new(user)?,
            })
        }

        /// Checks the password in the background, as that may take a while
        /// (PAM usually delays failures on purpose).
        pub fn check(&self, password: Password) -> Receiver<bool> {
            let (tx, rx) = mpsc::channel();
            let backend = self.backend.clone();
            thread::spawn(move || {
                let ok = backend.authenticate(&password);
                drop(password);
                let _ = tx.send(ok);
            });
            rx
        }
    }

    #[cfg(target_os = "linux")]
    mod pam {
        use std::ffi::{CString, c_char, c_int, c_void};
        use std::path::Path;
        use std::ptr;

        use super::{Password, wipe};

        #[repr(C)]
        struct PamHandle {
            _private: [u8; 0],
        }

        #[repr(C)]
        struct PamMessage {
            msg_style: c_int,
            msg: *const c_char,
        }

        #[repr(C)]
        struct PamResponse {
            resp: *mut c_char,
            resp_retcode: c_int,
        }

        type ConvFn = unsafe extern "C" fn(
            c_int,
            *mut *const PamMessage,
            *mut *mut PamResponse,
            *mut c_void,
        ) -> c_int;

        #[repr(C)]
        struct PamConv {
            conv: Option<ConvFn>,
            appdata_ptr: *mut c_void,
        }

        const PAM_SUCCESS: c_int = 0;
        const PAM_BUF_ERR: c_int = 5;
        const PAM_CONV_ERR: c_int = 19;
        const PAM_REFRESH_CRED: c_int = 0x0010;
        const PAM_PROMPT_ECHO_OFF: c_int = 1;
        const PAM_PROMPT_ECHO_ON: c_int = 2;
        const PAM_MAX_NUM_MSG: c_int = 32;

        // Link the runtime library directly: the unversioned libpam.so only
        // comes with the development package.
        #[link(name = "libpam.so.0", kind = "dylib", modifiers = "+verbatim")]
        unsafe extern "C" {
            fn pam_start(
                service_name: *const c_char,
                user: *const c_char,
                pam_conversation: *const PamConv,
                pamh: *mut *mut PamHandle,
            ) -> c_int;
            fn pam_authenticate(pamh: *mut PamHandle, flags: c_int) -> c_int;
            fn pam_setcred(pamh: *mut PamHandle, flags: c_int) -> c_int;
            fn pam_end(pamh: *mut PamHandle, pam_status: c_int) -> c_int;
        }

        /// Answers PAM's prompts with the password `appdata` points to.
        unsafe extern "C" fn converse(
            num_msg: c_int,
            msg: *mut *const PamMessage,
            resp: *mut *mut PamResponse,
            appdata: *mut c_void,
        ) -> c_int {
            if num_msg <= 0 || num_msg > PAM_MAX_NUM_MSG || appdata.is_null() {
                return PAM_CONV_ERR;
            }
            let num_msg = num_msg as usize;
            let password = unsafe { &*(appdata as *const Password) };

            // PAM frees the replies with free(), so they must come from malloc.
            let replies =
                unsafe { libc::calloc(num_msg, size_of::<PamResponse>()) } as *mut PamResponse;
            if replies.is_null() {
                return PAM_BUF_ERR;
            }
            for i in 0..num_msg {
                let style = unsafe { (**msg.add(i)).msg_style };
                if style != PAM_PROMPT_ECHO_OFF && style != PAM_PROMPT_ECHO_ON {
                    continue;
                }
                let len = password.0.len();
                let copy = unsafe { libc::malloc(len + 1) } as *mut u8;
                if copy.is_null() {
                    unsafe { free_replies(replies, i) };
                    return PAM_BUF_ERR;
                }
                unsafe {
                    ptr::copy_nonoverlapping(password.0.as_ptr(), copy, len);
                    *copy.add(len) = 0;
                    (*replies.add(i)).resp = copy as *mut c_char;
                }
            }
            unsafe { *resp = replies };
            PAM_SUCCESS
        }

        /// Wipes and frees the first `n` replies and the array holding them.
        unsafe fn free_replies(replies: *mut PamResponse, n: usize) {
            for i in 0..n {
                let reply = unsafe { (*replies.add(i)).resp };
                if !reply.is_null() {
                    unsafe {
                        let len = libc::strlen(reply);
                        wipe(std::slice::from_raw_parts_mut(reply as *mut u8, len));
                        libc::free(reply as *mut c_void);
                    }
                }
            }
            unsafe { libc::free(replies as *mut c_void) };
        }

        #[derive(Clone)]
        pub struct Backend {
            service: CString,
            user: CString,
        }

        impl Backend {
            pub fn new(user: CString) -> Result<Self, String> {
                let backend = Self {
                    service: CString::new(service_name()).unwrap(),
                    user,
                };
                let password = Password::new();
                match backend.start(&password) {
                    Ok(pamh) => {
                        unsafe { pam_end(pamh, PAM_SUCCESS) };
                        Ok(backend)
                    }
                    Err(err) => Err(format!("cannot initialize PAM (error {err})")),
                }
            }

            fn start(&self, password: &Password) -> Result<*mut PamHandle, c_int> {
                let conv = PamConv {
                    conv: Some(converse),
                    appdata_ptr: password as *const Password as *mut c_void,
                };
                let mut pamh = ptr::null_mut();
                // PAM copies the conversation struct, but not the password.
                let ret = unsafe {
                    pam_start(self.service.as_ptr(), self.user.as_ptr(), &conv, &mut pamh)
                };
                if ret == PAM_SUCCESS {
                    Ok(pamh)
                } else {
                    Err(ret)
                }
            }

            pub fn authenticate(&self, password: &Password) -> bool {
                let Ok(pamh) = self.start(password) else {
                    return false;
                };
                unsafe {
                    let ret = pam_authenticate(pamh, 0);
                    if ret == PAM_SUCCESS {
                        // Like other lockers, e.g. to renew Kerberos tickets.
                        pam_setcred(pamh, PAM_REFRESH_CRED);
                    }
                    pam_end(pamh, ret);
                    ret == PAM_SUCCESS
                }
            }
        }

        /// Our own PAM service if the system has one, otherwise "login",
        /// which every distribution sets up for password logins. (The
        /// fallback for unknown services, "other", denies everything on
        /// some.)
        fn service_name() -> &'static str {
            let ours = ["/etc/pam.d/lavalock", "/usr/lib/pam.d/lavalock"];
            if ours.iter().any(|p| Path::new(p).exists()) {
                "lavalock"
            } else {
                "login"
            }
        }
    }

    #[cfg(target_os = "openbsd")]
    mod bsd_auth {
        use std::ffi::{CStr, CString, c_char, c_int};
        use std::ptr;

        use super::{Password, wipe};

        unsafe extern "C" {
            fn auth_userokay(
                name: *mut c_char,
                style: *mut c_char,
                ty: *mut c_char,
                password: *mut c_char,
            ) -> c_int;
        }

        /// Holds the login scripts that check passwords. Only the "auth"
        /// group may use them, so like xlock, we must be installed setgid
        /// auth.
        const AUTH_DIR: &CStr = c"/usr/libexec/auth";

        #[derive(Clone)]
        pub struct Backend {
            user: CString,
        }

        impl Backend {
            pub fn new(user: CString) -> Result<Self, String> {
                let usable = unsafe {
                    libc::faccessat(
                        libc::AT_FDCWD,
                        AUTH_DIR.as_ptr(),
                        libc::X_OK,
                        libc::AT_EACCESS,
                    )
                } == 0;
                if usable {
                    Ok(Self { user })
                } else {
                    Err(
                        "cannot use BSD Authentication; lavalock must be setgid auth \
                         (chown root:auth lavalock && chmod 2555 lavalock)"
                            .into(),
                    )
                }
            }

            pub fn authenticate(&self, password: &Password) -> bool {
                // auth_userokay takes mutable, NUL-terminated strings.
                let mut user = self.user.clone().into_bytes_with_nul();
                let mut pw = Vec::with_capacity(password.0.len() + 1);
                pw.extend_from_slice(&password.0);
                pw.push(0);
                // Without a style and type, the user's login class decides
                // how to authenticate.
                let ok = unsafe {
                    auth_userokay(
                        user.as_mut_ptr().cast(),
                        ptr::null_mut(),
                        ptr::null_mut(),
                        pw.as_mut_ptr().cast(),
                    )
                } != 0;
                wipe(&mut pw);
                ok
            }
        }
    }
}

/// Keeps all keyboard and pointer input to ourselves on X11, like slock, so
/// that no other program sees what's typed and no window manager shortcut
/// can get around us. Wayland compositors don't let programs do this.
mod grab {
    use std::ffi::c_uint;
    use std::time::{Duration, Instant};

    use winit::window::Window;
    use x11_dl::xlib;

    /// How long to keep trying at startup, e.g. while a menu that was open
    /// still holds the keyboard. slock waits as long.
    const STARTUP_TIMEOUT: Duration = Duration::from_secs(1);
    /// How often to grab again, in case we lost the grab, e.g. because the
    /// window got unmapped.
    const REGRAB_INTERVAL: Duration = Duration::from_secs(1);

    pub struct Grab {
        xlib: xlib::Xlib,
        display: *mut xlib::Display,
        window: xlib::Window,
        deadline: Instant,
        grabbed: bool,
        next_try: Instant,
    }

    impl Grab {
        /// Prepares grabbing input for the window, if it is on X11.
        pub fn new(window: &Window) -> Result<Option<Self>, String> {
            let Some((display, window)) = super::xlib_window(window) else {
                return Ok(None);
            };
            let display = display.ok_or("no X11 display to grab input on")?;
            let xlib = xlib::Xlib::open().map_err(|err| format!("cannot load Xlib: {err}"))?;
            let now = Instant::now();
            Ok(Some(Self {
                xlib,
                // Winit's own connection: grabs belong to a connection, and
                // winit has to receive the grabbed input.
                display: display.as_ptr().cast(),
                window,
                deadline: now + STARTUP_TIMEOUT,
                grabbed: false,
                next_try: now,
            }))
        }

        /// Whether we got hold of the keyboard and pointer by now.
        pub fn grabbed(&self) -> bool {
            self.grabbed
        }

        /// Grabs input, if it's time to. Fails if we couldn't grab it in time
        /// at startup.
        pub fn update(&mut self) -> Result<(), &'static str> {
            let now = Instant::now();
            if now < self.next_try {
                return Ok(());
            }
            let x = &self.xlib;
            let keyboard = unsafe {
                (x.XGrabKeyboard)(
                    self.display,
                    self.window,
                    xlib::True,
                    xlib::GrabModeAsync,
                    xlib::GrabModeAsync,
                    xlib::CurrentTime,
                )
            } == xlib::GrabSuccess;
            let mask = xlib::ButtonPressMask | xlib::ButtonReleaseMask | xlib::PointerMotionMask;
            let pointer = unsafe {
                (x.XGrabPointer)(
                    self.display,
                    self.window,
                    xlib::False,
                    mask as c_uint,
                    xlib::GrabModeAsync,
                    xlib::GrabModeAsync,
                    0, // Don't confine the pointer.
                    0, // Keep the window's (hidden) cursor.
                    xlib::CurrentTime,
                )
            } == xlib::GrabSuccess;

            if keyboard && pointer {
                self.grabbed = true;
                self.next_try = now + REGRAB_INTERVAL;
            } else if !self.grabbed && now >= self.deadline {
                return Err(if keyboard {
                    "cannot grab the pointer"
                } else {
                    "cannot grab the keyboard"
                });
            }
            Ok(())
        }
    }
}

/// Winit's Xlib display, and the window's id, if the window is on X11.
fn xlib_window(window: &Window) -> Option<(Option<std::ptr::NonNull<std::ffi::c_void>>, u64)> {
    use winit::raw_window_handle::{
        HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle,
    };

    let (Ok(display), Ok(handle)) = (window.display_handle(), window.window_handle()) else {
        return None;
    };
    match (display.as_raw(), handle.as_raw()) {
        (RawDisplayHandle::Xlib(display), RawWindowHandle::Xlib(handle)) => {
            Some((display.display, handle.window))
        }
        _ => None,
    }
}

/// Turns the displays off and on through the DPMS extension on X11. The X
/// server turns them back on by itself on input, too.
mod dpms {
    use winit::window::Window;
    use x11_dl::dpms::{self, Xext};
    use x11_dl::xlib::{self, Xlib};

    pub struct Dpms {
        xlib: Xlib,
        xext: Xext,
        display: *mut xlib::Display,
        /// Whether DPMS was disabled until we turned the displays off, so
        /// that we disable it again when turning them back on.
        enabled_by_us: bool,
    }

    impl Dpms {
        /// Fails if the window isn't on X11 or the server can't do DPMS.
        pub fn new(window: &Window) -> Result<Self, &'static str> {
            let display = match super::xlib_window(window) {
                Some((Some(display), _)) => display.as_ptr().cast(),
                _ => return Err("only on X11"),
            };
            let xlib = Xlib::open().map_err(|_| "cannot load Xlib")?;
            let xext = Xext::open().map_err(|_| "cannot load libXext")?;
            let (mut event_base, mut error_base) = (0, 0);
            let capable = unsafe {
                (xext.DPMSQueryExtension)(display, &mut event_base, &mut error_base) != 0
                    && (xext.DPMSCapable)(display) != 0
            };
            if !capable {
                return Err("the X server can't do DPMS");
            }
            Ok(Self {
                xlib,
                xext,
                display,
                enabled_by_us: false,
            })
        }

        pub fn set(&mut self, on: bool) {
            let x = &self.xext;
            unsafe {
                if on {
                    (x.DPMSForceLevel)(self.display, dpms::DPMSModeOn);
                    if std::mem::take(&mut self.enabled_by_us) {
                        (x.DPMSDisable)(self.display);
                    }
                } else {
                    // Forcing a level fails while DPMS is disabled.
                    let (mut level, mut enabled) = (0, 0);
                    (x.DPMSInfo)(self.display, &mut level, &mut enabled);
                    if enabled == 0 {
                        (x.DPMSEnable)(self.display);
                        self.enabled_by_us = true;
                    }
                    (x.DPMSForceLevel)(self.display, dpms::DPMSModeOff);
                }
                (self.xlib.XFlush)(self.display);
            }
        }
    }
}

/// The monitors to cover, one window each. Falls back to a single window on
/// the current monitor if the platform doesn't report any.
fn monitors(elwt: &EventLoopWindowTarget<()>) -> Vec<Option<MonitorHandle>> {
    let monitors: Vec<_> = elwt.available_monitors().map(Some).collect();
    if monitors.is_empty() {
        vec![None]
    } else {
        monitors
    }
}

fn window_builder(monitor: &Option<MonitorHandle>) -> WindowBuilder {
    let mut builder = WindowBuilder::new()
        .with_title("Goop")
        .with_inner_size(LogicalSize::new(INITIAL_WIDTH, INITIAL_HEIGHT));
    if let Some(monitor) = monitor {
        builder = builder.with_position(monitor.position());
    }
    builder.with_fullscreen(Some(Fullscreen::Borderless(monitor.clone())))
}

fn enter_fullscreen(window: &Window, monitor: &Option<MonitorHandle>) {
    window.set_fullscreen(Some(Fullscreen::Borderless(monitor.clone())));
    window.set_cursor_visible(window.fullscreen().is_none());
}

/// Going to the background once locked, so whoever started us can carry on,
/// e.g. suspend the system, knowing the screen is locked.
mod daemon {
    use std::sync::atomic::{AtomicI32, Ordering};

    /// Where to tell the waiting parent that we're locked, if it waits.
    static READY: AtomicI32 = AtomicI32::new(-1);

    /// Forks. The parent waits until the child is locked and then exits; or,
    /// should the child fail before, exits as the child did. Call it before
    /// starting any threads.
    pub fn start() -> Result<(), String> {
        let mut fds = [0; 2];
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } == -1 {
            return Err(format!(
                "cannot create pipe: {}",
                std::io::Error::last_os_error()
            ));
        }
        let [read_end, write_end] = fds;
        match unsafe { libc::fork() } {
            -1 => Err(format!("cannot fork: {}", std::io::Error::last_os_error())),
            0 => {
                unsafe {
                    libc::close(read_end);
                    libc::setsid();
                    libc::chdir(c"/".as_ptr());
                }
                READY.store(write_end, Ordering::Relaxed);
                Ok(())
            }
            child => {
                unsafe { libc::close(write_end) };
                let mut byte = 0u8;
                let n = loop {
                    let n = unsafe { libc::read(read_end, (&raw mut byte).cast(), 1) };
                    if n != -1
                        || std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
                    {
                        break n;
                    }
                };
                if n == 1 {
                    std::process::exit(0);
                }
                // The child exited without locking.
                let mut status = 0;
                let code = if unsafe { libc::waitpid(child, &mut status, 0) } == child
                    && libc::WIFEXITED(status)
                {
                    libc::WEXITSTATUS(status)
                } else {
                    1
                };
                std::process::exit(code);
            }
        }
    }

    /// Lets the parent exit, if it waits for us.
    pub fn ready() {
        let fd = READY.swap(-1, Ordering::Relaxed);
        if fd != -1 {
            unsafe {
                libc::write(fd, [1u8].as_ptr().cast(), 1);
                libc::close(fd);
            }
        }
    }
}

/// Draws scenes, into a window or a Wayland surface.
trait Output {
    fn resize(&mut self, size: PhysicalSize<u32>);
    fn render(&mut self, scene: &goop::Scene);
}

/// How often to ask for fullscreen again after the compositor took it away,
/// so we don't fight one that keeps refusing.
const MAX_FULLSCREEN_RETRIES: u32 = 3;

struct Screen<O> {
    // Dropped before the window it draws into.
    output: O,
    window: Rc<Window>,
    monitor: Option<MonitorHandle>,
    fullscreen_retries: u32,
    scene: goop::Scene,
}

impl<O: Output> Screen<O> {
    fn new((output, window, monitor): (O, Rc<Window>, Option<MonitorHandle>)) -> Self {
        let size = window.inner_size();
        Self {
            output,
            window,
            monitor,
            fullscreen_retries: 0,
            scene: goop::Scene::new(size.width as f64, size.height as f64),
        }
    }

    fn resize(&mut self, size: PhysicalSize<u32>) {
        self.output.resize(size);
        self.scene = self.scene.resized(size.width as f64, size.height as f64);
        let window = &self.window;
        // Some compositors (e.g. River) drop a window out of fullscreen when
        // the next window gets mapped, so request it again.
        if window.fullscreen().is_none() && self.fullscreen_retries < MAX_FULLSCREEN_RETRIES {
            self.fullscreen_retries += 1;
            enter_fullscreen(window, &self.monitor);
        }
        window.set_cursor_visible(window.fullscreen().is_none());
        window.request_redraw();
    }
}

/// What a key press means for the lock.
enum Input<'a> {
    /// Check the password.
    Submit,
    /// Forget what was typed so far.
    Clear,
    Text(&'a str),
    /// Any other key, e.g. Ctrl or Tab.
    Other,
}

impl Input<'_> {
    fn from_winit(event: &KeyEvent) -> Option<Input<'_>> {
        if event.state != ElementState::Pressed {
            return None;
        }
        match &event.logical_key {
            Key::Named(NamedKey::Enter) => Some(Input::Submit),
            Key::Named(NamedKey::Escape | NamedKey::Backspace) => Some(Input::Clear),
            _ => Some(event.text.as_deref().map_or(Input::Other, Input::Text)),
        }
    }
}

/// Tells when to turn the displays off: after a while without input.
struct Idle {
    timeout: Duration,
    last_input: Instant,
    off: bool,
}

impl Idle {
    fn new(timeout: Duration) -> Self {
        Self {
            timeout,
            last_input: Instant::now(),
            off: false,
        }
    }

    /// Notes input. Returns whether to turn the displays back on.
    fn input(&mut self) -> bool {
        self.last_input = Instant::now();
        std::mem::take(&mut self.off)
    }

    /// Returns whether to turn the displays off now.
    fn expired(&mut self) -> bool {
        if self.off || self.last_input.elapsed() < self.timeout {
            return false;
        }
        self.off = true;
        true
    }

    /// Whether the displays are off, so there's no point in drawing.
    fn off(&self) -> bool {
        self.off
    }
}

/// Brings up the ring on every screen, so it's clear where the password
/// goes, and turns it `turns` further where it's already up.
///
/// The turns are random, so that onlookers can't count the keys: 0-3 while
/// gathering into the ring, 1-4 afterwards for keys that typed something.
/// The same on every screen, or comparing screens would give the count away.
fn show_key(scenes: &mut [&mut goop::Scene], turns: u32) {
    let gathering_turns = rng::range(4) as u32;
    for scene in scenes {
        if scene.formation() == goop::Formation::Ring {
            scene.nudge(turns);
        } else {
            scene.set_formation(goop::Formation::Ring);
            scene.nudge(gathering_turns);
        }
    }
}

/// How long to wait for the next key before forgetting what was typed.
const TYPING_TIMEOUT: Duration = Duration::from_secs(5);

/// Password entry, and what the screens show for it.
struct Lock {
    auth: auth::Authenticator,
    password: auth::Password,
    /// The pending result while a password is being checked.
    checking: Option<Receiver<bool>>,
    unlocking: bool,
    /// Whether a key was typed since the password was last emptied.
    typing: bool,
    /// When the last key was pressed, while waiting for the next one.
    last_key: Option<Instant>,
}

impl Lock {
    fn new(auth: auth::Authenticator) -> Self {
        Self {
            auth,
            password: auth::Password::new(),
            checking: None,
            unlocking: false,
            typing: false,
            last_key: None,
        }
    }

    fn input<'a>(&mut self, input: Input, scenes: impl IntoIterator<Item = &'a mut goop::Scene>) {
        if self.checking.is_some() || self.unlocking {
            return;
        }
        let mut scenes: Vec<_> = scenes.into_iter().collect();
        self.last_key = Some(Instant::now());
        match input {
            Input::Submit => {
                if !self.password.is_empty() {
                    self.checking = Some(self.auth.check(self.password.take()));
                    self.typing = false;
                    self.last_key = None;
                    for scene in scenes {
                        scene.set_spinning(true);
                    }
                }
            }
            Input::Clear => {
                // Like any key, bring up the ring first. Once it's up, go
                // back to roaming freely.
                if !scenes
                    .iter()
                    .any(|s| s.formation() == goop::Formation::Ring)
                {
                    show_key(&mut scenes, 0);
                    return;
                }
                self.clear(scenes);
            }
            Input::Text(text) => {
                // People tend to press space to wake the screen before
                // typing their password, so leave it out as the first key,
                // but let the screens show it like any other.
                let swallow = !self.typing && text == " ";
                let mut typed = swallow;
                if !swallow {
                    for ch in text.chars().filter(|c| !c.is_control()) {
                        typed |= self.password.push(ch);
                    }
                }
                self.typing |= typed;
                let turns = if typed { 1 + rng::range(4) as u32 } else { 0 };
                show_key(&mut scenes, turns);
            }
            Input::Other => show_key(&mut scenes, 0),
        }
    }

    /// Forgets what was typed so far, and lets the blobs roam freely again.
    fn clear<'a>(&mut self, scenes: impl IntoIterator<Item = &'a mut goop::Scene>) {
        self.password.clear();
        self.typing = false;
        self.last_key = None;
        for scene in scenes {
            scene.set_formation(goop::Formation::Free);
        }
    }

    /// Picks up the result of a password check, if there is one by now, and
    /// clears the password once nobody typed for a while.
    fn poll<'a>(&mut self, scenes: impl IntoIterator<Item = &'a mut goop::Scene>) {
        if self
            .last_key
            .is_some_and(|time| time.elapsed() >= TYPING_TIMEOUT)
        {
            self.clear(scenes);
            return;
        }
        let Some(result) = &self.checking else {
            return;
        };
        let formation = match result.try_recv() {
            Err(TryRecvError::Empty) => return,
            Ok(true) => {
                self.unlocking = true;
                goop::Formation::Burst
            }
            Ok(false) | Err(TryRecvError::Disconnected) => goop::Formation::Line,
        };
        self.checking = None;
        for scene in scenes {
            scene.set_spinning(false);
            scene.set_formation(formation);
        }
    }

    /// Whether the screens are unlocked and done animating.
    fn unlocked<'a>(&self, mut scenes: impl Iterator<Item = &'a goop::Scene>) -> bool {
        self.unlocking && scenes.all(|scene| scene.finished())
    }
}

fn run_screens<O: Output>(
    event_loop: EventLoop<()>,
    outputs: Vec<(O, Rc<Window>, Option<MonitorHandle>)>,
    auth: auth::Authenticator,
    options: Options,
) {
    let mut screens: Vec<Screen<O>> = outputs.into_iter().map(Screen::new).collect();
    let mut lock = Lock::new(auth);
    let mut grab = grab::Grab::new(&screens[0].window).unwrap_or_else(|err| fail(&err));
    let mut idle = options.display_off.and_then(|timeout| {
        let dpms = dpms::Dpms::new(&screens[0].window)
            .inspect_err(|err| eprintln!("lavalock: cannot turn the displays off: {err}"))
            .ok()?;
        Some((Idle::new(timeout), dpms))
    });
    if idle.is_some() {
        // Raw input, as the window events include made-up cursor motion,
        // e.g. when the pointer enters a window.
        event_loop.listen_device_events(DeviceEvents::Always);
    }
    let mut next_tick = Instant::now();

    event_loop
        .run(move |event, elwt| {
            elwt.set_control_flow(ControlFlow::WaitUntil(next_tick));

            match event {
                Event::WindowEvent { window_id, event } => {
                    let Some(i) = screens.iter().position(|s| s.window.id() == window_id) else {
                        return;
                    };
                    match event {
                        WindowEvent::Resized(size) => screens[i].resize(size),
                        WindowEvent::ScaleFactorChanged {
                            mut inner_size_writer,
                            ..
                        } => {
                            let size = screens[i].window.inner_size();
                            let _ = inner_size_writer.request_inner_size(size);
                            screens[i].resize(size);
                        }
                        // Synthetic presses are for keys that were already
                        // held down when we got focus.
                        WindowEvent::KeyboardInput {
                            event,
                            is_synthetic: false,
                            ..
                        } => {
                            if let Some((idle, dpms)) = &mut idle
                                && idle.input()
                            {
                                dpms.set(true);
                            }
                            if let Some(input) = Input::from_winit(&event) {
                                lock.input(input, screens.iter_mut().map(|s| &mut s.scene));
                            }
                        }
                        WindowEvent::RedrawRequested => {
                            let screen = &mut screens[i];
                            screen.scene.advance();
                            screen.output.render(&screen.scene);
                            next_tick = Instant::now() + FRAME_INTERVAL;
                        }
                        _ => {}
                    }
                }
                Event::DeviceEvent {
                    event:
                        DeviceEvent::MouseMotion { .. }
                        | DeviceEvent::MouseWheel { .. }
                        | DeviceEvent::Button { .. }
                        | DeviceEvent::Key(_),
                    ..
                } => {
                    if let Some((idle, dpms)) = &mut idle
                        && idle.input()
                    {
                        dpms.set(true);
                    }
                }
                Event::NewEvents(StartCause::ResumeTimeReached { .. })
                | Event::NewEvents(StartCause::Init) => {
                    if lock.unlocked(screens.iter().map(|s| &s.scene)) {
                        if let Some((idle, dpms)) = &mut idle
                            && idle.off()
                        {
                            dpms.set(true);
                        }
                        elwt.exit();
                        return;
                    }
                    if let Some(grab) = &mut grab {
                        // Better not to lock at all than to only look locked.
                        grab.update().unwrap_or_else(|err| fail(err));
                    }
                    if grab.as_ref().is_none_or(|grab| grab.grabbed()) {
                        daemon::ready();
                    }
                    lock.poll(screens.iter_mut().map(|s| &mut s.scene));
                    if let Some((idle, dpms)) = &mut idle {
                        if idle.expired() {
                            dpms.set(false);
                        }
                        if idle.off() {
                            // Keep the scenes going, e.g. to finish
                            // unlocking, but don't draw them.
                            for screen in &mut screens {
                                screen.scene.advance();
                            }
                            next_tick = Instant::now() + FRAME_INTERVAL;
                            return;
                        }
                    }
                    for screen in &screens {
                        screen.window.request_redraw();
                    }
                }
                _ => {}
            }
        })
        .expect("Event loop error");
}

fn fail(err: &str) -> ! {
    eprintln!("lavalock: {err}");
    std::process::exit(1);
}

const USAGE: &str = "\
usage: lavalock [-n] [-d seconds]

  -n, --nofork               stay in the foreground until unlocked, instead
                             of going to the background once locked
  -d, --display-off seconds  turn the displays off after this long without
                             input (0, the default, never does)";

/// What the command line asks for.
#[derive(Clone, Copy)]
struct Options {
    /// How long to wait for input before turning the displays off.
    display_off: Option<Duration>,
    /// Whether to go to the background once locked.
    daemonize: bool,
}

impl Options {
    fn parse() -> Self {
        let usage = || -> ! {
            eprintln!("{USAGE}");
            std::process::exit(2);
        };
        let mut options = Self {
            display_off: None,
            daemonize: true,
        };
        let mut args = std::env::args_os().skip(1);
        while let Some(arg) = args.next() {
            match arg.to_str() {
                Some("-h" | "--help") => {
                    println!("{USAGE}");
                    std::process::exit(0);
                }
                Some("-n" | "--nofork") => options.daemonize = false,
                Some("-d" | "--display-off") => {
                    let seconds: u64 = args
                        .next()
                        .and_then(|s| s.to_str()?.parse().ok())
                        .unwrap_or_else(|| usage());
                    options.display_off = (seconds > 0).then(|| Duration::from_secs(seconds));
                }
                _ => usage(),
            }
        }
        options
    }
}

#[cfg(feature = "renderer-gl")]
use gl_app as renderer;
#[cfg(feature = "renderer-pixels")]
use pixels_app as renderer;
#[cfg(feature = "renderer-softbuffer")]
use softbuffer_app as renderer;

fn main() {
    let options = Options::parse();
    if options.daemonize {
        daemon::start().unwrap_or_else(|err| fail(&err));
    }
    // Make sure we'll be able to unlock before locking.
    let auth = auth::Authenticator::new().unwrap_or_else(|err| fail(&err));

    #[cfg(target_os = "linux")]
    if let Some(session) = session_lock::Session::connect() {
        renderer::run_session_lock(session, auth, options);
        return;
    }

    renderer::run(auth, options);
}

#[cfg(feature = "renderer-gl")]
mod gl_app {
    use super::*;
    use std::num::NonZeroU32;

    use femtovg::renderer::OpenGl;
    use femtovg::{Canvas, Color, Paint, Path};
    use glutin::config::{Config, ConfigTemplateBuilder};
    use glutin::context::{
        ContextApi, ContextAttributesBuilder, NotCurrentContext, PossiblyCurrentContext, Version,
    };
    use glutin::display::GetGlDisplay;
    use glutin::prelude::*;
    use glutin::surface::{Surface, SurfaceAttributesBuilder, SwapInterval, WindowSurface};
    use glutin_winit::DisplayBuilder;
    use raw_window_handle::{HasRawWindowHandle, RawWindowHandle};

    /// Transparent, so that the desktop shows once we fade out after
    /// unlocking.
    fn template() -> ConfigTemplateBuilder {
        ConfigTemplateBuilder::new()
            .with_alpha_size(8)
            .with_depth_size(24)
            .with_stencil_size(8)
            .with_transparency(true)
    }

    /// Prefers transparency, then more samples.
    fn best_config(configs: Box<dyn Iterator<Item = Config> + '_>) -> Config {
        let transparent = |c: &Config| c.supports_transparency().unwrap_or(false);
        configs
            .reduce(|accum, config| {
                let more_transparent = transparent(&config) && !transparent(&accum);
                let as_transparent = transparent(&config) == transparent(&accum);
                if more_transparent
                    || (as_transparent && config.num_samples() > accum.num_samples())
                {
                    config
                } else {
                    accum
                }
            })
            .expect("No GL config")
    }

    fn create_context(gl_config: &Config, handle: Option<RawWindowHandle>) -> NotCurrentContext {
        let context_attributes = ContextAttributesBuilder::new()
            .with_context_api(ContextApi::OpenGl(Some(Version::new(3, 3))))
            .build(handle);
        let fallback_attributes = ContextAttributesBuilder::new()
            .with_context_api(ContextApi::Gles(Some(Version::new(3, 0))))
            .build(handle);
        unsafe {
            gl_config
                .display()
                .create_context(gl_config, &context_attributes)
                .or_else(|_| {
                    gl_config
                        .display()
                        .create_context(gl_config, &fallback_attributes)
                })
                .expect("Failed to create GL context")
        }
    }

    fn create_surface(
        gl_config: &Config,
        handle: RawWindowHandle,
        size: PhysicalSize<u32>,
    ) -> Surface<WindowSurface> {
        let surface_attributes = SurfaceAttributesBuilder::<WindowSurface>::new().build(
            handle,
            NonZeroU32::new(size.width.max(1)).unwrap(),
            NonZeroU32::new(size.height.max(1)).unwrap(),
        );
        unsafe {
            gl_config
                .display()
                .create_window_surface(gl_config, &surface_attributes)
                .expect("Failed to create GL surface")
        }
    }

    pub fn run(auth: auth::Authenticator, options: Options) {
        let event_loop = EventLoop::new().expect("Failed to create event loop");
        let monitors = monitors(&event_loop);

        let window_builder = |monitor| window_builder(monitor).with_transparent(true);
        let display_builder =
            DisplayBuilder::new().with_window_builder(Some(window_builder(&monitors[0])));
        let (window, gl_config) = display_builder
            .build(&event_loop, template(), best_config)
            .expect("Failed to create display");

        // The remaining windows must use the same GL config so that a single
        // context can render into all of them.
        let mut windows = vec![window.expect("Window wasn't created")];
        for monitor in &monitors[1..] {
            windows.push(
                glutin_winit::finalize_window(&event_loop, window_builder(monitor), &gl_config)
                    .expect("Failed to create window"),
            );
        }
        for (window, monitor) in windows.iter().zip(&monitors) {
            enter_fullscreen(window, monitor);
        }

        let not_current_gl_context =
            create_context(&gl_config, Some(windows[0].raw_window_handle()));

        let surfaces: Vec<_> = windows
            .iter()
            .map(|window| {
                create_surface(&gl_config, window.raw_window_handle(), window.inner_size())
            })
            .collect();

        let gl_context = Rc::new(
            not_current_gl_context
                .make_current(&surfaces[0])
                .expect("Failed to make GL context current"),
        );

        let outputs = windows
            .into_iter()
            .zip(surfaces)
            .zip(monitors)
            .map(|((window, surface), monitor)| {
                let size = window.inner_size();
                let output = GlOutput::new(surface, gl_context.clone(), &gl_config, size);
                (output, Rc::new(window), monitor)
            })
            .collect();

        run_screens(event_loop, outputs, auth, options);
    }

    #[cfg(target_os = "linux")]
    pub fn run_session_lock(
        session: session_lock::Session,
        auth: auth::Authenticator,
        options: Options,
    ) {
        let display = unsafe {
            glutin::display::Display::new(
                session_lock::raw_display_handle(session.display()),
                glutin::display::DisplayApiPreference::Egl,
            )
        }
        .unwrap_or_else(|err| fail(&format!("cannot use EGL: {err}")));
        let configs = unsafe { display.find_configs(template().build()) }
            .unwrap_or_else(|err| fail(&format!("no EGL config: {err}")));
        let gl_config = best_config(configs);
        let context = create_context(&gl_config, None);
        let surfaces = GlSurfaces {
            gl_config,
            context: Some(context),
            current: None,
        };
        session_lock::run(session, Box::new(surfaces), auth, options);
    }

    /// Makes GL outputs for Wayland surfaces, all sharing one context.
    #[cfg(target_os = "linux")]
    struct GlSurfaces {
        gl_config: Config,
        /// The context until it's made current with the first surface.
        context: Option<NotCurrentContext>,
        current: Option<Rc<PossiblyCurrentContext>>,
    }

    #[cfg(target_os = "linux")]
    impl session_lock::Surfaces for GlSurfaces {
        fn create(
            &mut self,
            surface: session_lock::SurfaceHandle,
            size: PhysicalSize<u32>,
        ) -> Box<dyn Output> {
            let surface = create_surface(&self.gl_config, surface.raw_window_handle(), size);
            let context = match (&self.current, self.context.take()) {
                (Some(current), _) => current.clone(),
                (None, Some(context)) => {
                    let current = Rc::new(
                        context
                            .make_current(&surface)
                            .expect("Failed to make GL context current"),
                    );
                    self.current = Some(current.clone());
                    current
                }
                (None, None) => unreachable!(),
            };
            Box::new(GlOutput::new(surface, context, &self.gl_config, size))
        }
    }

    struct GlOutput {
        // Field order matters: GL resources go before the context.
        canvas: Canvas<OpenGl>,
        surface: Surface<WindowSurface>,
        context: Rc<PossiblyCurrentContext>,
        size: PhysicalSize<u32>,
    }

    impl GlOutput {
        fn new(
            surface: Surface<WindowSurface>,
            context: Rc<PossiblyCurrentContext>,
            gl_config: &Config,
            size: PhysicalSize<u32>,
        ) -> Self {
            context
                .make_current(&surface)
                .expect("Failed to make GL context current");
            // With several windows sharing one context, waiting for vblank on
            // every swap would stall the frame loop once per monitor. Pacing
            // is done by FRAME_INTERVAL instead.
            let _ = surface.set_swap_interval(&context, SwapInterval::DontWait);

            let renderer = unsafe {
                OpenGl::new_from_function_cstr(|symbol| {
                    gl_config.display().get_proc_address(symbol)
                })
                .expect("Failed to create renderer")
            };
            let canvas = Canvas::new(renderer).expect("Failed to create canvas");

            Self {
                canvas,
                surface,
                context,
                size,
            }
        }

        fn make_current(&self) {
            self.context
                .make_current(&self.surface)
                .expect("Failed to make GL context current");
        }
    }

    impl Output for GlOutput {
        fn resize(&mut self, size: PhysicalSize<u32>) {
            self.make_current();
            self.size = PhysicalSize::new(size.width.max(1), size.height.max(1));
            let width = NonZeroU32::new(self.size.width).unwrap();
            let height = NonZeroU32::new(self.size.height).unwrap();
            self.surface.resize(&self.context, width, height);
        }

        fn render(&mut self, scene: &goop::Scene) {
            self.make_current();
            let size = self.size;
            // femtovg only applies the GL viewport in set_size, and the
            // viewport is context state shared by all windows, so it has to
            // be set again before every frame.
            self.canvas.set_size(size.width, size.height, 1.0);
            draw_scene(&mut self.canvas, scene, size);
            self.canvas.flush();
            self.surface.swap_buffers(&self.context).ok();
        }
    }

    fn draw_scene(canvas: &mut Canvas<OpenGl>, scene: &goop::Scene, size: PhysicalSize<u32>) {
        let alpha = (scene.background_alpha() * 255.0).round() as u8;
        canvas.clear_rect(0, 0, size.width, size.height, Color::rgba(0, 0, 0, alpha));

        for layer in &scene.layers {
            let paint = Paint::color(Color::rgb(layer.color[0], layer.color[1], layer.color[2]));
            for blob in &layer.blobs {
                let pts = blob.control_points();
                let sections = goop::closed_spline_sections(&pts);
                if sections.is_empty() {
                    continue;
                }
                let mut path = Path::new();
                path.move_to(sections[0].p0.0 as f32, sections[0].p0.1 as f32);
                for s in &sections {
                    path.bezier_to(
                        s.p1.0 as f32,
                        s.p1.1 as f32,
                        s.p2.0 as f32,
                        s.p2.1 as f32,
                        s.p3.0 as f32,
                        s.p3.1 as f32,
                    );
                }
                path.close();
                canvas.fill_path(&path, &paint);
            }
        }
    }
}

#[cfg(any(feature = "renderer-softbuffer", feature = "renderer-pixels"))]
mod cpu_renderer {
    use super::*;

    use tiny_skia::{Color, FillRule, Paint, PathBuilder, PixmapMut, Transform};

    /// One fullscreen window per monitor.
    pub fn create_windows(event_loop: &EventLoop<()>) -> Vec<(Rc<Window>, Option<MonitorHandle>)> {
        monitors(event_loop)
            .into_iter()
            .map(|monitor| {
                let window = window_builder(&monitor)
                    .build(event_loop)
                    .expect("Failed to create window");
                enter_fullscreen(&window, &monitor);
                (Rc::new(window), monitor)
            })
            .collect()
    }

    /// Byte order of the pixels `draw_scene` writes. tiny-skia always writes
    /// RGBA; drawing with red and blue swapped yields BGRA instead.
    #[derive(Clone, Copy)]
    #[allow(dead_code)] // Each renderer uses only one of them.
    pub enum ChannelOrder {
        Rgba,
        Bgra,
    }

    pub fn draw_scene(pixmap: &mut PixmapMut<'_>, scene: &goop::Scene, order: ChannelOrder) {
        // Neither softbuffer nor pixels can make the window transparent, so
        // the background stays black while unlocking.
        pixmap.fill(Color::BLACK);

        for layer in &scene.layers {
            let [r, g, b] = layer.color;
            let (r, b) = match order {
                ChannelOrder::Rgba => (r, b),
                ChannelOrder::Bgra => (b, r),
            };
            let mut paint = Paint::default();
            paint.set_color_rgba8(r, g, b, 255);
            // Like the original goop, which uses plain XFillPolygon; also
            // makes drawing about 1.5x faster.
            paint.anti_alias = false;

            for blob in &layer.blobs {
                let pts = blob.control_points();
                let sections = goop::closed_spline_sections(&pts);
                if sections.is_empty() {
                    continue;
                }
                let mut pb = PathBuilder::new();
                pb.move_to(sections[0].p0.0 as f32, sections[0].p0.1 as f32);
                for s in &sections {
                    pb.cubic_to(
                        s.p1.0 as f32,
                        s.p1.1 as f32,
                        s.p2.0 as f32,
                        s.p2.1 as f32,
                        s.p3.0 as f32,
                        s.p3.1 as f32,
                    );
                }
                pb.close();
                if let Some(path) = pb.finish() {
                    pixmap.fill_path(
                        &path,
                        &paint,
                        FillRule::Winding,
                        Transform::identity(),
                        None,
                    );
                }
            }
        }
    }
}

#[cfg(feature = "renderer-softbuffer")]
mod softbuffer_app {
    use super::*;

    use softbuffer::{Context, Surface};
    use std::num::NonZeroU32;
    use tiny_skia::PixmapMut;
    use winit::raw_window_handle::{HasDisplayHandle, HasWindowHandle};

    use crate::cpu_renderer::{ChannelOrder, create_windows, draw_scene};

    // We render straight into softbuffer's 0RGB `u32`s, which are BGRA bytes
    // only on little-endian machines.
    #[cfg(target_endian = "big")]
    compile_error!("renderer-softbuffer requires a little-endian target.");

    pub fn run(auth: auth::Authenticator, options: Options) {
        let event_loop = EventLoop::new().expect("Failed to create event loop");
        let outputs = create_windows(&event_loop)
            .into_iter()
            .map(|(window, monitor)| {
                let size = window.inner_size();
                (SoftbufferOutput::new(window.clone(), size), window, monitor)
            })
            .collect();
        run_screens(event_loop, outputs, auth, options);
    }

    #[cfg(target_os = "linux")]
    pub fn run_session_lock(
        session: session_lock::Session,
        auth: auth::Authenticator,
        options: Options,
    ) {
        session_lock::run(session, Box::new(SoftbufferSurfaces), auth, options);
    }

    #[cfg(target_os = "linux")]
    struct SoftbufferSurfaces;

    #[cfg(target_os = "linux")]
    impl session_lock::Surfaces for SoftbufferSurfaces {
        fn create(
            &mut self,
            surface: session_lock::SurfaceHandle,
            size: PhysicalSize<u32>,
        ) -> Box<dyn Output> {
            Box::new(SoftbufferOutput::new(surface, size))
        }
    }

    struct SoftbufferOutput<H: HasDisplayHandle + HasWindowHandle> {
        surface: Surface<H, H>,
        _context: Context<H>,
        size: PhysicalSize<u32>,
    }

    impl<H: HasDisplayHandle + HasWindowHandle + Clone> SoftbufferOutput<H> {
        fn new(handle: H, size: PhysicalSize<u32>) -> Self {
            let context =
                Context::new(handle.clone()).expect("Failed to create softbuffer context");
            let surface = Surface::new(&context, handle).expect("Failed to create surface");
            let mut output = Self {
                surface,
                _context: context,
                size,
            };
            output.resize(size);
            output
        }
    }

    impl<H: HasDisplayHandle + HasWindowHandle> Output for SoftbufferOutput<H> {
        fn resize(&mut self, size: PhysicalSize<u32>) {
            self.size = PhysicalSize::new(size.width.max(1), size.height.max(1));
            self.surface
                .resize(
                    NonZeroU32::new(self.size.width).unwrap(),
                    NonZeroU32::new(self.size.height).unwrap(),
                )
                .expect("Failed to resize surface");
        }

        fn render(&mut self, scene: &goop::Scene) {
            let mut buffer = self
                .surface
                .buffer_mut()
                .expect("Failed to get softbuffer buffer");
            // Draw directly into the buffer instead of into a separate pixmap
            // that then has to be converted pixel by pixel. All our pixels
            // are opaque, so the top byte is always 0xFF rather than the 0
            // softbuffer asks for; every backend either ignores it or reads
            // it as full alpha.
            let bytes: &mut [u8] = bytemuck::cast_slice_mut(&mut buffer);
            if let Some(mut pixmap) =
                PixmapMut::from_bytes(bytes, self.size.width, self.size.height)
            {
                draw_scene(&mut pixmap, scene, ChannelOrder::Bgra);
            }
            buffer.present().expect("Failed to present buffer");
        }
    }
}

#[cfg(feature = "renderer-pixels")]
mod pixels_app {
    use super::*;

    use pixels::{PixelsBuilder, SurfaceTexture};
    use raw_window_handle::{HasRawDisplayHandle, HasRawWindowHandle};
    use tiny_skia::PixmapMut;

    use crate::cpu_renderer::{ChannelOrder, create_windows, draw_scene};

    pub fn run(auth: auth::Authenticator, options: Options) {
        let event_loop = EventLoop::new().expect("Failed to create event loop");
        let outputs = create_windows(&event_loop)
            .into_iter()
            .map(|(window, monitor)| {
                let output = PixelsOutput::new(&*window, window.inner_size());
                (output, window, monitor)
            })
            .collect();
        run_screens(event_loop, outputs, auth, options);
    }

    #[cfg(target_os = "linux")]
    pub fn run_session_lock(
        session: session_lock::Session,
        auth: auth::Authenticator,
        options: Options,
    ) {
        session_lock::run(session, Box::new(PixelsSurfaces), auth, options);
    }

    #[cfg(target_os = "linux")]
    struct PixelsSurfaces;

    #[cfg(target_os = "linux")]
    impl session_lock::Surfaces for PixelsSurfaces {
        fn create(
            &mut self,
            surface: session_lock::SurfaceHandle,
            size: PhysicalSize<u32>,
        ) -> Box<dyn Output> {
            Box::new(PixelsOutput::new(&surface, size))
        }
    }

    struct PixelsOutput {
        pixels: pixels::Pixels,
        size: PhysicalSize<u32>,
    }

    impl PixelsOutput {
        /// The window or surface must outlive the output.
        fn new<W: HasRawWindowHandle + HasRawDisplayHandle>(
            window: &W,
            size: PhysicalSize<u32>,
        ) -> Self {
            let surface_texture =
                SurfaceTexture::new(size.width.max(1), size.height.max(1), window);
            // As with GL: waiting for vblank in every window's render would
            // stall the frame loop once per monitor. FRAME_INTERVAL paces.
            let pixels = PixelsBuilder::new(size.width.max(1), size.height.max(1), surface_texture)
                .enable_vsync(false)
                .build()
                .expect("Failed to create Pixels surface");
            Self { pixels, size }
        }
    }

    impl Output for PixelsOutput {
        fn resize(&mut self, size: PhysicalSize<u32>) {
            self.size = size;
            let _ = self
                .pixels
                .resize_surface(size.width.max(1), size.height.max(1));
            let _ = self
                .pixels
                .resize_buffer(size.width.max(1), size.height.max(1));
        }

        fn render(&mut self, scene: &goop::Scene) {
            let (width, height) = (self.size.width.max(1), self.size.height.max(1));
            if let Some(mut pixmap) = PixmapMut::from_bytes(self.pixels.frame_mut(), width, height)
            {
                draw_scene(&mut pixmap, scene, ChannelOrder::Rgba);
            }
            self.pixels.render().expect("Pixels render failed");
        }
    }
}
