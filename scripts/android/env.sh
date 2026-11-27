#!/usr/bin/env bash
# Puts the Android NDK's tools on PATH for this shell, and points Cargo at them.
#
# Source it, do not run it:
#
#     . scripts/android/env.sh
#
# The C compiler is the NDK's `clang` plus `--target`, the same driver on every
# host; `scripts/build-android.sh` sources this itself.

VICTORY_ANDROID_API="${VICTORY_ANDROID_API:-33}"

_victory_sdk() {
    if [ -n "${ANDROID_SDK_ROOT:-}" ]; then printf '%s\n' "$ANDROID_SDK_ROOT"; return 0; fi
    if [ -n "${ANDROID_HOME:-}" ]; then printf '%s\n' "$ANDROID_HOME"; return 0; fi
    for candidate in "$HOME/Android/Sdk" "$HOME/Library/Android/sdk" "$HOME/android-sdk"; do
        if [ -d "$candidate" ]; then printf '%s\n' "$candidate"; return 0; fi
    done
    return 1
}

_victory_ndk() {
    if [ -n "${ANDROID_NDK_HOME:-}" ]; then printf '%s\n' "$ANDROID_NDK_HOME"; return 0; fi
    if [ -n "${ANDROID_NDK_ROOT:-}" ]; then printf '%s\n' "$ANDROID_NDK_ROOT"; return 0; fi
    local sdk ndk
    sdk="$(_victory_sdk)" || return 1
    [ -d "$sdk/ndk" ] || return 1
    ndk="$(ls -d "$sdk"/ndk/* 2>/dev/null | sort -V | tail -1)"
    [ -n "$ndk" ] && printf '%s\n' "$ndk"
}

VICTORY_SDK="$(_victory_sdk || true)"
VICTORY_NDK="$(_victory_ndk || true)"

if [ -z "$VICTORY_NDK" ] || [ ! -d "$VICTORY_NDK" ]; then
    echo "Android NDK not found: install it from Android Studio's SDK Manager," >&2
    echo "or set ANDROID_NDK_HOME to an NDK r27 or newer." >&2
    return 1 2>/dev/null || exit 1
fi

case "$(uname -s)" in
    Darwin) VICTORY_NDK_HOST=darwin-x86_64 ;;
    *)      VICTORY_NDK_HOST=linux-x86_64 ;;
esac

VICTORY_NDK_BIN="$VICTORY_NDK/toolchains/llvm/prebuilt/$VICTORY_NDK_HOST/bin"
if [ ! -x "$VICTORY_NDK_BIN/clang" ]; then
    echo "the NDK at $VICTORY_NDK has no llvm/prebuilt/$VICTORY_NDK_HOST/bin/clang" >&2
    return 1 2>/dev/null || exit 1
fi

export ANDROID_NDK_HOME="$VICTORY_NDK"
[ -n "$VICTORY_SDK" ] && export ANDROID_HOME="$VICTORY_SDK"
export VICTORY_NDK_BIN
export PATH="$VICTORY_NDK_BIN:$PATH"

export CC_aarch64_linux_android=clang
export CFLAGS_aarch64_linux_android="--target=aarch64-linux-android$VICTORY_ANDROID_API"
export AR_aarch64_linux_android=llvm-ar
export RANLIB_aarch64_linux_android=llvm-ranlib

# The shader translator is C++: `victory-dxil` builds `dxil-spirv` through
# CMake for the target, which needs a C++ driver aimed at Android, Ninja (the
# generator CMake uses to cross-compile here), and libclang for bindgen.
export CXX_aarch64_linux_android=clang++
export CXXFLAGS_aarch64_linux_android="--target=aarch64-linux-android$VICTORY_ANDROID_API"
export CMAKE_GENERATOR=Ninja
export ANDROID_NDK="$VICTORY_NDK"

for tool in cmake ninja; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "$tool is not on PATH, and the shader translator's CMake build needs it: python -m pip install cmake ninja" >&2
        return 1 2>/dev/null || exit 1
    fi
done
export CMAKE_MAKE_PROGRAM="$(command -v ninja)"

if [ -z "${LIBCLANG_PATH:-}" ]; then
    for candidate in \
        "$(python3 -c 'import clang, os; print(os.path.dirname(clang.__file__))' 2>/dev/null)/native" \
        /usr/lib/llvm-*/lib /opt/homebrew/opt/llvm/lib /usr/local/opt/llvm/lib; do
        if [ -e "$candidate/libclang.so" ] || [ -e "$candidate/libclang.dylib" ]; then
            export LIBCLANG_PATH="$candidate"
            break
        fi
    done
fi
if [ -z "${LIBCLANG_PATH:-}" ]; then
    echo "libclang not found, and the shader translator's bindings need it: python -m pip install libclang (or set LIBCLANG_PATH)" >&2
    return 1 2>/dev/null || exit 1
fi

if ! java -version >/dev/null 2>&1 || ! java -version 2>&1 | head -1 | grep -qE '"(1[7-9]|[2-9][0-9])'; then
    for candidate in /usr/lib/jvm/*21* /usr/lib/jvm/*17* /opt/homebrew/opt/openjdk*; do
        if [ -x "$candidate/bin/java" ]; then
            export JAVA_HOME="$candidate"
            export PATH="$JAVA_HOME/bin:$PATH"
            break
        fi
    done
fi

echo "NDK   : $ANDROID_NDK_HOME"
[ -n "$VICTORY_SDK" ] && echo "SDK   : $VICTORY_SDK"
[ -n "${JAVA_HOME:-}" ] && echo "JDK   : $JAVA_HOME"
echo "clang : $("$VICTORY_NDK_BIN/clang" --version | head -1)"
echo "target: aarch64-linux-android$VICTORY_ANDROID_API"
