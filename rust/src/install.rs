//! M4: device connection + app installation over Wi-Fi (0.6.0).
//!
//! JNI surface (on `com.yongshuai.ipaloader.InstallActivity`):
//! - `nativeProbeDevice(phoneIp, prefixLen, filesDir)` — locate the
//!   target iPhone on the LAN and report it. Success:
//!   `OK\n<ip>\n<device name>\n<iOS version>`; failures are
//!   `ERR\n<CODE>\n<message>` with CODE one of NOPAIRING / PAIRING /
//!   BADIP / NOTFOUND / LOCKDOWN.
//! - `nativeInstallIpa(ipaPath, phoneIp, prefixLen, filesDir)` —
//!   the same discovery, then install of an already-signed IPA via
//!   isideload's `sideload::install_app`. Progress reaches Kotlin
//!   through `InstallActivity.onInstallProgress(int)`; success is
//!   the same OK shape as the probe, failures add EXTRACT / DEVICE
//!   codes (DEVICE carries the device's own error text).
//! - `nativeGetPairing` / `nativeImportPairing` — the M3.1 pairing
//!   import, bound to this activity as well (same implementations
//!   as the sign page's copies in lib.rs).
//!
//! How the pieces fit together:
//! - The wire protocol is entirely the libraries': idevice's
//!   `TcpProvider` + `LockdownClient` (port 62078, session started
//!   with the imported pairing file), and isideload's install half
//!   (AFC upload to PublicStaging, then InstallationProxy). Nothing
//!   is re-implemented here.
//! - The libraries have no LAN discovery, so finding the phone is
//!   the one piece this module adds: scan the phone's own subnet
//!   for TCP 62078, then verify each responder by opening a
//!   lockdown session with our pairing file and comparing its
//!   UniqueDeviceID against the imported UDID. Only a UDID match
//!   counts as "the target device".
//! - `install_app` consumes an extracted .app directory, so the
//!   IPA is unpacked first (`Payload/<App>.app` subtree, unix
//!   permissions and symlinks restored — the inverse of the M3
//!   re-zip in lib.rs).

use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use idevice::lockdown::LockdownClient;
use idevice::pairing_file::PairingFile;
use idevice::provider::TcpProvider;
use idevice::IdeviceService as _;
use jni::objects::{GlobalRef, JObject, JString};
use jni::sys::{jint, jstring};
use jni::{JNIEnv, JavaVM};

use crate::{
    do_import_pairing, do_pairing_info, ensure_rpv_init, ensure_tmp_dir, init_core,
    jstring_to_rust, load_pairing, log_line, make_jstring, mask_udid, pairing_dir,
    panic_message, read_bundle_id, runtime, PairingInfo,
};

/// Wi-Fi lockdownd port (`LockdownClient::LOCKDOWND_PORT`).
const LOCKDOWND_PORT: u16 = 62078;

/// Marker for "a device answered, but it is not the target" so the
/// discovery loop can tell it apart from real session failures.
const UDID_MISMATCH: &str = "UDID_MISMATCH";

/// Char-boundary-safe first line of a message, capped at 220 bytes
/// (same trimming as the login classifier in lib.rs).
fn short_line(text: &str) -> String {
    let mut message = text.lines().next().unwrap_or("").trim().to_string();
    if message.len() > 220 {
        let mut end = 220;
        while end > 0 && !message.is_char_boundary(end) {
            end -= 1;
        }
        message.truncate(end);
    }
    message
}

/// UDID comparison form: dashes removed, lower case (old 40-hex
/// and new 8-16 formats both normalize to plain hex).
fn normalize_udid(udid: &str) -> String {
    udid.chars()
        .filter(|c| *c != '-')
        .collect::<String>()
        .to_lowercase()
}

/// The target device found on the LAN.
struct FoundDevice {
    ip: Ipv4Addr,
    name: String,
    version: String,
}

