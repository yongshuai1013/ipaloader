# M3 Notes — 簽名（2026-11-04）

## M3.1 補修（2026-11-05，versionCode 15 / 0.5.1）

- 真機首驗：cert／appid 兩階段過，死在 profile 階段
  DeveloperError(8220, "Your team has no devices from which to
  generate a provisioning profile")——yiaoabb 團隊（ABPWV4MYHV）
  未註冊任何裝置 UDID，M3 流程缺註冊裝置這一步
- 修法（把 M4 的一小塊提前）：
  - 裝置來源＝導入配對檔（v1 原定路線）：簽名頁新增「目標裝置」
    區＋「導入配對檔」——Rust 以 plist crate 解析 lockdown 配對
    plist（XML／binary 皆可），取 `UDID`（標準配對記錄無裝置名，
    有 `DeviceName` 才帶），原檔存 `<filesDir>/pairing/
    pairing.plist` 供 M4 連線用，UI 只顯示 UDID 後 8 碼
  - 簽名流程在最前面新增「裝置」階段：無配對檔直接停步回
    `NODEVICE`（不再讓 8220 原樣出現）；有則以 isideload
    `DevicesApi`（listDevices／addDevice，QH65B2、iOS 段）先
    list 比對、目標 UDID 不在才註冊（名稱＝配對檔裝置名，
    無則 `ipaloader`），35 競態比照函式庫視同已註冊；註冊的
    團隊與接著 `sign_app` 用的團隊以 `get_team()` 快取＋
    `Some(team)` 明確傳入保證同一隊
  - 錯誤分類修正（只限簽名路徑）：新增 `sign_err_string`——
    從 report 因果鏈擷取 `SideloadError::DeveloperError` 回
    `PORTAL`＋「錯誤碼：伺服器原文」；NETWORK 只留給真正的
    傳輸失敗（reqwest send／DNS／逾時）。8220 先前被歸
    NETWORK，是因為其訊息內含 "Connect" 與 "Certificates"
    撞上通用啟發式關鍵字；登入路徑的 `classify` 不動
- 證書硬規則未動（machineName／永不撤銷／CERTLIMIT 全沿用）

## 範圍

- 已登入狀態下選 IPA → App 內完成簽名 → 產出可安裝的已簽 IPA，
  並顯示描述檔到期時間（建立日起 7 天）；簽名階段與結果進日誌頁
- 版本：versionCode 14 / versionName 0.5.0

## isideload 0.5.2 簽名面盤點（先查源碼再動手）

- 證書：`CertificatesApi`（list_ios_certs／submit_development_csr）
  ＋`CertificateIdentity::retrieve`——依 machineName 與存儲私鑰的
  公鑰比對複用自有證書，無匹配才送 CSR 新建；撞上限是 Apple
  DeveloperError 7460，走 `MaxCertsBehavior` 回調
- App ID：`Application::register_app_ids` 先 list 比對、已存在
  複用、缺的才 add（另有 available_quantity 檢查）
- 描述檔：`download_team_provisioning_profile`（team profile，
  回應含 date_expire）
- 簽名：`sideload::sign::sign`（apple-codesign-quick 的
  sign_bundle）；編排在 `Sideloader::sign_app`（證書→改 bundle
  id 為 `<原 id>.<teamId>`→App ID→app group→profile→簽名），
  回傳已簽 .app 目錄
- 結論：簽名面齊備、不用重寫協議層。App 側只補三個缺口：
  ① 已簽 .app 重新打包成 IPA ② 嵌入描述檔到期解析
  ③ Android 的 temp_dir 指向（見下）

## 證書硬規則的落實

- machineName 固定常數 `ipaloader`（rust/src/lib.rs 的
  MACHINE_NAME），Sideloader 以此建構
- 永不自動撤銷：MaxCertsBehavior 用 `Prompt`，回調只記錄現有
  證書清單並回 `Ok(None)`（一張都不選）→ 函式庫以 7460 錯誤
  停下、任何證書未被動過 → JNI 回 `CERTLIMIT\n<json>`，UI 列出
  現有證書（名稱／machineName／到期）＋人話說明，請用戶在其他
  工具手動撤銷後重試
- 私鑰存儲沿用 M2 的 FsStorage（`<filesDir>/core/`），與 session
  同一存儲根，證書複用可跨重啟

## 實作要點

- Rust：`nativeInspectIpa`（Application 解析 bundle id／名稱／
  版本，不碰網路）＋`nativeSignIpa`（還原 session →
  DeveloperSession::from_account 真驗證 → Sideloader 簽名 →
  自家 zip 打包 → 解析 embedded.mobileprovision 的
  ExpirationDate）；進度以 stage key（cert／appid／profile／
  sign／zip）回呼 `SignActivity.onSignProgress`
- Android 專屬修法：isideload-vfs 的 temp_dir 走
  `std::env::temp_dir`，安卓無 TMPDIR 時是不可寫的 `/tmp`；
  簽名前以 TMPDIR 指到 `<filesDir>/core/tmp`
- 打包：zip 條目 `Payload/<App>.app/…`，保留 unix 權限（主程式
  執行位）與符號連結；簽完清理解包暫存樹
- Kotlin：新 `SignActivity`（Holo 深色、繁中）——系統檔案挑選
  複製到 `sign_input/` → 顯示目標 App 資訊 → 階段清單 →
  產出在 `signed/<原名>-signed.ipa`，顯示到期時間（UTC 轉本地）
  與簽名後 bundle id，附「另存到…」（CREATE_DOCUMENT）
- 入口：主畫面已登入區新增「簽名 IPA」按鈕
- 新依賴只有兩個且都在原 Cargo.lock 內：apple-codesign-quick
  0.1.0（isideload 自己的簽名引擎，借其 profile 解析）、zip 8
  （規格與 isideload 相同：default-features=false＋deflate）；
  Cargo.lock 只多兩條 ipaloader_core 的依賴邊

## 未做（留給後續里程碑）

- M4：經 Wi-Fi 把已簽 IPA 裝進 iPhone（本次產出可先以其他
  方式驗裝）
- 簽名頁不提供撤銷證書的入口（硬規則：只列出、用戶手動決定）
