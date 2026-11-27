# The APK shell

This is the packaging half of the Android host; the code is in
`apps/victory-android`, and `docs/android.md` explains what the app does and
what is still missing before a title can run.

There is no Java or Kotlin here on purpose. The activity is the framework's
`android.app.NativeActivity`, which loads `lib/arm64-v8a/libvictory_android.so`
and calls its `android_main`, so the manifest and this Gradle project exist only
to produce and sign that package.

## Build

Gradle does not drive Cargo: the native library is built first and staged next
to this manifest.

```powershell
# from the repository root
.\scripts\build-android.ps1 -Assemble
```

```bash
scripts/build-android.sh --assemble
```

Opening this folder in Android Studio works too, once
`app/src/main/jniLibs/arm64-v8a/libvictory_android.so` exists; the Gradle build
stops with a message naming the script if it does not.

The shader translator is C++ built through CMake, so the host needs two tools on
`PATH` in addition to the NDK: `cmake` and `ninja` (`python -m pip install
cmake ninja`), plus `libclang` for its bindings (`python -m pip install
libclang`). `scripts/android/env.ps1` finds libclang and points CMake at the
NDK; it stops with the install line if either tool is missing.

## Signing

`assembleRelease` produces an unsigned APK unless a keystore is supplied:

```powershell
$env:VICTORY_KEYSTORE          = 'C:\Users\me\victory.jks'
$env:VICTORY_KEYSTORE_PASSWORD = '...'
$env:VICTORY_KEY_ALIAS         = 'victory'
$env:VICTORY_KEY_PASSWORD      = '...'
.\scripts\build-android.ps1 -Assemble -Release
```

Never commit a keystore or its passwords; `android/.gitignore` already excludes
`*.jks`, `*.keystore`, `local.properties` and build output.

## Logs

```bash
adb logcat -s victory:V
```
