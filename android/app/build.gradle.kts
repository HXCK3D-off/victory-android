plugins {
    id("com.android.application")
}

android {
    namespace = "com.victory.android"
    compileSdk = 35
    buildToolsVersion = "35.0.0"

    defaultConfig {
        applicationId = "com.victory.android"
        // Vulkan 1.1 is the Android 10 floor; 33 is the first release whose
        // loader understands the 16 KB page size and the newer swap chain
        // paths this host exercises. Raise it only if a later layer needs more.
        minSdk = 33
        targetSdk = 35
        versionCode = 1
        versionName = "0.1.0"
    }

    buildTypes {
        debug {
            isMinifyEnabled = false
        }
        release {
            isMinifyEnabled = false
            isShrinkResources = false
        }
    }

    // The library is mapped straight out of the APK. `useLegacyPackaging =
    // false` keeps it uncompressed and page-aligned; the 16 KB maximum page
    // size comes from the linker flags in `.cargo/config.toml`.
    packaging {
        jniLibs {
            useLegacyPackaging = false
            // `scripts/build-android.*` strips the library with the NDK's own
            // `llvm-strip`; Gradle has no NDK here and would warn that it
            // cannot strip an already stripped file.
            keepDebugSymbols += "**/libvictory_android.so"
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    lint {
        abortOnError = false
        checkReleaseBuilds = false
    }

    /**
     * A release APK is only signed when a keystore is supplied, so a plain
     * `assembleRelease` works out of the box and produces an unsigned APK.
     *
     *   $env:VICTORY_KEYSTORE          = 'C:\Users\me\victory.jks'
     *   $env:VICTORY_KEYSTORE_PASSWORD = '...'
     *   $env:VICTORY_KEY_ALIAS         = 'victory'
     *   $env:VICTORY_KEY_PASSWORD      = '...'
     */
    val keystore = System.getenv("VICTORY_KEYSTORE")
    if (keystore != null) {
        signingConfigs {
            create("release") {
                storeFile = file(keystore)
                storePassword = System.getenv("VICTORY_KEYSTORE_PASSWORD")
                keyAlias = System.getenv("VICTORY_KEY_ALIAS")
                keyPassword = System.getenv("VICTORY_KEY_PASSWORD")
            }
        }
        buildTypes {
            getByName("release") {
                signingConfig = signingConfigs.getByName("release")
            }
        }
    }
}

/**
 * Fails before Gradle packages a shell with no native code in it, and says what
 * to run instead: the Rust build needs the NDK, which Gradle does not have to
 * know about.
 */
val nativeLibrary =
    layout.projectDirectory.file("src/main/jniLibs/arm64-v8a/libvictory_android.so")

val checkNativeLibrary by tasks.registering {
    description = "Checks that libvictory_android.so has been built and staged."
    doFirst {
        if (!nativeLibrary.asFile.isFile) {
            throw GradleException(
                "missing ${nativeLibrary.asFile}\n" +
                    "Build the native library first:\n" +
                    "  Windows : powershell -File scripts/build-android.ps1 -Assemble\n" +
                    "  Unix    : scripts/build-android.sh --assemble"
            )
        }
    }
}

tasks.named("preBuild") {
    dependsOn(checkNativeLibrary)
}