/// Load the imported pairing info plus the parsed pairing file.
/// Errors already follow the `ERR\n<CODE>\n<message>` protocol.
fn load_pairing_file(files_dir: &str) -> Result<(PairingInfo, PairingFile), String> {
    let Some(info) = load_pairing(files_dir) else {
        return Err(
            "ERR\nNOPAIRING\n尚未導入配對檔，請先導入與 iPhone 配對產生的配對檔".to_string(),
        );
    };
    let path = pairing_dir(files_dir).join("pairing.plist");
    let bytes = std::fs::read(&path)
        .map_err(|e| format!("ERR\nPAIRING\n配對檔讀取失敗：{}", e))?;
    let pairing = PairingFile::from_bytes(&bytes).map_err(|e| {
        format!(
            "ERR\nPAIRING\n配對檔內容無法解析（{}），請重新導入配對檔",
            short_line(&format!("{}", e))
        )
    })?;
    Ok((info, pairing))
}

/// Open a lockdown session to one candidate address and check it
/// is the target device. Returns the device info on a UDID match;
/// `UDID_MISMATCH` when the device answers but is a different one;
/// otherwise a human-readable failure line (no protocol wrapper).
async fn verify_candidate(
    ip: Ipv4Addr,
    pairing: &PairingFile,
    target_udid: &str,
) -> Result<FoundDevice, String> {
    let provider = TcpProvider {
        addr: IpAddr::V4(ip),
        scope_id: None,
        pairing_file: pairing.clone(),
        label: "ipaloader".to_string(),
    };
    let mut lockdown = LockdownClient::connect(&provider)
        .await
        .map_err(|e| format!("無法連上裝置的 lockdown：{}", short_line(&format!("{}", e))))?;
    lockdown
        .start_session(pairing)
        .await
        .map_err(|e| {
            format!(
                "lockdown 工作階段建立失敗：{}",
                short_line(&format!("{}", e))
            )
        })?;
    let udid = lockdown
        .get_value(Some("UniqueDeviceID"), None)
        .await
        .ok()
        .and_then(|v| v.as_string().map(|s| s.to_string()))
        .unwrap_or_default();
    if normalize_udid(&udid) != target_udid {
        logf!("device probe: {} answered but UDID differs, skipping", ip);
        return Err(UDID_MISMATCH.to_string());
    }
    let name = lockdown
        .get_value(Some("DeviceName"), None)
        .await
        .ok()
        .and_then(|v| v.as_string().map(|s| s.to_string()))
        .unwrap_or_default();
    let version = lockdown
        .get_value(Some("ProductVersion"), None)
        .await
        .ok()
        .and_then(|v| v.as_string().map(|s| s.to_string()))
        .unwrap_or_default();
    Ok(FoundDevice { ip, name, version })
}

