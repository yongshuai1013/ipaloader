# M2 Notes — 真登入（2026-11-02）

## 範圍

- Rust 核心接 isideload 0.5.2 真登入（GrandSlam SRP＋2FA），session 保存與還原
- Kotlin UI：登入表單／登入中進度／已登入畫面（帳號＋團隊）／登出；
  App 重開先 restore session；錯誤訊息翻成繁中人話（附簡短代碼）
- 版本：versionCode 2 / versionName 0.2.0；新增 INTERNET 權限

## 實作要點

- 登入 API 以 0.5.2 原始碼實查為準：`AppleAccountBuilder::new(email)
  .anisette_provider(..).login(password, two_factor_callback)`；
  callback 型別 `Fn(TwoFactorCallbackParams) -> Fut`，回
  `TwoFactorCallbackResponse`（SubmitCode／SendSms／SendToDevices／
  ResendCode／Abort）。
- anisette：`RemoteV3AnisetteProvider` 指 `https://ani2.stikstore.app/`，
  storage 用 `FsStorage` 釘在 App 私有目錄 `<filesDir>/core/`
  （FsStorage 預設路徑是 "."，不可直接用在安卓）。另照 iloader 現行
  做法設 `client_info_override`（Mac15,7／macOS 27.0／akd claim＋
  `akd/1.0 CFNetwork/811.5.4` UA），與 anisette 伺服器回報值脫鉤。
- session 保存：isideload 0.5.2 沒有現成的帳號序列化；登入成功後把
  `account.spd`（登入令牌所在 plist）以 binary plist 經同一個
  FsStorage 存 `session_spd`＋`session_email`。還原時重建 account、
  塞回 spd，再以 `DeveloperSession::from_account`（app token 交換）
  ＋`list_teams` 真驗證；Apple 明確拒絕（401/403/status code）才清掉
  session 回未登入，網路類失敗保留 session 並回報錯誤。
- 2FA 橋接：Rust 在 tokio  worker 執行緒以 `block_in_place` 呼叫
  Kotlin `MainActivity.onTwoFactorNeeded(kind, numbers, lastError)`；
  Kotlin 在 UI 執行緒開 Holo AlertDialog（依 kind 顯示信任裝置／簡訊
  輸入，或 unknown 時的驗證方式選擇），以 CountDownLatch 等待
  （上限 300 秒，逾時視同取消）。回覆協定字串：
  `code:<碼>`／`sms:<號碼id>`／`devices`／`resend`／`abort`。
- **密碼與驗證碼不寫任何 log**：Rust 側無敏感值輸出；Kotlin 側不印；
  登入嘗試結束後清空密碼欄。

## SMS 身份核查（重點）

查 isideload 0.5.2 原始碼 `auth/grandslam.rs::base_headers`：SMS 三
端點（取號碼 `GET /auth`、發送 `PUT /auth/verify/phone`、驗證
`POST /auth/verify/phone/securitycode`）與 SRP／信任裝置 2FA 用的是
**同一個 client claim**——`base_headers(client_info, sms)` 的 sms
旗標只切換 Content-Type／Accept（JSON vs plist），`X-Mme-Client-Info`
與 `User-Agent` 一律取自 GrandSlam 的 `client_info`，來源是 anisette
provider 的 `get_client_info()`（或 override）。也就是：

- 本 App 的 SMS 會帶 **Mac claim**（理由如上：StikStore 回報值本身是
  MacBookPro13,3／macOS 10.12.6 akd claim——已於 2026-11-02 實查
  `ani2.stikstore.app/v3/client_info`；且我們又照 iloader 設了
  Mac15,7 override）。**不是** 2026-10-14 起被 Apple 擋的 Android
  自稱形狀，無需 vendored patch。
- 受影響範圍提醒：若日後把 anisette 來源換成「Android ADI 的伺服器」
  （例如自建 anisette-v3-server 跑 Pixel 庫），且拿掉 override，
  claim 就會變 Android，SMS 發送／驗證可能重演 EiliStore 2026-10
  的 403／空回應；信任裝置 2FA 不受此影響。屆時修法在 provider
  層（設 client_info_override 即可，不必改 isideload 本體）。

## CI／驗包結果

