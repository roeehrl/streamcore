plugins {
    id("com.android.application") version "8.5.2" apply false
    id("com.android.library") version "8.5.2" apply false
    id("org.jetbrains.kotlin.android") version "2.0.20" apply false
}

// The newest installed NDK, so the Android Gradle plugin can strip the native libraries
// when it packages them (its own default NDK is usually not installed).
val sdkDir: File? = run {
    val local = rootProject.file("local.properties").takeIf { it.isFile }?.readLines()
        ?.firstOrNull { it.startsWith("sdk.dir=") }?.substringAfter("=")
    listOfNotNull(local, System.getenv("ANDROID_HOME"), System.getenv("ANDROID_SDK_ROOT"))
        .map(::File).firstOrNull { it.isDirectory }
        ?: File(System.getProperty("user.home"), "Library/Android/sdk").takeIf { it.isDirectory }
}
extra["streamcoreNdkVersion"] = sdkDir?.resolve("ndk")?.listFiles()
    ?.filter { it.isDirectory }?.map { it.name }
    ?.maxWithOrNull(compareBy<String> { 0 }.thenComparator { a, b ->
        val x = a.split('.').map { it.toIntOrNull() ?: 0 }
        val y = b.split('.').map { it.toIntOrNull() ?: 0 }
        x.zip(y).map { (p, q) -> p.compareTo(q) }.firstOrNull { it != 0 } ?: x.size.compareTo(y.size)
    })