/// Locate the target iPhone: probe the Android phone's own subnet
/// for lockdownd (TCP 62078), then UDID-verify each responder with
/// the imported pairing file. At most a /24 is scanned (home LANs
/// are /24; a wider subnet is narrowed to the /24 around the
/// phone). Every step is time-boxed so the UI never hangs.
async fn discover_device(
    phone_ip: &str,
    prefix_len: i32,
    files_dir: &str,
) -> Result<(FoundDevice, PairingFile), String> {
    let (info, pairing) = load_pairing_file(files_dir)?;
    let phone: Ipv4Addr = phone_ip.trim().parse().map_err(|_| {
        "ERR\nBADIP\n無法取得手機的 Wi-Fi 位址，請確認手機已連上 Wi-Fi".to_string()
    })?;
    let target = normalize_udid(&info.udid);

    let prefix = if prefix_len <= 0 || prefix_len > 32 {
        24
    } else {
        prefix_len
    };
    let scan_prefix = prefix.max(24);
    let mask: u32 = u32::MAX << (32 - scan_prefix);
    let phone_u = u32::from(phone);
    let network = phone_u & mask;
    let broadcast = network | !mask;
    let hosts: Vec<Ipv4Addr> = ((network + 1)..broadcast)
        .map(Ipv4Addr::from)
        .filter(|h| *h != phone)
        .collect();
    logf!(
        "device scan: {}/{} ({} hosts), target {}",
        Ipv4Addr::from(network),
        scan_prefix,
        hosts.len(),
        mask_udid(&info.udid)
    );

    // TCP probes in chunks of 64 concurrent connects, 350 ms each.
    let mut responders: Vec<Ipv4Addr> = Vec::new();
    for chunk in hosts.chunks(64) {
        let mut set = tokio::task::JoinSet::new();
        for host in chunk.iter().copied() {
            set.spawn(async move {
                let open = tokio::time::timeout(
                    Duration::from_millis(350),
                    tokio::net::TcpStream::connect((host, LOCKDOWND_PORT)),
                )
                .await
                .map(|r| r.is_ok())
                .unwrap_or(false);
                (host, open)
            });
        }
        while let Some(joined) = set.join_next().await {
            if let Ok((host, true)) = joined {
                responders.push(host);
            }
        }
    }
    logf!(
        "device scan: {} host(s) with port {} open",
        responders.len(),
        LOCKDOWND_PORT
    );

    let mut session_error: Option<String> = None;
    for ip in responders {
        match tokio::time::timeout(
            Duration::from_secs(8),
            verify_candidate(ip, &pairing, &target),
        )
        .await
        {
            Ok(Ok(found)) => {
                logf!(
                    "device found: {} at {} (iOS {})",
                    found.name, found.ip, found.version
                );
                return Ok((found, pairing));
            }
            Ok(Err(e)) => {
                if e != UDID_MISMATCH && session_error.is_none() {
                    session_error = Some(e);
                }
            }
            Err(_) => {
                logf!("device probe: {} timed out during lockdown verify", ip);
            }
        }
    }
    if let Some(e) = session_error {
        return Err(format!("ERR\nLOCKDOWN\n{}", e));
    }
    Err("ERR\nNOTFOUND\n在區網內找不到目標 iPhone：請確認 iPhone 與手機連同一個 Wi-Fi、iPhone 已喚醒並解鎖、且已開啟「透過 Wi-Fi 同步」".to_string())
}

async fn do_probe(phone_ip: String, prefix_len: i32, files_dir: String) -> String {
    match discover_device(&phone_ip, prefix_len, &files_dir).await {
        Ok((found, _)) => format!(
            "OK\n{}\n{}\n{}",
            found.ip,
            found.name.replace('\n', " "),
            found.version.replace('\n', " ")
        ),
        Err(e) => e,
    }
}

/// Progress bridge from isideload's install callback (a plain
/// `Fn(u64)`, percent 0–100) to `InstallActivity.onInstallProgress`.
/// Forwards only on change, and logs every 10% step.
struct InstallProgressBridge {
    vm: JavaVM,
    callback: GlobalRef,
    last_sent: AtomicI32,
    last_logged: AtomicI32,
}

impl InstallProgressBridge {
    fn percent(&self, value: u64) {
        let p = value.min(100) as i32;
        let step = p / 10;
        if step > self.last_logged.swap(step, Ordering::Relaxed) {
            logf!("install progress: {}%", p);
        }
        if p == self.last_sent.swap(p, Ordering::Relaxed) {
            return;
        }
        let mut env = match self.vm.attach_current_thread() {
            Ok(e) => e,
            Err(e) => {
                logf!("install progress: attach failed: {}", e);
                return;
            }
        };
        let result = env.call_method(
            self.callback.as_obj(),
            "onInstallProgress",
            "(I)V",
            &[jni::objects::JValue::Int(p)],
        );
        if env.exception_check().unwrap_or(false) {
            let _ = env.exception_clear();
            logf!("install progress: Kotlin callback threw (ignored)");
            return;
        }
        if let Err(e) = result {
            logf!("install progress: call failed: {}", e);
        }
    }
}

