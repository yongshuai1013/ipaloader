//! 0.6.3: in-app USB pairing. Connects to a device that has
//! never been paired (or whose stored pairing no longer
//! matches) over USB, runs lockdownd's `pair()` handshake —
//! the part where the user taps "Trust" on the device — and
//! stores the resulting pairing file in exactly the shape
//! `do_import_pairing` writes (pairing.plist + udid.txt +
//! name.txt under `<filesDir>/pairing/`), so Wi-Fi signing
//! and installing pick it up unchanged.
//!
//! The upper half is idevice's `pair` feature
//! (`LockdownClient::pair` generates the host/root
//! certificate pair and the record); the lower half is the
//! minimal USB mux client in `usbmux.rs`, because idevice
//! 0.1.68 has no direct-USB provider.

use std::sync::Arc;
use std::time::Duration;

use idevice::lockdown::LockdownClient;
use idevice::{IdeviceError, IdeviceService as _};
use jni::objects::{JObject, JString};
use jni::sys::jstring;
use jni::JNIEnv;

use crate::usbmux::{UsbBridge, UsbMuxProvider, UsbMuxStream};
use crate::{
    ensure_rpv_init, init_core, jstring_to_rust, load_pairing, log_line,
    make_jstring, mask_udid, panic_message, runtime, store_pairing_files,
};

/// Total budget for the Pair request: the device answers
/// immediately once the user taps, and the wait is otherwise
/// bounded by usbmuxd-style dialog patience plus margin.
const PAIR_TRUST_TIMEOUT: Duration = Duration::from_secs(120);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const GETVALUE_TIMEOUT: Duration = Duration::from_secs(10);

/// One short, single-line version of an error for the UI.
fn short_line(text: &str) -> String {
    let first = text.lines().next().unwrap_or("").trim();
    const MAX: usize = 220;
    if first.chars().count() > MAX {
        let mut s: String = first.chars().take(MAX).collect();
        s.push('…');
        s
    } else {
        first.to_string()
    }
}

/// Random UUID (v4, uppercase) for the host identity. These
/// are identifiers only — the actual key material is
/// generated inside idevice's `pair()` — so a weak fallback
/// is acceptable if /dev/urandom is somehow unavailable.
fn uuid_v4() -> String {
    let mut b = [0u8; 16];
    let read = std::fs::File::open("/dev/urandom").and_then(|mut f| {
        use std::io::Read as _;
        f.read_exact(&mut b)
    });
    if read.is_err() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let mut x = (nanos as u64)
            ^ ((std::process::id() as u64) << 32)
            ^ ((nanos >> 64) as u64);
        for byte in b.iter_mut() {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            *byte = (x & 0xff) as u8;
        }
        logf!("usbpair: /dev/urandom unavailable, host identity from fallback RNG");
    }
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    format!(
        "{:02X}{:02X}{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11],
        b[12], b[13], b[14], b[15]
    )
}

/// RAII: every exit path closes the mux stream (RST + IO
/// thread shutdown).
struct CloseGuard(UsbMuxStream);

impl Drop for CloseGuard {
    fn drop(&mut self) {
        self.0.close();
    }
}

/// Map a transport-level failure to its UI code: version
/// negotiation problems get their own code, everything else
/// is a generic connect failure carrying the detail.
fn mux_fail(stream: &UsbMuxStream, fallback: &str) -> String {
    match stream.error_detail() {
        Some(d) if d.contains("mux version") => {
            format!("ERR\nVERSION\n{}", short_line(&d))
        }
        Some(d) => format!("ERR\nCONNECT\n{}", short_line(&d)),
        None => format!("ERR\nCONNECT\n{}", fallback),
    }
}

/// Map an error from lockdownd's Pair request. A dead stream
/// behind the error means the cable/transport went away;
/// otherwise the pairing-specific variants speak for
/// themselves.
fn pair_error(stream: &UsbMuxStream, e: &IdeviceError) -> String {
    match e {
        IdeviceError::UserDeniedPairing => {
            "ERR\nDENIED\n你已在 iPhone 上拒絕信任，配對取消".to_string()
        }
        IdeviceError::PasswordProtected => {
            "ERR\nPASSCODE\n裝置要求先解鎖並輸入密碼後再配對".to_string()
        }
        _ => {
            if let Some(d) = stream.error_detail() {
                format!("ERR\nUSBLOST\nUSB 傳輸中斷：{}", short_line(&d))
            } else {
                format!(
                    "ERR\nLOCKDOWN\n配對失敗：{}",
                    short_line(&format!("{}", e))
                )
            }
        }
    }
}

/// Map an error from a lockdownd call outside Pair.
fn lockdown_error(stream: &UsbMuxStream, what: &str, e: &IdeviceError) -> String {
    if let Some(d) = stream.error_detail() {
        format!("ERR\nUSBLOST\nUSB 傳輸中斷：{}", short_line(&d))
    } else {
        format!(
            "ERR\nLOCKDOWN\n{}失敗：{}",
            what,
            short_line(&format!("{}", e))
        )
    }
}

