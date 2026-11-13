// A bring-up harness for the video client: pastes video_host's connect line, pairs,
// connects, decodes stream 1 with MediaCodecDecoder onto a full-screen SurfaceView and
// overlays the client's counters. Logs under the tag StreamCoreVideo.
plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

android {
    namespace = "streamcore.video.harness"
    compileSdk = 36
    (rootProject.extra["streamcoreNdkVersion"] as String?)?.let { ndkVersion = it }

    defaultConfig {
        applicationId = "streamcore.video.harness"
        minSdk = 28
        targetSdk = 36
        versionCode = 1
        versionName = "0.1"
        ndk { abiFilters += listOf("arm64-v8a", "armeabi-v7a", "x86_64") }
    }

    buildTypes {
        getByName("release") {
            isMinifyEnabled = false
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    kotlinOptions { jvmTarget = "17" }
}

dependencies {
    implementation(project(":streamcore-video"))
}