- 本機 `cargo check --target aarch64-linux-android` 先過（零錯誤）。
- CI run 41439701189（2026-11-02 11:48:34Z 觸發，commit
  `d192fd22694c78233916b960cc197ce506f233cf`）：success（兩次獨立
  讀數一致）。artifact `13137684806`（ipaloader-apk，7.0 MB）。
- 驗包（aapt2 實查 debug 與 release 兩包，一致）：
  - package `app.ipaloader`、versionCode 2、versionName 0.2.0
  - minSdkVersion 26、targetSdkVersion 34
  - application theme = `@0x0103006b` ＝ Theme.Holo（深色）✔
  - uses-permission `android.permission.INTERNET` ✔
  - launchable activity = `app.ipaloader.MainActivity` ✔
  - `lib/arm64-v8a/libipaloader_core.so` 在包內，匯出符號含
    `Java_app_ipaloader_MainActivity_nativeLogin`、
    `nativeRestoreSession`、`nativeLogout`、`nativeVersion` ✔
- 簽名狀態同 M1：release 未簽名不可裝；真機測試請裝 artifact 裡的
  `app-debug.apk`。

## 真機測試步驟（M2 驗收）

1. 下載 CI run 41439701189 的 artifact，裝 `app-debug.apk`（覆蓋
   M1 的 debug 版可直接升級，versionCode 2）。
2. 開 App：應先顯示「正在檢查登入狀態…」，然後回到登入表單
   （第一次還沒有 session）。
3. 輸入真 Apple ID＋密碼按「登入」：若帳號有 2FA，先到信任裝置
   會收到驗證碼，App 應跳出「信任裝置驗證」對話框；輸入六位碼
   提交。想改走簡訊：按對話框裡「以簡訊傳送到 ••XX」換號碼，
   之後對話框會變簡訊模式再輸入簡訊碼。
4. 登入成功後主畫面應顯示帳號與團隊名稱（個人團隊會顯示你的
   名字），並有「登出」鈕。
5. 把 App 從最近清單劃掉重開：應直接回到已登入畫面（session
   還原，中間會顯示檢查狀態），不必再輸入密碼。
6. 按「登出」後重開 App：應回到登入表單。
7. 錯誤路徑：故意打錯一次密碼，應顯示「Apple ID 或密碼不正確」
   而不是整坨英文錯誤。

## 包名變更實驗（2026-11-02，versionName 0.2.1）

- 動機：M2 的 debug APK 在真機安裝時被 Google Play Protect 以
  「已封鎖有害的應用程式」擋下；用戶要單一變量驗證封鎖是否與
  包名有關。
- 變更（僅身份，功能內容與 M2 完全相同）：
  - applicationId／namespace：`app.ipaloader` →
    `com.yongshuai.ipaloader`
  - Kotlin package 與原始碼目錄同步改為
    `com/yongshuai/ipaloader/`
  - Rust JNI 符號前綴改為
    `Java_com_yongshuai_ipaloader_MainActivity_*`（nativeVersion／
    nativeLogin／nativeRestoreSession／nativeLogout 四個）
  - versionCode 3 / versionName 0.2.1
  - 未接正式簽名金鑰（與包名分開試，簽名維持 CI debug 簽名）
- 本機 `cargo check --target aarch64-linux-android` 先過（零錯誤）。
- CI run 41445500231（2026-11-02 12:32:11Z 觸發，commit
  `18290f4a8863115ec6f0665c106a0e6812ae5c72`）：success（jobs 全
  步驟綠；run 層級曾出現代理假狀態——先報 success 又閃回
  in_progress，以 jobs 明細＋artifact 清單穩定為準）。artifact
  `13142647089`（ipaloader-apk，7,012,488 B）。
- 驗包（aapt2 實查 debug 與 release 兩包，一致）：
  - package `com.yongshuai.ipaloader`、versionCode 3、
    versionName 0.2.1、minSdkVersion 26、targetSdkVersion 34
  - application theme = `@0x0103006b` ＝ Theme.Holo（深色）✔
  - launchable activity = `com.yongshuai.ipaloader.MainActivity` ✔
  - `lib/arm64-v8a/libipaloader_core.so` 在包內，llvm-nm 實查
    匯出符號為 `Java_com_yongshuai_ipaloader_MainActivity_` 前綴
    四個（nativeVersion／nativeLogin／nativeRestoreSession／
    nativeLogout），舊前綴 0 個 ✔
