#!/usr/bin/env bash
# Builds the Android host (libvictory_android.so) and, optionally, the APK.
#
#   scripts/build-android.sh                 library only
#   scripts/build-android.sh --assemble      library, then gradlew assembleDebug
#   scripts/build-android.sh --assemble --release --install
#   scripts/build-android.sh --diagnostics   debug/trace logging in logcat
set -euo pipefail

assemble=0
release=0
install=0
diagnostics=0
api=33

while [ $# -gt 0 ]; do
    case "$1" in
        --assemble) assemble=1 ;;
        --release) release=1 ;;
        --install) install=1 ;;
        --diagnostics) diagnostics=1 ;;
        --api) shift; api="$1" ;;
        -h|--help) sed -n '2,9p' "$0"; exit 0 ;;
        *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
    shift
done

VICTORY_ANDROID_API="$api" . "$(dirname "$0")/android/env.sh"

root="$(cd "$(dirname "$0")/.." && pwd)"
target=aarch64-linux-android
library="$root/target/$target/release/libvictory_android.so"
jniLibs="$root/android/app/src/main/jniLibs/arm64-v8a"

cargo_build=(build --package victory-android --target "$target" --release)
if [ "$diagnostics" -eq 1 ]; then
    cargo_build+=(--features diagnostics)
fi
echo "cargo ${cargo_build[*]}"
(cd "$root" && cargo "${cargo_build[@]}")

[ -f "$library" ] || { echo "cargo reported success but $library is missing" >&2; exit 1; }

mkdir -p "$jniLibs"
staged="$jniLibs/libvictory_android.so"
cp "$library" "$staged"

# The packaged copy is stripped; the one in `target` keeps its symbols for a
# debugger. Without this the APK carries 28 MB of line tables.
"$VICTORY_NDK_BIN/llvm-strip" --strip-unneeded "$staged"

# 16 KB pages are not optional on Android 15, and the Play Store checks the
# alignment of every shared library in an upload.
bad="$("$VICTORY_NDK_BIN/llvm-readelf" -lW "$staged" | awk '/^ *LOAD/ && $0 !~ /0x4000/')"
if [ -n "$bad" ]; then
    echo "libvictory_android.so is not 16 KB page aligned:" >&2
    echo "$bad" >&2
    exit 1
fi
if ! "$VICTORY_NDK_BIN/llvm-nm" -D --defined-only "$staged" | grep -qw android_main; then
    echo "android_main is not exported from the stripped library" >&2
    exit 1
fi
echo "alignment: 16 KB (OK), android_main exported"
echo "staged: $staged ($(du -h "$staged" | cut -f1))"

if [ "$assemble" -eq 1 ]; then
    if [ -n "${ANDROID_HOME:-}" ]; then
        printf 'sdk.dir=%s\n' "$ANDROID_HOME" > "$root/android/local.properties"
    fi
    task=assembleDebug
    [ "$release" -eq 1 ] && task=assembleRelease
    if [ "$release" -eq 1 ]; then
        apk="$root/android/app/build/outputs/apk/release/app-release.apk"
    else
        apk="$root/android/app/build/outputs/apk/debug/app-debug.apk"
    fi
    # A packaging step that rewrites the APK in place leaves the old tail
    # behind when the library shrank; deleting first avoids an APK that is
    # mostly stale bytes.
    rm -f "$apk"
    (cd "$root/android" && ./gradlew --no-daemon "$task")
    echo "apk   : $apk"

    if [ "$install" -eq 1 ]; then
        adb install -r "$apk"
    fi
fi
