// The APK shell around `libvictory_android.so`. It has no Java or Kotlin
// sources: the activity is the platform's `NativeActivity`, and everything it
// does is in `apps/victory-android`.
plugins {
    id("com.android.application") version "8.7.3" apply false
}