- 簽名狀態同 M2：release 未簽名不可裝；真機測試請裝 artifact
  裡的 `app-debug.apk`。注意：換包名後在 Android 上是另一個
  App，舊 `app.ipaloader` 版可卸除。

## 固定正式簽名（v0.2.2，2026-11-02）

- 背景：包名換成 `com.yongshuai.ipaloader` 後 Play Protect 仍擋，
  下一個單一變量＝簽名。原先延後的固定金鑰待辦一併在此完成。
- 已建固定 release keystore：alias `ipaloader`、RSA 2048、
  validity 10000 天。證書 SHA-256 指紋（公開資訊）：
  `37:80:D3:68:3D:61:96:23:D9:78:1F:A7:B6:DC:A0:18:7E:1B:EA:06:80:E5:13:6A:D7:9A:E4:FA:D9:0A:B7:F6`
- 金鑰與密碼：**不在本倉庫**。已上 GitHub Secrets
  （KEYSTORE_BASE64／KEYSTORE_PASSWORD／KEY_ALIAS／KEY_PASSWORD），
  本地備份在目標工作區 hidden_files（權限 600）。筆記與倉庫只記指紋。
- CI／Gradle：workflow 從 Secrets 還原 keystore 至 runner 臨時
  目錄並以 `KEYSTORE_PATH` 環境變數傳給 Gradle
  `signingConfigs.release`；本機無此環境變數時 release 維持未
  簽名（行為不變）；debug 構建維持 CI 臨時 debug 金鑰。
- `.gitignore` 已加 `*.jks`／`*.keystore`／`keystore.properties`／
  `release-keystore*` 防誤推。
- versionCode 4 / versionName 0.2.2，功能未動。
- CI run 41447718538（2026-11-02 12:48:39Z 觸發，commit
  `8e5c284910dbd764c867673e21270e1fc2081dd4`）：success（兩次
  讀數一致、jobs 全 12 步驟綠、artifact 清單穩定；run 作用域
  端點一度給出假 artifact ID，改以 repo 級清單按 run 號核對
  為準）。artifact `13143625737`（ipaloader-apk，7,013,858 B）。
- 驗包（apksigner＋aapt2 實查 release 包）：
  - `apksigner verify --print-certs`：簽名者 DN
    `CN=ipaloader, O=ipaloader release`，SHA-256 digest
    `3780d3683d619623d9781fa7b6dca0187e1bea0680e5136ad79ae4fad90ab7f6`
    ＝keystore 證書指紋 ✔（非 Android Debug）；debug 包仍為
    Android Debug 簽名（行為不變）。
  - package `com.yongshuai.ipaloader`、versionCode 4、
    versionName 0.2.2、minSdk 26、targetSdk 34 ✔
  - application theme = `@0x0103006b` ＝ Theme.Holo（深色）✔
  - `lib/arm64-v8a/libipaloader_core.so` 在包內，四個
    `Java_com_yongshuai_ipaloader_MainActivity_` 符號齊全 ✔
- **真機安裝注意**：這次要裝 artifact 裡的
  `app-release.apk`（已簽名、可裝）；簽名與先前 debug 版不同，
  裝前先卸除舊版 `com.yongshuai.ipaloader`（0.2.1）。之後的
  版本皆同一把金鑰，可直接覆蓋更新。

## 簽名方案 V1+V2+V3（v0.2.3，2026-11-02）

- 起因（真機實驗）：v2-only 的 0.2.2 release 安裝時跳 Play
  Protect 風險提示；同一包經 MT 管理器重簽成 V2+V3 後不跳、
  且成功安裝。用戶指示在自家固定金鑰下複製同一簽名條件。
- 改動：`app/build.gradle.kts` 的 `signingConfigs.release`
  明確加 `enableV1Signing = true`、`enableV2Signing = true`、
  `enableV3Signing = true`（Kotlin DSL 屬性名）；versionCode
  5 / versionName 0.2.3，功能零改動、debug 簽名不動。
