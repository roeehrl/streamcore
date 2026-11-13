// The Android side of StreamCore's video client: the UniFFI Kotlin bindings and
// libstreamcore_video.so from scripts/build-streamcore-video-android.sh, plus the
// MediaCodec decoder and the HandlerThread queue that implement StreamDecoder and
// SerialQueue. Nothing built is committed: syncStreamCoreVideo copies the script's
// output (target/streamcore-video-android) into build/ before every build; run the
// script first, or pass -PbuildStreamCoreVideo to have this task run it.
plugins {
    id("com.android.library")
    id("org.jetbrains.kotlin.android")
}

val coreRoot: File = rootProject.projectDir.parentFile
val coreOutput = File(coreRoot, "target/streamcore-video-android")
val synced = layout.buildDirectory.dir("streamcore-video")
val abis = listOf("arm64-v8a", "armeabi-v7a", "x86_64")

val syncStreamCoreVideo = tasks.register<Sync>("syncStreamCoreVideo") {
    description = "Copies libstreamcore_video.so and the UniFFI Kotlin from target/streamcore-video-android."
    if (providers.gradleProperty("buildStreamCoreVideo").isPresent) {
        doFirst {
            project.exec {
                workingDir = coreRoot
                commandLine("bash", "scripts/build-streamcore-video-android.sh", *abis.toTypedArray())
            }
        }
    }
    doFirst {
        val missing = abis.filterNot { File(coreOutput, "jniLibs/$it/libstreamcore_video.so").isFile } +
            listOf("kotlin/uniffi/streamcore_video/streamcore_video.kt").filterNot { File(coreOutput, it).isFile }
        if (missing.isNotEmpty()) {
            throw GradleException(
                "Missing from target/streamcore-video-android: ${missing.joinToString()}. Run " +
                    "bash scripts/build-streamcore-video-android.sh from the repository root, or build with " +
                    "-PbuildStreamCoreVideo."
            )
        }
    }
    from(coreOutput) {
        include("jniLibs/**/libstreamcore_video.so", "kotlin/**/*.kt")
    }
    into(synced)
}

android {
    namespace = "streamcore.video.android"
    compileSdk = 36
    (rootProject.extra["streamcoreNdkVersion"] as String?)?.let { ndkVersion = it }

    defaultConfig {
        minSdk = 28
        ndk { abiFilters += abis }
        consumerProguardFiles("consumer-rules.pro")
    }

    sourceSets.getByName("main") {
        kotlin.srcDir(synced.map { it.dir("kotlin") })
        jniLibs.srcDir(synced.map { it.dir("jniLibs") })
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    kotlinOptions { jvmTarget = "17" }
}

tasks.named("preBuild") { dependsOn(syncStreamCoreVideo) }

dependencies {
    // UniFFI's Kotlin loads libstreamcore_video.so through JNA (5.19.1: 16 KB aligned).
    api("net.java.dev.jna:jna:5.19.1@aar")
    // The async calls (ClientStream.nextMessage, closeStream, pairWithHost, connectSecure).
    api("org.jetbrains.kotlinx:kotlinx-coroutines-android:1.8.1")
}
