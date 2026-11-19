# 0.6.9 工作筆記（匯出配對檔＋清除配對紀錄）

## 基線
- 遠端 HEAD `1b3f1fe3`（0.6.8/code 24）已 API 核實。本機 checkout
  `~/workspace/ipaloader` 本機 commit c988829 = 0.6.8 replay 對應，樹乾淨。

## 配對紀錄儲存事實（讀代碼查證）
- 落盤：`<filesDir>/pairing/` 三件組——`pairing.plist`（原始紀錄 bytes）、
  `udid.txt`、`name.txt`。`store_pairing_files()`（lib.rs）為匯入與 USB
  配對共用寫入點；`load_pairing()` 只讀 udid.txt/name.txt。
- USB 配對（usbpair.rs）：idevice 0.1.68 `LockdownClient::pair()` 產生
  PairingFile——build_pair_request 組 DeviceCertificate／HostCertificate／
  HostID／RootCertificate／RootPrivateKey／WiFiMACAddress／SystemBUID，
  Pair 成功後再插入 HostPrivateKey 與裝置回應的 EscrowBag；
  usbpair 補 `pairing.udid` 後 `serialize()`（XML plist）落盤。
  → USB 配對產的紀錄本體含齊十鍵（EscrowBag 以裝置有回應為前提，
  iPhone 正常會回）。
- 匯入路徑：`do_import_pairing` 要求檔案本身含 UDID 才收，原 bytes 直存。
- 記憶體快取：**無**。install.rs `load_pairing_file()` 每次操作從磁碟
  重讀 pairing.plist 解析；sign flow 每次呼叫 `load_pairing()`；
  UsbMuxProvider 的 get_pairing_file 恆回錯（不持紀錄）。Rust statics
  只有 runtime／log path／RPV（anisette）init，與配對無關。
  → 清除＝刪三件組即可真清除；已在執行中的操作持有自己的 clone，
  會自然完成或失敗（顯名限制，非快取殘留）。
- Kotlin 側：PairingPanel 每次 refresh 都呼叫 nativeGetPairing，無快取。

## 設計
- Rust 新增（lib.rs，綁 SignActivity，與 import/get 同組）：
  - `nativeExportPairing(filesDir, outPath)` → `do_export_pairing`：
    讀 pairing.plist → `build_pairing_export()` 正規化（解析→缺 UDID 時
    以 udid.txt 的已知 UDID 補入→斷言十鍵齊全且非空→重序列化 XML）
    → 寫 outPath（比照 signing key 匯出的 tmp 檔模式）。
    回 `OK\n<udid>\n<name>`；無配對 `ERR\nNOPAIRING`；缺鍵 `ERR\nPAIRING`。
  - `nativeClearPairing(filesDir)` → `do_clear_pairing`：刪三件組
    （＋匯出 tmp），回 `OK\nCLEARED`／`OK\nNONE`。
  - host 測試：`build_pairing_export` 純函數＋fixture（十鍵齊全／
    缺 UDID 補入／缺 EscrowBag 拒絕／垃圾輸入拒絕）。
- Kotlin（PairingPanel）：device 區下方加「配對紀錄」區：狀態沿用
  deviceText；「匯出配對檔」鈕（SAF ACTION_CREATE_DOCUMENT、建議檔名
  `{UDID}.plist`、模式比照 InventoryPanel 簽名私鑰匯出）、私鑰警示小字、
  「清除配對紀錄」鈕（AlertDialog 確認→nativeClearPairing→refresh）。
  未配對時兩鈕 disabled。
- 日誌：只記元資訊（匯出 UDID 前 8 碼／清除完成），內容絕不入誌。
