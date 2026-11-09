plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

android {
    namespace = "dev.pinemic"
    compileSdk = 35

    defaultConfig {
        applicationId = "dev.pinemic.mic"
        minSdk = 29          // platform Opus encoder (MediaCodec "audio/opus") exists from Android 10
        targetSdk = 34
        versionCode = 1
        versionName = "1.0"
    }

    buildTypes {
        release {
            isMinifyEnabled = false
        }
    }
    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    kotlinOptions {
        jvmTarget = "17"
    }
}

// No third-party dependencies on purpose: nothing to resolve, nothing to break.