- CI run 41450202356（2026-11-02 13:07:06Z 觸發，commit
  `27d21cd123f62ad17023d5cb22ec9a5e5c48c94f`）：success（兩次
  讀數一致、jobs 12 步驟全綠）。artifact `13144563091`
  （ipaloader-apk，7,017,344 B；ID 經 repo 級清單核對）。
- 驗包（apksigner＋aapt2 實查 release 包）：
  - 預設 `apksigner verify --verbose`：v1 false／v2 true／
    v3 true；但 APK 內含 `META-INF/MANIFEST.MF`＋`CERT.SF`＋
    `CERT.RSA`（0.2.2 時無此三檔），v1 簽名本體確實存在；
    加 `--min-sdk-version 23` 重驗：**v1／v2／v3 全 true**。
    預設模式的 v1 false 是 apksigner 對 minSdk ≥ 24 跳過
    JAR 簽名驗證的既定行為，非簽名缺失。
  - 簽名者同固定證書：DN `CN=ipaloader, O=ipaloader
    release`，SHA-256 `3780d368…fad90ab7f6` ✔
  - package `com.yongshuai.ipaloader`、versionCode 5、
    versionName 0.2.3、minSdk 26、targetSdk 34 ✔
  - application theme = `@0x0103006b` ＝ Theme.Holo ✔；
    `libipaloader_core.so` 在包內，四個
    `Java_com_yongshuai_ipaloader_MainActivity_` 符號齊全 ✔
  - debug 包維持 v2-only＋CI 臨時 debug 金鑰（未動）✔
- **真機安裝注意**：裝 artifact 裡的 `app-release.apk`；
  與 0.2.2 同一把固定金鑰，可直接覆蓋更新免卸除。待真機
  驗：Play Protect 風險提示是否消失。

## 換用永帥自備簽名證書（v0.2.4，2026-11-02）

- 背景：0.2.3（Pax 臨時金鑰＋V1+V2+V3）真機仍跳 Play Protect
  提示；永帥決定自己生成簽名證書長期保管，取代 Pax 的臨時金鑰。
- 新證書：DN `CN=yongshuai, OU=yongshuai, O=yongshuai, L=, ST=TW, C=TW`，
  有效期 2026-11-02 至 2076-10-20，SHA-256 指紋
  `A4:09:AF:E8:10:28:28:F8:50:E4:4D:BA:2A:24:12:3B:08:EB:E5:03:9C:FF:10:4C:25:4A:57:67:25:C6:AC:EE`。
- GitHub Secrets 四項已換新值；CI／Gradle 簽名機制與 V1+V2+V3
  設定不動。本機備份已換至目標資料夾，舊金鑰備份刪除退役。
- CI run 41452697769（workflow #11），commit
  `4be09441b4732c9991f1dd1d010d2a02e8c03ae0`，全綠（12 步驟）。
- 驗包（artifact 13146216152 下載實查）：
  - apksigner：v2 ✔／v3 ✔；簽名者 DN 與指紋與新證書一致 ✔
  - package `com.yongshuai.ipaloader`、versionCode 6／0.2.4、
    minSdk 26／targetSdk 34 ✔；theme `@0x0103006b`（Theme.Holo）✔
  - `libipaloader_core.so` 在包內，JNI 符號字串齊全 ✔
- **真機安裝注意**：證書已換，裝 0.2.4 前須先卸除舊版
  （Pax 金鑰簽的 0.2.2／0.2.3 或 MT 重簽版），之後同證書版本
  可直接覆蓋更新。待真機驗：Play Protect 提示與登入實測。

## 修登入閃退＋日誌功能＋panic 攔截（v0.2.5，2026-11-02）

- 閃退定案：isideload 0.5.2 釘 reqwest 的 `rustls-no-provider`，
  宿主必須先安裝 rustls CryptoProvider；我們從未安裝，
  do_login 第一步建 RemoteV3AnisetteProvider 時 reqwest 建構
  panic，release 的 `panic="abort"` 使其變 SIGABRT——按登入瞬間
  閃退。JNI 符號、簽名對齊、tokio 上下文、callback 描述符經逐項
  查證皆無問題。