/// Extract the `Payload/<App>.app` subtree of an IPA into
/// `work_root`, restoring unix permissions and symlinks (the
/// inverse of the M3 re-zip). Returns the .app directory.
fn extract_app_bundle(ipa_path: &str, work_root: &std::path::Path) -> Result<PathBuf, String> {
    use std::io::Read as _;
    use std::os::unix::fs::PermissionsExt as _;

    let file = std::fs::File::open(ipa_path)
        .map_err(|e| format!("無法開啟 IPA：{}", e))?;
    let mut archive =
        zip::ZipArchive::new(file).map_err(|e| format!("IPA 讀取失敗：{}", e))?;

    // First pass: the .app directory name under Payload/.
    let mut app_name: Option<String> = None;
    for i in 0..archive.len() {
        let entry = archive
            .by_index(i)
            .map_err(|e| format!("IPA 讀取失敗：{}", e))?;
        let parts: Vec<&str> = entry.name().split('/').collect();
        if parts.len() >= 2 && parts[0] == "Payload" && parts[1].ends_with(".app") {
            app_name = Some(parts[1].to_string());
            break;
        }
    }
    let app_name =
        app_name.ok_or_else(|| "IPA 內找不到 Payload/*.app，這不是有效的 IPA".to_string())?;
    let prefix = format!("Payload/{}/", app_name);
    let app_dir = work_root.join(&app_name);
    std::fs::create_dir_all(&app_dir)
        .map_err(|e| format!("建立解包目錄失敗：{}", e))?;

    // Second pass: extract the subtree.
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| format!("IPA 讀取失敗：{}", e))?;
        let name = entry.name().to_string();
        if !name.starts_with(&prefix) {
            continue;
        }
        let rel = &name[prefix.len()..];
        if rel.is_empty() {
            continue;
        }
        // Rebuild the relative path component by component,
        // refusing anything that could escape the app directory.
        let mut dest = app_dir.clone();
        for part in rel.split('/') {
            if part.is_empty() || part == "." {
                continue;
            }
            if part == ".." {
                return Err("IPA 內含不合法的路徑".to_string());
            }
            dest.push(part);
        }
        let mode = entry.unix_mode().unwrap_or(0);
        let file_type = mode & 0o170000;
        if name.ends_with('/') || file_type == 0o040000 {
            std::fs::create_dir_all(&dest)
                .map_err(|e| format!("解包失敗：{}", e))?;
        } else if file_type == 0o120000 {
            let mut target = String::new();
            entry
                .read_to_string(&mut target)
                .map_err(|e| format!("解包失敗：{}", e))?;
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("解包失敗：{}", e))?;
            }
            let _ = std::fs::remove_file(&dest);
            std::os::unix::fs::symlink(&target, &dest)
                .map_err(|e| format!("解包失敗：{}", e))?;
        } else {
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("解包失敗：{}", e))?;
            }
            let mut out = std::fs::File::create(&dest)
                .map_err(|e| format!("解包失敗：{}", e))?;
            std::io::copy(&mut entry, &mut out)
                .map_err(|e| format!("解包失敗：{}", e))?;
            let perm = if mode & 0o7777 != 0 {
                mode & 0o7777
            } else {
                0o644
            };
            let _ = std::fs::set_permissions(
                &dest,
                std::fs::Permissions::from_mode(perm),
            );
        }
    }

    // The main executable must be executable on the device even
    // if the IPA lost its mode bits somewhere along the way.
    let info_path = app_dir.join("Info.plist");
    if let Ok(bytes) = std::fs::read(&info_path) {
        if let Ok(plist::Value::Dictionary(dict)) =
            plist::Value::from_reader(std::io::Cursor::new(bytes))
        {
            if let Some(exe) = dict
                .get("CFBundleExecutable")
                .and_then(|v| v.as_string())
            {
                let exe_path = app_dir.join(exe);
                let _ = std::fs::set_permissions(
                    &exe_path,
                    std::fs::Permissions::from_mode(0o755),
                );
            }
        }
    }
    Ok(app_dir)
}

