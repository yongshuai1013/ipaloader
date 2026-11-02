pluginManagement {
    repositories {
        google()
        mavenCentral()
        gradlePluginPortal()
    }
}
dependencyResolutionManagement {
    repositoriesMode.set(RepositoriesMode.FAIL_ON_PROJECT_REPOS)
    repositories {
        google()
        mavenCentral()
        // rustls-platform-verifier's Kotlin component is published to
        // the rustls project's own maven archive on GitHub (see the
        // crate README). Its version must match the
        // rustls-platform-verifier-android crate version in
        // rust/Cargo.lock (currently 0.2.0).
        maven {
            url = uri("https://github.com/rustls/rustls-platform-verifier/raw/maven-archive/android-release-support/maven/")
        }
    }
}
rootProject.name = "ipaloader"
include(":app")