- 主修：`init_core()`（OnceLock/Once 全域初始化）在任何建 client
  的路徑之前執行
  `rustls::crypto::aws_lc_rs::default_provider().install_default()`
  （重複安裝的 Err 忽略）；Cargo.toml 直依 rustls 0.23
  （default-features=false＋aws_lc_rs/std/tls12，與 Cargo.lock
  內的 0.23.46 對齊）。同處設 panic hook 把 panic 訊息寫進日誌。
- panic 攔截：release profile 改 `panic="unwind"`；nativeLogin／
  nativeRestoreSession 本體包 catch_unwind，panic 時回
  `ERR\nPANIC\n<訊息>` 協定字串，Kotlin 側翻成「內部錯誤
  （已記錄日誌）」。
- 順修：`classify()` 的 `String::truncate(220)` 改字元邊界安全
  截斷（多字節字元落在 220 處原本會 panic）。
- 日誌功能（永帥點名要的）：`<filesDir>/core/ipaloader.log`，
  append、每行帶時間戳（Rust 側 [core]、Kotlin 側 [app] 共用
  一檔）。Rust 記登入里程碑（開始、anisette 取得、SRP 結果、
  2FA 事件種類、錯誤全文鏈、panic）；Kotlin 記按鈕與對話框
  動作。紅線：密碼、2FA 碼、session/spd/token 內容不寫日誌。
  UI：主畫面「日誌」鈕進 LogActivity（Holo 深色、等寬字、
  可捲動），頁內「複製全部」與「清除」。
- CI run 41455183303（workflow #13），commit
  `eece3408e2adfc12ca3741cc590478c044902060`，兩次讀數一致、
  jobs 12 步驟全綠。
- 驗包（artifact 13146358888，repo 級清單核對後下載實查）：
  - apksigner：v2 ✔／v3 ✔；簽名者為永帥證書 DN
    `CN=yongshuai, OU=yongshuai, O=yongshuai, L=, ST=TW, C=TW`，
    SHA-256 `a409afe8…c6acee` ✔
  - package `com.yongshuai.ipaloader`、versionCode 7／0.2.5、
    minSdk 26／targetSdk 34 ✔；theme `@0x0103006b`（Theme.Holo）✔
  - `.so` 在包內（11.99 MB，含 unwind 表與 aws-lc provider），
    四個 JNI 符號齊全；新字串實查在包內：`ipaloader.log`、
    `core init: rustls CryptoProvider (aws-lc-rs) installed`、
    `login start:` ✔
- **真機安裝注意**：與 0.2.4 同證書，可直接覆蓋更新免卸除。
  待真機驗：按登入應走到網路／2FA 或顯示錯誤（不再閃退）；
  日誌頁應有全程紀錄；登入＋2FA 實測。

## 0.2.6 — 登入死在建 HTTP client 的真修復（2026-11-02 晚）

0.2.5 真機日誌定案的錯誤鏈：`login: SRP login begin` →
GrandSlam 建 client 失敗「Failed to build HTTP client /
builder error / rustls-platform-verifier could not load extra
certs」（isideload grandslam.rs:250），且被 classify 誤標
NETWORK（實測網路正常）。讀 reqwest 0.13.5／rustls-platform-
verifier 0.7.1／isideload 0.5.2 三方原始碼後，確認是三層：

1. **建構期硬錯誤（本錯誤本體）**：isideload 用
   `.add_root_certificate(APPLE_ROOT)` 加根憑證。reqwest
   0.13.5 在 `!tls_certs_only`（平台驗證）模式下，Android 被
   cfg 排除在 `new_with_extra_roots` 之外——RPV 的 Android
   verifier 本來就不支援 extra roots——非空 root_certs 在
   Android 上**無條件**回此 builder error，與執行期 init
   無關，桌面平台（macOS/Windows/unix）則正常。
2. **握手期缺 init**：RPV 在 Android 需先以 JavaVM＋應用
   Context 呼叫 `android::init_with_env`，且 APK 內要有它的
   Kotlin 元件 `org.rustls.platformverifier.
   CertificateVerifier`（經 app class loader 以 JNI 呼叫），
   兩者缺一，第一次平台驗證握手（anisette 抓取、portal
   呼叫）就會失敗。0.2.5 兩者皆無（SRP 還沒走到握手就先死
   在第 1 層）。
