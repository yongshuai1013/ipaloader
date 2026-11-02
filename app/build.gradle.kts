plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

android {
    namespace = "com.yongshuai.ipaloader"
    compileSdk = 34

    defaultConfig {
        applicationId = "com.yongshuai.ipaloader"
        minSdk = 26
        targetSdk = 34
        versionCode = 3
        versionName = "0.2.1"
        ndk {
            abiFilters += "arm64-v8a"
        }
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
