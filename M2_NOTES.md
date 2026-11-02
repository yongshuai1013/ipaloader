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