async fn do_install(
    files_dir: String,
    ipa_path: String,
    phone_ip: String,
    prefix_len: i32,
    bridge: Arc<InstallProgressBridge>,
) -> String {
    ensure_tmp_dir(&files_dir);
    logf!("install start: {}", ipa_path);
    // Connection first: if the device cannot be found and verified,
    // the install never starts (NOTFOUND / LOCKDOWN come back here).
    let (found, pairing) = match discover_device(&phone_ip, prefix_len, &files_dir).await {
        Ok(v) => v,
        Err(e) => {
            logf!("install: device discovery failed");
            return e;
        }
    };

    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let work_root = PathBuf::from(&files_dir)
        .join("install_tmp")
        .join(format!("job-{}-{}", std::process::id(), stamp));
    let app_dir = match extract_app_bundle(&ipa_path, &work_root) {
        Ok(d) => d,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&work_root);
            logf!("install: extract failed: {}", e);
            return format!("ERR\nEXTRACT\n{}", e.replace('\n', " "));
        }
    };
    let bundle_id = read_bundle_id(&app_dir);
    logf!(
        "install: extracted bundle {} for device at {}",
        bundle_id, found.ip
    );

    let provider = TcpProvider {
        addr: IpAddr::V4(found.ip),
        scope_id: None,
        pairing_file: pairing,
        label: "ipaloader".to_string(),
    };
    let progress_bridge = bridge.clone();
    let progress = move |p: u64| progress_bridge.percent(p);
    let result = tokio::time::timeout(
        Duration::from_secs(900),
        isideload::sideload::install::install_app(&provider, &app_dir, progress),
    )
    .await;
    let _ = std::fs::remove_dir_all(&work_root);
    match result {
        Ok(Ok(())) => {
            bridge.percent(100);
            logf!("install ok: {} installed on {}", bundle_id, found.ip);
            format!(
                "OK\n{}\n{}\n{}",
                found.ip,
                found.name.replace('\n', " "),
                found.version.replace('\n', " ")
            )
        }
        Ok(Err(report)) => {
            logf!("install failed: {:?}", report);
            let msg = short_line(&format!("{}", report));
            format!(
                "ERR\nDEVICE\n{}",
                if msg.is_empty() {
                    "裝置回報安裝失敗".to_string()
                } else {
                    msg
                }
            )
        }
        Err(_) => {
            logf!("install timed out");
            "ERR\nDEVICE\n安裝逾時（超過 15 分鐘），請確認 Wi-Fi 連線穩定後重試".to_string()
        }
    }
}

// ---------------------------------------------------------------------------
// JNI entry points (com.yongshuai.ipaloader.InstallActivity)
// ---------------------------------------------------------------------------

/// `external fun nativeProbeDevice(phoneIp, prefixLen, filesDir)`
/// — locate and verify the target iPhone on the LAN.
#[no_mangle]
pub extern "system" fn Java_com_yongshuai_ipaloader_InstallActivity_nativeProbeDevice(
    mut env: JNIEnv,
    this: JObject,
    phone_ip: JString,
    prefix_len: jint,
    files_dir: JString,
) -> jstring {
    let phone_ip = jstring_to_rust(&mut env, &phone_ip);
    let files_dir = jstring_to_rust(&mut env, &files_dir);
    init_core(&files_dir);
    ensure_rpv_init(&mut env, &this);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        runtime().block_on(do_probe(phone_ip, prefix_len as i32, files_dir))
    }));
    let text = match result {
        Ok(t) => t,
        Err(payload) => {
            let msg = panic_message(&*payload);
            logf!("nativeProbeDevice PANIC caught: {}", msg);
            format!("ERR\nPANIC\n{}", msg.replace('\n', " "))
        }
    };
    make_jstring(&mut env, &text)
}

