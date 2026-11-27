//! The Android host.
//!
//! This is the phone-side counterpart of `apps/victory`. The desktop front end
//! is a command line that opens a Win32 window and drives a title through the
//! Windows loader; Android has no command line, no Win32 window, and no way to
//! run the title's x86-64 image at all, so the entry point is an
//! `ANativeActivity` (`android_main` below), the window arrives as an
//! `ANativeWindow`, and the files live in the app's private directory.
//!
//! What is here is deliberately the *host*: the portable crates (`victory-pe`,
//! `victory-shader`, `victory-formats`, `victory-game`, `victory-pipeline`)
//! know nothing about it and keep working unchanged. What is not here, and
//! cannot be until it exists, is the part that makes a title run: an
//! x86-64-to-ARM64 translation of the title's own code and a D3D12-to-Vulkan
//! backend. `docs/android.md` states both plainly, with what each one touches.
//!
//! The milestone this file does reach is the one every later step needs: a
//! Vulkan device on the phone's adapter and a swap chain on the activity's
//! surface, presenting a frame every frame, with the pipeline cache and the
//! shader translator's host limits set from what the driver actually reports.

#![cfg(target_os = "android")]

mod boot;
mod render;
mod shader;
mod title;

use std::time::Duration;

use android_activity::input::InputEvent;
use android_activity::InputStatus;
use android_activity::ndk::native_window::NativeWindow;
use android_activity::{AndroidApp, MainEvent, PollEvent};

use render::{Frame, Renderer};

/// How long the loop waits for events before drawing again. `FIFO` present is
/// what actually paces the app; this only keeps the loop from spinning while
/// the activity is idle.
const POLL: Duration = Duration::from_millis(16);

struct Host {
    window: Option<NativeWindow>,
    renderer: Option<Renderer>,
    resized: bool,
    running: bool,
}

impl Host {
    fn new() -> Self {
        Host {
            window: None,
            renderer: None,
            resized: false,
            running: true,
        }
    }

    fn on_event(&mut self, app: &AndroidApp, event: PollEvent<'_>) {
        let PollEvent::Main(event) = event else {
            return;
        };
        match event {
            MainEvent::InitWindow { .. } => self.start(app),
            MainEvent::TerminateWindow { .. } => {
                tracing::info!("native window gone");
                self.renderer = None;
                self.window = None;
            }
            MainEvent::WindowResized { .. } => {
                tracing::debug!("window resized");
                self.resized = true;
            }
            MainEvent::ContentRectChanged { .. } => {
                tracing::debug!("content rect changed");
                self.resized = true;
            }
            MainEvent::RedrawNeeded { .. } => self.draw(),
            MainEvent::LowMemory => {
                // The pipeline cache is the biggest thing we own outright and
                // the cheapest to rebuild, but dropping it mid-frame buys
                // almost nothing; say so rather than pretend to help.
                tracing::warn!("the system is low on memory");
            }
            MainEvent::Destroy => {
                tracing::info!("activity destroyed");
                self.running = false;
            }
            MainEvent::InputAvailable => self.drain_input(app),
            _ => {}
        }
    }

    /// Brings up the device and the swap chain on the activity's surface.
    ///
    /// A failure here is fatal for the host: there is no second adapter to
    /// fall back to and no window to draw without, so the loop stops with the
    /// error in logcat instead of retrying forever behind a black screen.
    fn start(&mut self, app: &AndroidApp) {
        let Some(window) = app.native_window() else {
            tracing::error!("InitWindow without a native window");
            return;
        };
        tracing::info!(
            width = window.width(),
            height = window.height(),
            "native window"
        );
        match Renderer::new(&window, boot::cache_dir(), &boot::shader_dir()) {
            Ok(renderer) => {
                let extent = renderer.extent();
                tracing::info!(
                    width = extent.width,
                    height = extent.height,
                    "Vulkan host ready"
                );
                self.renderer = Some(renderer);
                self.window = Some(window);
            }
            Err(error) => {
                tracing::error!(error = format!("{error:#}"), "Vulkan host failed");
                self.running = false;
            }
        }
    }

    fn tick(&mut self) {
        if self.resized
            && let Some(renderer) = self.renderer.as_mut()
            && let Some(window) = &self.window
        {
            let size = size_of(window);
            self.resized = false;
            if let Err(error) = renderer.resize(size) {
                tracing::error!(error = format!("{error:#}"), "resize failed");
                self.renderer = None;
                self.running = false;
                return;
            }
        }
        self.draw();
    }

    fn draw(&mut self) {
        let Some(window) = &self.window else {
            return;
        };
        let size = size_of(window);
        let Some(renderer) = self.renderer.as_mut() else {
            return;
        };
        match renderer.draw(size) {
            Ok(Frame::Presented) | Ok(Frame::Rebuilt) => {}
            Err(error) => {
                tracing::error!(error = format!("{error:#}"), "frame failed");
                self.renderer = None;
                self.running = false;
            }
        }
    }

    /// Drains the input queue. Android stops sending `InputAvailable` until the
    /// queue is read, so this runs even while nothing acts on input yet; the
    /// title's own input goes through `victory-hle`'s GameInput module, which
    /// is a later layer than the surface.
    fn drain_input(&mut self, app: &AndroidApp) {
        let Ok(mut events) = app.input_events_iter() else {
            return;
        };
        while events.next(|event| match event {
            InputEvent::MotionEvent(motion) => {
                tracing::trace!(action = ?motion.action(), "touch");
                InputStatus::Handled
            }
            InputEvent::KeyEvent(key) => {
                tracing::trace!(key = ?key.key_code(), "key");
                InputStatus::Handled
            }
            _ => InputStatus::Unhandled,
        }) {}
    }
}

fn size_of(window: &NativeWindow) -> (u32, u32) {
    (
        window.width().max(1) as u32,
        window.height().max(1) as u32,
    )
}

/// The native activity entry point. `android-activity` exports
/// `ANativeActivity_onCreate` from the same crate and calls this from the
/// activity's own thread.
///
/// `#[unsafe(no_mangle)]` because the Java side looks the symbol up by name;
/// the ABI is the one `android-activity` documents, a single `AndroidApp`
/// handle.
#[unsafe(no_mangle)]
pub fn android_main(app: AndroidApp) {
    boot::start(&app);
    let mut host = Host::new();
    while host.running {
        app.poll_events(Some(POLL), |event| host.on_event(&app, event));
        host.tick();
    }
    tracing::info!("android_main returning");
}
