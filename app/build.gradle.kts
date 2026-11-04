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
        versionCode = 15
        versionName = "0.5.1"
        ndk {
            abiFilters += "arm64-v8a"
        }
    }

    signingConfigs {
        create("release") {
            // Sign with all three schemes: v2-only APKs get a Play
            // Protect risk prompt on the user's device, while v2+v3
            // (produced by re-signing the same APK) installs cleanly.
            enableV1Signing = true
            enableV2Signing = true
            enableV3Signing = true
            // Credentials come from CI secrets via environment variables;
            // nothing keystore-related is ever committed to this repo.
            val ksPath = System.getenv("KEYSTORE_PATH")
            if (ksPath != null) {
                storeFile = file(ksPath)
                storePassword = System.getenv("KEYSTORE_PASSWORD")
                keyAlias = System.getenv("KEY_ALIAS")
                keyPassword = System.getenv("KEY_PASSWORD")
            }
        }
    }

    buildTypes {
        release {
            isMinifyEnabled = false
            // Sign only when the keystore env is present (CI); local builds
            // without it keep producing an unsigned release APK as before.
            if (System.getenv("KEYSTORE_PATH") != null) {
                signingConfig = signingConfigs.getByName("release")
            }
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

dependencies {
    // Kotlin half of rustls-platform-verifier: it provides
    // org.rustls.platformverifier.CertificateVerifier, which the
    // Rust verifier calls through JNI to check certificate chains
    // with the Android platform verifier. Without it the first
    // platform-verified TLS handshake fails. The version must match
    // the rustls-platform-verifier-android crate version pinned in
    // rust/Cargo.lock; the maven repo lives in settings.gradle.kts.
    implementation("org.rustls:rustls-platform-verifier:0.2.0")
    // WebSocket server for the local WISP relay (WispServer.kt).
    // Plain Java, works on Android; from Maven Central, which CI
    // already uses. Its only transitive dependency is slf4j-api
    // (no binding on Android — it just stays silent).
    implementation("org.java-websocket:Java-WebSocket:1.5.7")
}