/// `external fun nativeInstallIpa(ipaPath, phoneIp, prefixLen,
/// filesDir)` — discover the device, then install an
/// already-signed IPA. Blocking: run on a background thread.
/// `this` must implement `onInstallProgress(int)`.
#[no_mangle]
pub extern "system" fn Java_com_yongshuai_ipaloader_InstallActivity_nativeInstallIpa(
    mut env: JNIEnv,
    this: JObject,
    ipa_path: JString,
    phone_ip: JString,
    prefix_len: jint,
    files_dir: JString,
) -> jstring {
    let ipa_path = jstring_to_rust(&mut env, &ipa_path);
    let phone_ip = jstring_to_rust(&mut env, &phone_ip);
    let files_dir = jstring_to_rust(&mut env, &files_dir);
    init_core(&files_dir);
    ensure_rpv_init(&mut env, &this);
    logf!("nativeInstallIpa called");
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let vm = match env.get_java_vm() {
            Ok(vm) => vm,
            Err(e) => return format!("ERR\nUNKNOWN\nJNI VM: {}", e),
        };
        let callback = match env.new_global_ref(&this) {
            Ok(r) => r,
            Err(e) => return format!("ERR\nUNKNOWN\nJNI ref: {}", e),
        };
        let bridge = Arc::new(InstallProgressBridge {
            vm,
            callback,
            last_sent: AtomicI32::new(-1),
            last_logged: AtomicI32::new(-1),
        });
        runtime().block_on(do_install(
            files_dir,
            ipa_path,
            phone_ip,
            prefix_len as i32,
            bridge,
        ))
    }));
    let text = match result {
        Ok(t) => t,
        Err(payload) => {
            let msg = panic_message(&*payload);
            logf!("nativeInstallIpa PANIC caught: {}", msg);
            format!("ERR\nPANIC\n{}", msg.replace('\n', " "))
        }
    };
    make_jstring(&mut env, &text)
}

/// `external fun nativeGetPairing(filesDir)` — same implementation
/// as the sign page's copy (M3.1), bound to this activity.
#[no_mangle]
pub extern "system" fn Java_com_yongshuai_ipaloader_InstallActivity_nativeGetPairing(
    mut env: JNIEnv,
    this: JObject,
    files_dir: JString,
) -> jstring {
    let files_dir = jstring_to_rust(&mut env, &files_dir);
    init_core(&files_dir);
    ensure_rpv_init(&mut env, &this);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        do_pairing_info(&files_dir)
    }));
    let text = match result {
        Ok(t) => t,
        Err(payload) => {
            let msg = panic_message(&*payload);
            logf!("nativeGetPairing PANIC caught: {}", msg);
            format!("ERR\nPANIC\n{}", msg.replace('\n', " "))
        }
    };
    make_jstring(&mut env, &text)
}

/// `external fun nativeImportPairing(srcPath, filesDir)` — same
/// implementation as the sign page's copy (M3.1), bound here.
#[no_mangle]
pub extern "system" fn Java_com_yongshuai_ipaloader_InstallActivity_nativeImportPairing(
    mut env: JNIEnv,
    this: JObject,
    src_path: JString,
    files_dir: JString,
) -> jstring {
    let src_path = jstring_to_rust(&mut env, &src_path);
    let files_dir = jstring_to_rust(&mut env, &files_dir);
    init_core(&files_dir);
    ensure_rpv_init(&mut env, &this);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        do_import_pairing(&src_path, &files_dir)
    }));
    let text = match result {
        Ok(t) => t,
        Err(payload) => {
            let msg = panic_message(&*payload);
            logf!("nativeImportPairing PANIC caught: {}", msg);
            format!("ERR\nPANIC\n{}", msg.replace('\n', " "))
        }
    };
    make_jstring(&mut env, &text)
}