async fn do_usb_pair(files_dir: String, bridge: Arc<UsbBridge>) -> String {
    logf!("usbpair: start");
    let stream = UsbMuxStream::new(bridge, 1, LockdownClient::LOCKDOWND_PORT);
    let _guard = CloseGuard(stream.clone());
    let provider = UsbMuxProvider {
        stream: stream.clone(),
        label: "ipaloader".to_string(),
    };
    let mut lockdown = match tokio::time::timeout(
        CONNECT_TIMEOUT,
        LockdownClient::connect(&provider),
    )
    .await
    {
        Ok(Ok(c)) => c,
        Ok(Err(e)) => {
            logf!("usbpair: lockdown connect failed: {}", e);
            return mux_fail(&stream, &format!("連接 lockdownd 失敗：{}", e));
        }
        Err(_) => {
            logf!("usbpair: lockdown connect timed out");
            return mux_fail(&stream, "連接 lockdownd 逾時");
        }
    };
    logf!("usbpair: lockdown session up");

    let udid = match tokio::time::timeout(
        GETVALUE_TIMEOUT,
        lockdown.get_value(Some("UniqueDeviceID"), None),
    )
    .await
    {
        Ok(Ok(v)) => v
            .as_string()
            .map(|s| s.trim().to_string())
            .unwrap_or_default(),
        Ok(Err(e)) => {
            logf!("usbpair: GetValue(UniqueDeviceID) failed: {}", e);
            return lockdown_error(&stream, "讀取 UDID", &e);
        }
        Err(_) => {
            return "ERR\nUSBLOST\n讀取裝置資訊逾時，請確認連線後再試".to_string();
        }
    };
    if udid.len() < 20
        || !udid
            .chars()
            .all(|c| c.is_ascii_hexdigit() || c == '-')
    {
        logf!("usbpair: unexpected UDID value {:?}", udid);
        return "ERR\nLOCKDOWN\n讀到的 UDID 格式異常".to_string();
    }
    let name = match tokio::time::timeout(
        GETVALUE_TIMEOUT,
        lockdown.get_value(Some("DeviceName"), None),
    )
    .await
    {
        Ok(Ok(v)) => v
            .as_string()
            .map(|s| s.trim().to_string())
            .unwrap_or_default(),
        other => {
            logf!("usbpair: GetValue(DeviceName) not available: {:?}", other.is_ok());
            String::new()
        }
    };
    logf!(
        "usbpair: device {} name {:?}",
        mask_udid(&udid),
        name
    );
    if let Some(old) = load_pairing(&files_dir) {
        if !old.udid.eq_ignore_ascii_case(&udid) {
            logf!(
                "usbpair: stored pairing belongs to another device ({}), replacing it",
                mask_udid(&old.udid)
            );
        }
    }

    let host_id = uuid_v4();
    let system_buid = uuid_v4();
    logf!(
        "usbpair: sending Pair request (host {}); waiting for the Trust tap",
        &host_id[..8]
    );
    let pair_result = tokio::time::timeout(
        PAIR_TRUST_TIMEOUT,
        lockdown.pair(host_id, system_buid, Some("ipaloader")),
    )
    .await;
    let mut pairing = match pair_result {
        Ok(Ok(p)) => p,
        Ok(Err(e)) => {
            logf!("usbpair: Pair failed: {}", e);
            return pair_error(&stream, &e);
        }
        Err(_) => {
            logf!("usbpair: Pair timed out after {}s", PAIR_TRUST_TIMEOUT.as_secs());
            return "ERR\nTRUST_TIMEOUT\n等待在 iPhone 上點「信任」逾時，配對未完成，請解鎖裝置後再試"
                .to_string();
        }
    };
    logf!("usbpair: Pair accepted by the device");

    // Make the serialized record carry the UDID, exactly the
    // shape the import path validates and stores.
    if pairing.udid.is_none() {
        pairing.udid = Some(udid.clone());
    }
    let bytes = match pairing.serialize() {
        Ok(b) => b,
        Err(e) => {
            logf!("usbpair: serialize failed: {}", e);
            return format!(
                "ERR\nPAIRING\n配對檔序列化失敗：{}",
                short_line(&format!("{}", e))
            );
        }
    };
    if let Err(e) = store_pairing_files(&files_dir, &bytes, &udid, &name) {
        logf!("usbpair: store failed: {}", e);
        return format!("ERR\nPAIRING\n配對檔儲存失敗：{}", e);
    }
    logf!(
        "usbpair: pairing stored ({} bytes) for {}",
        bytes.len(),
        mask_udid(&udid)
    );
    format!(
        "OK\n{}\n{}",
        udid.replace(['\n', '\r'], " "),
        name.replace(['\n', '\r'], " ")
    )
}

/// `external fun nativeUsbPair(link: UsbMuxLink, filesDir: String): String`
/// in `com.yongshuai.ipaloader.SignActivity` (0.6.3). Blocking;
/// the panel calls it from a worker thread. Result protocol:
/// `OK\n<udid>\n<name>` / `ERR\n<CODE>\n<message>`.
#[no_mangle]
pub extern "system" fn Java_com_yongshuai_ipaloader_SignActivity_nativeUsbPair(
    mut env: JNIEnv,
    this: JObject,
    link: JObject,
    files_dir: JString,
) -> jstring {
    let files_dir = jstring_to_rust(&mut env, &files_dir);
    init_core(&files_dir);
    ensure_rpv_init(&mut env, &this);
    logf!("nativeUsbPair called");
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let vm = match env.get_java_vm() {
            Ok(v) => v,
            Err(e) => return format!("ERR\nPANIC\nJavaVM unavailable: {}", e),
        };
        let global = match env.new_global_ref(&link) {
            Ok(g) => g,
            Err(e) => return format!("ERR\nPANIC\nUSB link reference failed: {}", e),
        };
        let bridge = Arc::new(UsbBridge::new(vm, global));
        logf!("usbpair: link {}", bridge.note(&mut env));
        runtime().block_on(do_usb_pair(files_dir.clone(), bridge))
    }));
    let message = match result {
        Ok(s) => s,
        Err(e) => format!("ERR\nPANIC\nUSB 配對發生未預期錯誤：{}", panic_message(&e)),
    };
    logf!(
        "nativeUsbPair done: {}",
        message.lines().next().unwrap_or("")
    );
    make_jstring(&mut env, &message)
}