3. **誤標**：client 建構錯誤被標 NETWORK。

修法（commit `f091c47` 本地／遠端 `fc2a213f`）：

- **vendored isideload 一行 patch**：crates.io 0.5.2 原樣
  複製進 `rust/vendor/isideload`，`rust/Cargo.toml` 以
  `[patch.crates-io]` 接入；grandslam.rs 的
  `.add_root_certificate(cert)` 改為
  `.tls_certs_only([cert])`——同一個 Apple 根信任錨改走
  WebPKI verifier（GrandSlam 端點憑證鏈本就根於 Apple
  Root CA），此路徑在 Android 可正常建構。其餘 vendored
  內容零改動。
- **RPV init**：四個 JNI 入口（nativeVersion／
  nativeRestoreSession／nativeLogin／nativeLogout）以
  AtomicBool 保證只做一次 `ensure_rpv_init`：以 jni 0.21
  在 Activity 上取 `getApplicationContext`，橋接 jni 0.22
  （Cargo.toml 以 `jni22` 改名直依 `=0.22.4`，與 lock 對齊）
  的 `EnvUnowned::from_raw`＋`with_env` 呼叫
  `rustls_platform_verifier::android::init_with_env`。
  結果寫入現有日誌（`platform verifier init ok`／
  `platform verifier init FAILED: …`；nativeVersion 先於
  日誌路徑就緒時先暫存，init_core 補寫）。`rustls-
  platform-verifier = "=0.7.1"` 直依（對齊 lock）。
- **Kotlin 元件進包**：settings.gradle.kts 的
  dependencyResolutionManagement 加 rustls 官方 maven
  倉（GitHub maven-archive）；app 依賴
  `org.rustls:rustls-platform-verifier:0.2.0`（版本須與
  Cargo.lock 的 rustls-platform-verifier-android crate
  版本一致，現同為 0.2.0）。
- **classify 順修**：錯誤鏈含「failed to build http
  client」→ UNKNOWN（優先於 NETWORK 桶），畫面顯示原始
  訊息，不再誤導成網路錯誤。
- versionCode 8／versionName 0.2.6；登入流程與其餘功能
  零改動；簽名沿用永帥 keystore＋V1+V2+V3。本機
  `cargo check --target aarch64-linux-android` 先過再推。
- CI run 41458156068（workflow #15），commit
  `fc2a213f2c0c25e9b9908e315a355403739efaa2`，兩次讀數
  一致、jobs 全綠（含 Build Rust core／Build APK）。
- 驗包（artifact 13148242039，repo 級清單核對 ID 後
  下載，大小 10,493,686 B 與清單一致）：
  - apksigner：v2 ✔／v3 ✔（`--min-sdk-version 23` 下
    v1 亦 ✔）；簽名者永帥證書 SHA-256 `a409afe8…c6acee` ✔
  - package `com.yongshuai.ipaloader`、versionCode 8／
    0.2.6 ✔；theme `@0x0103006b`（Theme.Holo）✔
  - classes.dex 實查含 `org/rustls/platformverifier/
    CertificateVerifier`（及 StatusCode／
    VerificationResult）——Kotlin 元件確實進包 ✔
  - `.so` 四個 JNI 符號齊全；新字串在包內：`platform
    verifier init ok`、`getApplicationContext`
    `()Landroid/content/Context;`、classify 的
    `failed to build http client` ✔
- **真機安裝注意**：與 0.2.4／0.2.5 同證書，可直接覆蓋
  更新。待真機驗：按登入後 SRP 應真正發出——日誌應出現
  `platform verifier init ok`，並走到 SRP 結果或 2FA
  對話框；若再有 TLS 問題，日誌會留下完整錯誤鏈。
## 0.2.7 anisette URL 雙斜線修復（2026-11-02）

- **症狀（0.2.6 真機日誌）**：SRP 已能真正發出，但登入死在
  anisette provisioning：WS 連線 `.../remote_v3/websocket.rs`
  回 HTTP 404「Failed to connect to provisioning socket」。
