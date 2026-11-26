plugins { id("com.android.application"); id("org.jetbrains.kotlin.android") }
// The release tool updates these two manifests together and increments the release
// code even when a custom version skips one or more semantic-version components.
fun manifestField(file: String, section: String, field: String): String {
    val lines = providers.fileContents(layout.projectDirectory.file("../../../$file")).asText.get().lineSequence()
    return lines.dropWhile { it.trim() != "[$section]" }.drop(1)
        .takeWhile { !it.trimStart().startsWith("[") }
        .map { it.substringBefore('#').trim() }
        .first { it.substringBefore('=').trim() == field }
        .substringAfter('=').trim().removeSurrounding("\"")
}
val releaseName = manifestField("Cargo.toml", "package", "version")
val releaseCode = manifestField("packaging/magisk/kam.toml", "prop", "versionCode").toInt()
require(manifestField("packaging/magisk/kam.toml", "prop", "version") == "v$releaseName" && releaseCode > 1) {
    "The Android release manifests disagree or the APK cannot update the original versionCode 1"
}

val verifyPackagedCli by tasks.registering {
    val staged = fileTree("src/main/jniLibs") {
        include("arm64-v8a/libssmcp.so", "x86_64/libssmcp.so")
    }
    inputs.files(staged)
    doLast {
        check(staged.files.isNotEmpty() && staged.files.all { it.isFile && it.length() > 0 }) {
            "Build the Android CLI first: run apps/ssmcp-android/build-native.sh [arm64-v8a|x86_64]"
        }
    }
}
tasks.named("preBuild") { dependsOn(verifyPackagedCli) }

android {
    namespace = "dev.ssmcp.mobile"
    compileSdk = 35
    buildToolsVersion = "35.0.0"
    defaultConfig {
        applicationId = "dev.ssmcp.mobile"
        minSdk = 26
        targetSdk = 35
        ndk { abiFilters += listOf("arm64-v8a", "x86_64") }
        versionCode = releaseCode
        versionName = releaseName
        testInstrumentationRunner = "dev.ssmcp.mobile.ProtocolInstrumentation"
    }
    packaging.jniLibs {
        useLegacyPackaging = true
        keepDebugSymbols += "**/libssmcp.so"
    }
    buildTypes {
        release {
            isMinifyEnabled = true
            isShrinkResources = true
            proguardFiles(getDefaultProguardFile("proguard-android-optimize.txt"), "proguard-rules.pro")
        }
    }
    compileOptions { sourceCompatibility = JavaVersion.VERSION_17; targetCompatibility = JavaVersion.VERSION_17 }
    kotlinOptions { jvmTarget = "17" }
}
// Android platform APIs and Kotlin's small runtime only; no UI/network libraries.
