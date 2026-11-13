#!/usr/bin/env bash
# Builds the Android video client library and harness app (android/, Gradle): first the
# native library and Kotlin bindings (scripts/build-streamcore-video-android.sh, all three
# ABIs; skip with STREAMCORE_VIDEO_ANDROID_SKIP_CORE=1 when target/streamcore-video-android
# is current), then `./gradlew assembleDebug`, which also compiles the generated Kotlin.
# Output: android/harness/build/outputs/apk/debug/harness-debug.apk.
#
# Needs JDK 17 (JAVA_HOME, else Homebrew's openjdk@17) and the Android SDK (ANDROID_HOME,
# else ~/Library/Android/sdk) with platform 36; Gradle downloads the Android Gradle plugin
# and the libraries on first use.
set -euo pipefail
CORE_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$CORE_ROOT"

if [[ -z "${JAVA_HOME:-}" ]] && command -v brew >/dev/null; then
    CANDIDATE="$(brew --prefix openjdk@17 2>/dev/null || true)/libexec/openjdk.jdk/Contents/Home"
    [[ -x "$CANDIDATE/bin/java" ]] && export JAVA_HOME="$CANDIDATE"
fi
[[ -n "${JAVA_HOME:-}" && -x "$JAVA_HOME/bin/java" ]] || {
    echo "error: no JDK 17; set JAVA_HOME (for example to Homebrew's openjdk@17)" >&2
    exit 1
}
export ANDROID_HOME="${ANDROID_HOME:-${ANDROID_SDK_ROOT:-$HOME/Library/Android/sdk}}"
[[ -d "$ANDROID_HOME/platforms" ]] || {
    echo "error: no Android SDK at $ANDROID_HOME; set ANDROID_HOME" >&2
    exit 1
}

if [[ "${STREAMCORE_VIDEO_ANDROID_SKIP_CORE:-0}" != 1 ]]; then
    bash scripts/build-streamcore-video-android.sh arm64-v8a armeabi-v7a x86_64
fi
(cd android && ./gradlew --no-daemon assembleDebug)
APK=android/harness/build/outputs/apk/debug/harness-debug.apk
[[ -f "$APK" ]] || { echo "error: no $APK" >&2; exit 1; }
echo "Android video harness ready: $APK ($(wc -c <"$APK" | tr -d ' ') bytes)"