- **根因**：`rust/src/lib.rs` 的 `ANISETTE_URL =
  "https://ani2.stikstore.app/"` 帶尾斜線，vendored isideload
  以 `format!("{}/v3/...")` 拼接 → `//v3/client_info`、
  `//v3/get_headers`、`//v3/provisioning_session` 全被伺服器
  404。主線實測對照：雙斜線兩路徑皆 404；單斜線
  `GET /v3/client_info` = 200、`WS /v3/provisioning_session`
  握手 = 101。
- **修法（單點）**：常數改為無尾斜線
  `"https://ani2.stikstore.app"`；全倉 grep 確認該常數僅
  lib.rs 一處定義、一處使用，vendored 三個拼接點皆假設
  無尾斜線，無其他受影響處。
- versionCode 9／versionName 0.2.7；其餘零改動；簽名沿用
  永帥 keystore＋V1+V2+V3。本機 `cargo check --target
  aarch64-linux-android` 先過再推。
- CI run 41461306166（workflow #17），commit
  `7c471bb181baec17a7eedb655492287487ef012d`，兩次讀數
  一致、jobs 12 步驟全綠。
- 驗包（artifact 13149566196，repo 級清單核對 ID 後
  下載，大小 10,493,554 B 與清單一致）：
  - apksigner：v2 ✔／v3 ✔（`--min-sdk-version 23` 下
    v1 亦 ✔）；簽名者永帥證書 SHA-256 `a409afe8…c6acee` ✔
  - package `com.yongshuai.ipaloader`、versionCode 9／
    0.2.7、minSdk 26／targetSdk 34 ✔；theme
    `@0x0103006b`（Theme.Holo）✔
  - `.so` 字串實查：`https://ani2.stikstore.app`（無尾
    斜線）✔、尾斜線形式計數 0 ✔；四個 JNI 符號齊全 ✔
- **真機安裝注意**：與 0.2.4 起同證書，可直接覆蓋更新。
  待真機驗：按登入後 provisioning WS 應連通（不再出現
  404 的 provisioning 錯誤），走到 SRP 結果或 2FA 對話框。

## 0.2.8 記住帳號＋密碼（2026-11-02）

- **新功能（用戶拍板：帳密都記、密碼加密存）**：登入頁加
  Holo CheckBox「記住帳號」（預設勾選），手寫 helper
  `CredentialStore.kt`、無新依賴：
  - 登入成功＋有勾選 → 保存；未勾選登入成功 → 清除先前
    保存。登出不清（用途是 session 失效後快速重登）。
  - email 存 SharedPreferences（明文）；密碼以 Android
    Keystore AES/GCM/NoPadding 金鑰（alias
    `ipaloader_cred_key`）加密，僅密文＋IV（Base64）落盤。
  - 啟動／回到登入頁自動帶入並呈勾選；解密失敗（金鑰
    失效等）→ 清除保存值、空白啟動不閃退。密碼絕不
    明文落盤、絕不寫日誌（沿用紅線）。
- versionCode 10／versionName 0.2.8；Rust 側零改動；
  簽名沿用永帥 keystore＋V1+V2+V3。
- CI run 41462556449（workflow #19），commit
  `1a4d17af19939c323c31e19653fcf72c6a38090e`，兩次讀數
  一致、jobs 全步驟綠。
- 驗包（artifact 13149597343，repo 級清單按 run 號
  核對 ID 後下載，大小 10,496,560 B 與清單一致）：
  - apksigner：v2 ✔／v3 ✔（`--min-sdk-version 23` 下
    v1 亦 ✔）；簽名者永帥證書 SHA-256
    `a409afe8…c6acee` ✔
  - package `com.yongshuai.ipaloader`、versionCode 10／
    0.2.8、minSdk 26／targetSdk 34 ✔；theme
    `@0x0103006b`（Theme.Holo）✔
  - classes.dex 字串實查含 `ipaloader_cred_key`、
    `saved_credentials`、`remember_account` ✔
- **真機安裝注意**：與 0.2.4 起同證書，可直接覆蓋
  更新。待真機驗：登入成功 → 劃掉重開 → 帳密應自動
  帶入、CheckBox 呈勾選；取消勾選再登入成功 → 重開
  應空白。
