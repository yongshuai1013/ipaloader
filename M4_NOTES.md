# M4 安裝（v0.6.0）

讓 ipaloader 能經 Wi-Fi 把已簽 IPA 裝進 iPhone 並能開啟——
v1 完成定義（M3 簽名已於 2026-11-05 真機驗收通過後開工）。

## API 盤點結論

- **Wi-Fi 連線**：vendored idevice 0.1.68 的
  `provider::TcpProvider`（`tcp` feature，本版新開）＋
  `LockdownClient`（埠 62078，`start_session` 吃配對檔）就是
  函式庫的 Wi-Fi 路；`pairing_file::PairingFile::from_bytes`
  直接解析 M3.1 已保存的 `<filesDir>/pairing/pairing.plist`，
  不重寫協議層。裝置名稱／iOS 版本／UDID 走 lockdown
  GetValue（DeviceName／ProductVersion／UniqueDeviceID）。
- **裝置發現**：函式庫沒有區網搜尋 API，這是唯一要 App 側
  補的一層（不碰協議）：以安卓手機自身子網（至多 /24）掃
  TCP 62078（64 並發、單點 350ms），對回應者開 lockdown
  session 驗 UDID 與導入配對檔的目標 UDID 一致才算找到；
  全程有逾時（驗證單台 8 秒），找不到回 NOTFOUND 停步。
- **安裝**：isideload `sideload::install::install_app` 的安裝
  半段——AFC 上傳 `PublicStaging`（佔進度 0–70%）＋
  InstallationProxy 安裝（70–100%），`Fn(u64)` 進度回調直接
  是百分比。它吃已簽 `.app` **目錄**，所以 IPA 先由 App 側
  解出 `Payload/<App>.app` 子樹（保留 unix 權限與符號連結，
  是 M3 打包的反向；主程式強制補 755）。

## 實作要點

- Rust 新模組 `rust/src/install.rs`：`nativeProbeDevice`
  （發現＋驗證，回 ip／名稱／iOS 版本）、`nativeInstallIpa`
  （先發現、後解包、再安裝，整體 900 秒上限）、以及與簽名頁
  同實作的 `nativeGetPairing`／`nativeImportPairing`（綁到
  InstallActivity）。進度經 `onInstallProgress(int)` 回 Kotlin，
  10% 一筆日誌；錯誤碼 NOPAIRING／PAIRING／BADIP／NOTFOUND／
  LOCKDOWN／EXTRACT／DEVICE（DEVICE 帶裝置回報的原文）。
- Kotlin 新 `InstallActivity`（Holo 深色、繁中）：目標裝置區
  （可導入配對檔）＋連線狀態區（開頁自動探測＋「檢查連線」
  重試）＋選已簽 IPA＋水平進度條與百分比＋人話結果。手機子網
  由 ConnectivityManager 的 LinkProperties 取得（新增
  ACCESS_NETWORK_STATE／ACCESS_WIFI_STATE 權限宣告）。
- 兩條安裝入口：主畫面已登入區「安裝 IPA」（獨立選檔）；
  簽名頁簽名成功後「安裝到 iPhone」（Intent 帶入剛簽好的
  IPA，免再選檔）。未動：證書硬規則、M2 登入、M3 簽名
  流程、WISP／代打。

## 交付

- CI run 41948349571（workflow build）全綠、兩次讀數一致；
  遠端 main HEAD `8ff54d3e5b2eed44d7cd35c2ca2e9869032895d4`
  （本地 commit de7662f 經 git_push 推上），觸發
  2026-11-05 09:24 CST；versionCode 16／versionName 0.6.0。
- 驗包（artifact 13379244333，15,418,318 B，run 級與 repo
  級一致）：release app-release.apk 18,861,738 B、簽名＝
  永帥長期鑰 a409afe8…c6acee、minSdk 26／targetSdk 34
  不變、.so 18,086,272 B 含 4 個新 InstallActivity JNI
  符號、dex 含 InstallActivity 與
  nativeProbeDevice／nativeInstallIpa／onInstallProgress、
  resources 含全部 install_* 新字串。
- 待真機驗收：覆蓋裝 0.6.0 → 安裝頁確認連上 iPhone 7 並
  顯示名稱/版本 → 用 M3 簽好的 BitBar 已簽 IPA 安裝 →
  在 iPhone 上開啟。

## 真機前置條件

- iPhone 與安卓手機連同一個 Wi-Fi（路由器不開 AP 隔離）。
- iPhone 已開啟「透過 Wi-Fi 同步」（曾在 Finder/iTunes 勾
  「透過 Wi-Fi 與此 iPhone 同步」），lockdownd 才會在 Wi-Fi
  上開 62078；驗收時保持 iPhone 喚醒、最好解鎖亮屏。
- 已在 App 內導入該 iPhone 的配對檔（簽名頁或安裝頁皆可）。

## 未做（留給後續里程碑）

- M5：錯誤訊息全面人話化、描述檔到期提醒。
- USB 連線／安裝不在 v1；已裝 App 的描述檔刷新不在 v1。
