//! One tracing setup for every binary.
//!
//! Filter with `VICTORY_LOG` (falls back to `RUST_LOG`), e.g.
//! `VICTORY_LOG=victory_loader=trace,info`. Debug and trace output exist
//! only in builds with the `diagnostics` feature; a plain release build
//! logs info and above whatever the filter asks for.
//!
//! On Android the same events go to logcat. An app's stderr is not connected
//! to anything a developer can see, so the events are written through `log` at
//! the level they were recorded with and `android_logger` forwards them to
//! `__android_log_print`; `adb logcat -s victory` then shows exactly what
//! `victory` prints on the desktop.

use tracing_subscriber::EnvFilter;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// The most verbose level this build can log.
const CAP: LevelFilter = if cfg!(feature = "diagnostics") {
    LevelFilter::TRACE
} else {
    LevelFilter::INFO
};

/// The logcat tag every event from this process is filed under.
#[cfg(target_os = "android")]
const ANDROID_TAG: &str = "victory";

pub fn init(default_level: &str) {
    let filter = std::env::var("VICTORY_LOG")
        .or_else(|_| std::env::var("RUST_LOG"))
        .ok()
        .and_then(|v| EnvFilter::try_new(v).ok())
        .unwrap_or_else(|| EnvFilter::new(default_level));
    let registry = tracing_subscriber::registry().with(CAP).with(filter);

    #[cfg(not(target_os = "android"))]
    {
        let _ = registry
            .with(
                tracing_subscriber::fmt::layer()
                    .with_target(true)
                    .with_writer(std::io::stderr),
            )
            .try_init();
    }

    #[cfg(target_os = "android")]
    {
        // The floor for anything using `log` directly: android-activity
        // traces every turn of the event loop at TRACE, which floods logcat
        // and buries our own lines. The tracing filter above still decides
        // what of ours gets through.
        let floor = if cfg!(feature = "diagnostics") {
            log::LevelFilter::Debug
        } else {
            log::LevelFilter::Info
        };
        android_logger::init_once(
            android_logger::Config::default()
                .with_max_level(floor)
                .with_tag(ANDROID_TAG),
        );
        let _ = registry.with(logcat::Logcat).try_init();
    }
}

/// A `tracing` layer that hands each event to `log`, which on Android is
/// `android_logger`. Levels are preserved, so `adb logcat` filters the same way
/// the desktop's `VICTORY_LOG` does.
#[cfg(target_os = "android")]
mod logcat {
    use std::fmt::Write as _;

    use tracing::field::{Field, Visit};
    use tracing::{Event, Level, Subscriber};
    use tracing_subscriber::layer::{Context, Layer};

    /// The layer itself. Named `Logcat` rather than `Layer` so the trait it
    /// implements stays in scope by its own name.
    pub struct Logcat;

    impl<S: Subscriber> Layer<S> for Logcat {
        fn on_event(&self, event: &Event<'_>, _context: Context<'_, S>) {
            let metadata = event.metadata();
            let mut line = Line::default();
            event.record(&mut line);
            let message = line.into_string();
            log::log!(
                target: super::ANDROID_TAG,
                level_of(*metadata.level()),
                "{}: {message}",
                metadata.target(),
            );
        }
    }

    fn level_of(level: Level) -> log::Level {
        match level {
            Level::TRACE => log::Level::Trace,
            Level::DEBUG => log::Level::Debug,
            Level::INFO => log::Level::Info,
            Level::WARN => log::Level::Warn,
            Level::ERROR => log::Level::Error,
        }
    }

    /// The event's fields as one line: the message first, then `key=value`
    /// for the structured fields, which is what the desktop's formatter does.
    #[derive(Default)]
    struct Line {
        message: Option<String>,
        fields: String,
    }

    impl Line {
        fn into_string(self) -> String {
            match self.message {
                Some(message) => format!("{message}{}", self.fields),
                None => self.fields.trim_start().to_owned(),
            }
        }
    }

    impl Visit for Line {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            if field.name() == "message" {
                self.message = Some(format!("{value:?}"));
            } else {
                let _ = write!(self.fields, " {}={value:?}", field.name());
            }
        }

        fn record_str(&mut self, field: &Field, value: &str) {
            if field.name() == "message" {
                self.message = Some(value.to_owned());
            } else {
                let _ = write!(self.fields, " {}={value}", field.name());
            }
        }

        fn record_i64(&mut self, field: &Field, value: i64) {
            let _ = write!(self.fields, " {}={value}", field.name());
        }

        fn record_u64(&mut self, field: &Field, value: u64) {
            let _ = write!(self.fields, " {}={value}", field.name());
        }

        fn record_f64(&mut self, field: &Field, value: f64) {
            let _ = write!(self.fields, " {}={value}", field.name());
        }

        fn record_bool(&mut self, field: &Field, value: bool) {
            let _ = write!(self.fields, " {}={value}", field.name());
        }

        fn record_error(
            &mut self,
            field: &Field,
            value: &(dyn std::error::Error + 'static),
        ) {
            let _ = write!(self.fields, " {}={value}", field.name());
        }
    }
}
