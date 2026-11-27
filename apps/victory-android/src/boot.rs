//! What the host needs before it can draw anything: a place for its files, a
//! logcat logger, and an honest account of which console layers this build
//! actually has.
//!
//! On the desktop these are `victory_core::Config` and the launcher's
//! environment. Android gives an app one private directory and no environment
//! at all, so the same variables are installed here before anything reads
//! them; every tool and crate below `victory-core` then behaves exactly as it
//! does on Windows.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use android_activity::AndroidApp;
use victory_core::Config;

/// Where this process keeps its files, fixed at startup.
struct Paths {
    data: PathBuf,
    cache: PathBuf,
}

static PATHS: OnceLock<Paths> = OnceLock::new();

/// Installs the environment, starts the logger and reports the configuration.
pub fn start(app: &AndroidApp) {
    let root = app
        .internal_data_path()
        .unwrap_or_else(|| PathBuf::from("/data/local/tmp/victory"));
    let paths = Paths {
        data: root.join("victory"),
        cache: root.join("victory").join("cache"),
    };
    install_env(&paths, &root);
    let _ = PATHS.set(paths);
    victory_core::logging::init("info");

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        abi = std::env::consts::ARCH,
        sdk = AndroidApp::sdk_version(),
        "victory Android host"
    );
    report_config();
    report_layers();
}

/// Where a title's shader containers are dropped: `<app files>/victory/shaders`.
pub fn shader_dir() -> PathBuf {
    PATHS
        .get()
        .map_or_else(|| PathBuf::from("/data/local/tmp"), |paths| paths.data.join("shaders"))
}

/// The cache root for this process: `<app files>/victory/cache`.
pub fn cache_dir() -> &'static Path {
    PATHS
        .get()
        .map_or(Path::new("/data/local/tmp"), |paths| paths.cache.as_path())
}

/// The same variables the launcher sets on Windows. The values are absolute
/// and app-private, so the asset policy's "never inside the repository" check
/// passes for the same reason it does there: there is no repository.
fn install_env(paths: &Paths, home: &Path) {
    // SAFETY: this runs once, at the top of `android_main`, before the host
    // has started a thread of its own and before any crate reads the
    // environment. The Android framework has its own threads in this process,
    // but none of them reads the Rust crate environment.
    unsafe {
        std::env::set_var("VICTORY_DATA_DIR", &paths.data);
        std::env::set_var("VICTORY_CACHE_DIR", &paths.cache);
        std::env::set_var("HOME", home);
        if std::env::var_os("VICTORY_LOG").is_none() {
            std::env::set_var("VICTORY_LOG", "info");
        }
    }
}

fn report_config() {
    match Config::load() {
        Ok(config) => tracing::info!(
            data = %config.data_dir.display(),
            cache = %config.cache_dir.display(),
            "files"
        ),
        Err(error) => tracing::warn!(error = format!("{error:#}"), "configuration"),
    }
}

/// The state of the port, in one line per layer. It is logged at every start
/// on purpose: a build that claims more than it does is worse than one that
/// says which piece is missing, and a logcat capture is how a bug report from
/// a phone arrives.
fn report_layers() {
    tracing::info!("layer: victory-pe, victory-formats, victory-game, victory-pipeline, victory-shader: portable, in this APK");
    tracing::info!(
        "layer: victory-loader: prepares and resolves images, but executing the \
         title's x86-64 code needs a translation to ARM64 this fork does not have"
    );
    tracing::info!(
        "layer: victory-gpu: D3D12 on host D3D12 on Windows; on Android a Vulkan \
         backend, which has pipeline state from a description, descriptor \
         layouts, a texture path and a four-target pass, and no command lists \
         yet"
    );
    tracing::info!("layer: victory-hle: Xbox system modules, no Android host for them yet");
    tracing::info!("see docs/android.md for what each of those takes");
}
