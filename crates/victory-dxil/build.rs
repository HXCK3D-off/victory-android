//! Android ships libc++ and has no `libstdc++`, but `dxil-spirv-sys` asks every
//! non-Windows link for `-lstdc++`. This script gives the link a private
//! directory holding an empty `libstdc++.a` for that request and the NDK's own
//! libc++ archives, which is what the C++ converter actually needs.
//!
//! The directory has to be private. The NDK keeps its static libc, `libc.a`, in
//! the same place as the libc++ archives, and the linker searches a `-L`
//! directory before its own sysroot, so pointing `-L` at the NDK's library
//! directory makes `-lc` resolve to static bionic: the app then links its own
//! copy of libc, whose aux vector is null inside a shared library, and it
//! segfaults in `getauxval` during load before any Rust code runs.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("android") {
        return;
    }

    let sysroot = cxx_library_dir().unwrap_or_else(|| {
        panic!(
            "the NDK's libc++ was not found. Set ANDROID_NDK_HOME to the NDK root \
             (scripts/android/env.ps1 does it for the Android build script)."
        )
    });

    let out = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR")).join("cxx");
    fs::create_dir_all(&out).expect("creating the link directory");
    // `_Unwind_*` comes from bionic, so libc++ is the whole of what is missing.
    for library in ["libc++_static.a", "libc++abi.a"] {
        fs::copy(sysroot.join(library), out.join(library))
            .unwrap_or_else(|error| panic!("copying {library}: {error}"));
    }
    fs::write(out.join("libstdc++.a"), b"!<arch>\n").expect("writing an empty libstdc++ archive");

    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=c++_static");
    println!("cargo:rustc-link-lib=static=c++abi");
}

/// The NDK directory holding `libc++_static.a` for this target.
fn cxx_library_dir() -> Option<PathBuf> {
    let host = env::var("HOST").ok()?;
    let host_tag = if host.contains("windows") {
        "windows-x86_64"
    } else if host.contains("darwin") {
        "darwin-x86_64"
    } else {
        "linux-x86_64"
    };
    let target = env::var("TARGET").ok()?;

    let mut roots: Vec<PathBuf> = ["ANDROID_NDK_HOME", "ANDROID_NDK_ROOT", "ANDROID_NDK"]
        .iter()
        .filter_map(|key| env::var(key).ok())
        .map(PathBuf::from)
        .collect();

    // A compiler that lives in the NDK says where the NDK is, which covers the
    // builds that set CC/CXX and nothing else.
    for key in ["CC", "CXX"] {
        let Ok(compiler) = env::var(format!("{key}_{}", target.replace('-', "_"))) else {
            continue;
        };
        for ancestor in Path::new(&compiler).ancestors() {
            if ancestor.join("toolchains/llvm").is_dir() {
                roots.push(ancestor.to_path_buf());
                break;
            }
        }
    }

    roots
        .into_iter()
        .map(|root| {
            root.join("toolchains/llvm/prebuilt")
                .join(host_tag)
                .join("sysroot/usr/lib")
                .join(&target)
        })
        .find(|dir| dir.join("libc++_static.a").is_file() && dir.join("libc++abi.a").is_file())
}
