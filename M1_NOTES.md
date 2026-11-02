# M1 Notes — 空殼（2026-11-02）

## 範圍

- GitHub 倉庫 `yongshuai1013/ipaloader`（public）
- Android 空殼：Kotlin 原生 View、Theme.Holo（深色）、無 AppCompat
  - applicationId `app.ipaloader`、minSdk 26、targetSdk 34、compileSdk 34
  - 主畫面：TextView 顯示 Rust 占位函式回傳字串；「登入」按鈕 disabled（M2 啟用）
- Rust core（`rust/`，cdylib `libipaloader_core.so`）：
  - JNI `Java_app_ipaloader_MainActivity_nativeVersion` 回傳
    `rust core ok: isideload 0.5.2 / idevice 0.1.68`
  - 依賴釘版：isideload 0.5.2（default-features=false，
    features = fs-storage / tokio-mt / install——避開安卓無後端的
    keyring-storage）、idevice 0.1.68（usbmuxd）、jni 0.21
- CI（`.github/workflows/build.yml`）：ubuntu-latest → JDK 17 →
  sdkmanager 裝 NDK 27.0.12077973 → cargo build（aarch64-linux-android，
  API 26 clang，與 spike 同配方）→ .so 放入 jniLibs → gradle
  assembleRelease → 上傳 APK artifact

## 本機環境坑

- 本機 Gradle daemon 起不來（sandbox 限制），wrapper 檔案改從
  iDroidLoader 的 Tauri gen/android 專案複用（gradlew／
  gradle-wrapper.jar），distributionUrl 釘 gradle-8.9-bin.zip
  （配 AGP 8.5.2／Kotlin 2.0.20）。CI 上用 wrapper 正常下載發行包。

## CI／驗包結果

- Run 41435930482（2026-11-02 11:19:37Z 觸發，commit
  `76ab5614675c8a03e80658035837a680b62a464c`）：全步驟 success
  （NDK 安裝、Rust core 編譯、Gradle 打包、artifact 上傳）。
- 驗包（下載 artifact 13136243093 後以 aapt2 實查）：
  - package `app.ipaloader`、versionCode 1、versionName 0.1.0
  - minSdkVersion 26、targetSdkVersion 34、compileSdk 34
  - application theme = `@0x0103006b`，對照 AOSP public-final.xml
    即 `Theme.Holo`（深色）✔
  - launchable activity = `app.ipaloader.MainActivity`（exported）
  - `lib/arm64-v8a/libipaloader_core.so` 在包內（ARM64 ELF），
    匯出符號 `Java_app_ipaloader_MainActivity_nativeVersion` ✔
- 簽名狀態：release APK 無簽名設定，出的是
  `app-release-unsigned.apk`（不可直接安裝）；自本版起 CI 同時出
  debug APK（`app-debug.apk`，debug 簽名可安裝）供真機驗 M1 畫面。
  正式簽名設定留待發佈里程碑處理。
