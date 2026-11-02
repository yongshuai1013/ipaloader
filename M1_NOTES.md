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

（待 CI 完成後補）
