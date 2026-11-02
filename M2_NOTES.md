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

（待 CI 完成後補）
