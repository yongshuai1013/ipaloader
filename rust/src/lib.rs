//! ipaloader Rust core (M2: real Apple ID login).
//!
//! JNI surface (all on `com.yongshuai.ipaloader.MainActivity`):
//! - `nativeVersion()` — M1 placeholder, proves the channel works.
//! - `nativeLogin(email, password, filesDir)` — GrandSlam SRP login with
//!   2FA bridged back to Kotlin through `MainActivity.onTwoFactorNeeded`.
//!   On success the account session (the GrandSlam `spd` plist) is saved
//!   through isideload's fs-storage into `<filesDir>/core/`.
//! - `nativeRestoreSession(filesDir)` — reloads the saved session,
//!   validates it with a real app-token exchange + team listing.
//! - `nativeLogout(filesDir)` — deletes the saved session.
//!
//! Result protocol for login/restore (newline separated, max 3 fields):
//! `OK\n<email>\n<team name or empty>` or `ERR\n<CODE>\n<short message>`,
//! plus the literal `NONE` from restore when no usable session exists.
//! A caught panic is reported as `ERR\nPANIC\n<message>`.
//!
//! Diagnostics go to an append-only log at `<filesDir>/core/ipaloader.log`
//! (login milestones, full error chains, panics). The password, 2FA codes
//! and session/token contents are never logged.

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Once, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use isideload::anisette::remote_v3::RemoteV3AnisetteProvider;
use isideload::anisette::AnisetteClientInfo;
use isideload::auth::apple_account::{
    AppleAccount, TwoFactorCallbackParams, TwoFactorCallbackResponse,
};
use isideload::auth::builder::AppleAccountBuilder;
use isideload::dev::developer_session::{DeveloperSession, TeamsApi};
use isideload::util::fs_storage::FsStorage;
use isideload::util::storage::SideloadingStorage;
use jni::objects::{GlobalRef, JObject, JString, JValue};
use jni::sys::{jboolean, jstring};
use jni::{JNIEnv, JavaVM};
use rootcause::prelude::*;
use rootcause::Report;

/// Remote anisette (v3) server used for M2. StikStore runs a Sierra Mac
/// ADI, so the client claim it reports is a Mac claim — the shape that
/// passes Apple's current GrandSlam edge rules.
const ANISETTE_URL: &str = "https://ani2.stikstore.app";

const SESSION_EMAIL_KEY: &str = "session_email";
const SESSION_SPD_KEY: &str = "session_spd";

fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("failed to build tokio runtime")
    })
}

/// Path of the shared diagnostic log, pinned by the first native call
/// that knows the app files dir. The Kotlin side appends to the same
/// file through its own helper (see AppLog.kt).
static LOG_PATH: OnceLock<PathBuf> = OnceLock::new();

fn log_path_for(files_dir: &str) -> PathBuf {
    PathBuf::from(files_dir).join("core").join("ipaloader.log")
}

/// Current time in UTC+8 (the user's timezone), formatted
/// `YYYY-MM-DD HH:MM:SS`, without pulling in a date crate.
fn timestamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
        + 8 * 3600;
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    // Civil-from-days conversion (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    if month <= 2 {
        year += 1;
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        year,
        month,
        day,
        tod / 3600,
        (tod % 3600) / 60,
        tod % 60
    )
}

/// Append one timestamped line to the diagnostic log. Best-effort: a
/// logging failure must never take the app down.
fn log_line(msg: &str) {
    static LOG_LOCK: Mutex<()> = Mutex::new(());
    let Some(path) = LOG_PATH.get() else {
        return;
    };
    let _guard = LOG_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let flat = msg.replace('\n', " ⏎ ");
        let _ = writeln!(f, "[{}] [core] {}", timestamp(), flat);
    }
}

macro_rules! logf {
    ($($arg:tt)*) => {
        log_line(&format!($($arg)*))
    };
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        s.to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_string()
    }
}

/// One-time rustls-platform-verifier Android initialization. The
/// verifier talks to the JVM through the JavaVM + application Context
/// stored by `android::init_with_env`; without it, the first TLS
/// handshake that relies on the platform verifier (the anisette
/// fetch, the developer-portal calls) panics with "Expect
/// rustls-platform-verifier to be initialized". Attempted from every
/// JNI entry point — they all receive the Activity — until it has
/// succeeded once.
static RPV_INIT_DONE: AtomicBool = AtomicBool::new(false);

/// Init result waiting to be written to the log. `nativeVersion`
/// runs at app startup before any entry point that knows the files
/// dir (and therefore the log path); `init_core` flushes this.
static RPV_INIT_MSG: Mutex<Option<String>> = Mutex::new(None);

fn ensure_rpv_init(env: &mut JNIEnv, this: &JObject) {
    if RPV_INIT_DONE.load(Ordering::Relaxed) {
        return;
    }
    let msg = match try_rpv_init(env, this) {
        Ok(()) => {
            RPV_INIT_DONE.store(true, Ordering::Relaxed);
            "platform verifier init ok".to_string()
        }
        Err(e) => format!("platform verifier init FAILED: {}", e),
    };
    if LOG_PATH.get().is_some() {
        log_line(&msg);
    } else if let Ok(mut slot) = RPV_INIT_MSG.lock() {
        *slot = Some(msg);
    }
}

fn try_rpv_init(env: &mut JNIEnv, this: &JObject) -> Result<(), String> {
    // The application Context, not the Activity itself: the verifier
    // keeps a process-lifetime global reference to what it is given.
    let ctx = env
        .call_method(
            this,
            "getApplicationContext",
            "()Landroid/content/Context;",
            &[],
        )
        .map_err(|e| format!("getApplicationContext: {}", e))?;
    let ctx_obj = ctx
        .l()
        .map_err(|e| format!("getApplicationContext result: {}", e))?;
    let raw_ctx = ctx_obj.into_raw();
    // Bridge to the jni 0.22 types rustls-platform-verifier 0.7 is
    // typed against; both jni copies wrap the same C pointers, and
    // this native call's stack frame owns them for the duration.
    let mut inner: Option<Result<(), String>> = None;
    let mut unowned =
        unsafe { jni22::EnvUnowned::from_raw(env.get_raw() as *mut jni22::sys::JNIEnv) };
    let _outcome = unowned.with_env(|env22| {
        let ctx22 =
            unsafe { jni22::objects::JObject::from_raw(env22, raw_ctx as jni22::sys::jobject) };
        inner = Some(
            rustls_platform_verifier::android::init_with_env(env22, ctx22)
                .map_err(|e| format!("{}", e)),
        );
        Ok::<(), jni22::errors::Error>(())
    });
    match inner {
        Some(r) => r,
        None => Err("jni bridge produced no result".to_string()),
    }
}

fn init_core(files_dir: &str) {
    let _ = LOG_PATH.set(log_path_for(files_dir));
    // Flush a platform-verifier init result that happened before the
    // log path was known (see ensure_rpv_init).
    if let Ok(mut slot) = RPV_INIT_MSG.lock() {
        if let Some(msg) = slot.take() {
            log_line(&msg);
        }
    }
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // Record panics in the log even when they happen on a tokio
        // worker thread and never reach a catch_unwind site.
        std::panic::set_hook(Box::new(|info| {
            let msg = if let Some(s) = info.payload().downcast_ref::<&str>() {
                s.to_string()
            } else if let Some(s) = info.payload().downcast_ref::<String>() {
                s.clone()
            } else {
                "unknown panic".to_string()
            };
            let loc = info
                .location()
                .map(|l| format!(" at {}:{}", l.file(), l.line()))
                .unwrap_or_default();
            log_line(&format!("PANIC: {}{}", msg, loc));
        }));
        // isideload 0.5.2 pins reqwest's rustls-no-provider flavour:
        // the host must install a rustls CryptoProvider process-wide
        // before the first HTTP client is built, otherwise client
        // construction panics ("No provider set"). Do it here, ahead
        // of every code path that builds a client. A duplicate install
        // (already-present provider) errors and is ignored.
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        log_line("core init: rustls CryptoProvider (aws-lc-rs) installed");
        let _ = isideload::init();
    });
}

/// Directory (inside the app-private files dir) that backs fs-storage for
/// both the anisette provider state and the saved account session.
fn core_dir(files_dir: &str) -> PathBuf {
    PathBuf::from(files_dir).join("core")
}

fn session_storage(files_dir: &str) -> FsStorage {
    FsStorage::new(core_dir(files_dir))
}

fn anisette_provider(files_dir: &str) -> Result<RemoteV3AnisetteProvider, Report> {
    let provider = RemoteV3AnisetteProvider::new(
        ANISETTE_URL,
        Box::new(session_storage(files_dir)),
        "0".to_string(),
    )?;
    // Same client-info override iloader ships: a modern Mac akd claim for
    // every GrandSlam request (SRP, trusted-device 2FA and the SMS
    // endpoints all share it), independent of what the anisette server
    // happens to report. The anisette data itself still comes from the
    // server's own (Mac) ADI.
    Ok(provider.set_client_info_override(Some(AnisetteClientInfo {
        client_info: "<Mac15,7> <macOS;27.0;26A5378j> <com.apple.AuthKit/1 (com.apple.akd/1.0)>"
            .to_string(),
        user_agent: "akd/1.0 CFNetwork/811.5.4".to_string(),
    })))
}

fn save_session(files_dir: &str, account: &AppleAccount) -> Result<(), Report> {
    let spd = account
        .spd
        .as_ref()
        .ok_or_else(|| report!("login finished without spd, cannot save session"))?;
    let mut buf = Vec::new();
    plist::to_writer_binary(&mut buf, &plist::Value::Dictionary(spd.clone()))
        .map_err(|e| report!("failed to serialize session: {}", e))?;
    let storage = session_storage(files_dir);
    storage.store_data(SESSION_SPD_KEY, &buf)?;
    storage.store(SESSION_EMAIL_KEY, &account.email)?;
    Ok(())
}

fn load_session(files_dir: &str) -> Option<(String, plist::Dictionary)> {
    let storage = session_storage(files_dir);
    let email = storage.retrieve(SESSION_EMAIL_KEY).ok()??;
    if email.is_empty() {
        return None;
    }
    let bytes = storage.retrieve_data(SESSION_SPD_KEY).ok()??;
    if bytes.is_empty() {
        return None;
    }
    let value = plist::Value::from_reader(std::io::Cursor::new(bytes)).ok()?;
    match value {
        plist::Value::Dictionary(dict) => Some((email, dict)),
        _ => None,
    }
}

fn clear_session(files_dir: &str) {
    let dir = core_dir(files_dir);
    let _ = std::fs::remove_file(dir.join(SESSION_SPD_KEY));
    let _ = std::fs::remove_file(dir.join(SESSION_EMAIL_KEY));
}

/// Map an isideload failure onto a short stable code for the UI plus a
/// one-line message. Matching is done on the full cause chain; the
/// message never contains the password or a 2FA code (isideload does not
/// attach either to its errors).
fn classify(report: &Report) -> (&'static str, String) {
    let chain = format!("{:?}", report).to_lowercase();
    let first_line = format!("{}", report);
    let mut message = first_line
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .to_string();
    if message.len() > 220 {
        // Char-boundary safe truncation: a plain truncate(220) panics
        // when byte 220 falls inside a multi-byte character.
        let mut end = 220;
        while end > 0 && !message.is_char_boundary(end) {
            end -= 1;
        }
        message.truncate(end);
    }
    let code = if chain.contains("failed to build http client") {
        // Client-construction failures (TLS/verifier setup in the
        // host) are not connectivity problems. Keep them out of the
        // NETWORK bucket so the UI shows the raw message instead of
        // a misleading "network error" (0.2.5's GrandSlam client
        // build failure was misreported exactly that way).
        "UNKNOWN"
    } else if chain.contains("no 2fa code provided")
        || chain.contains("no 2fa method selected")
    {
        "CANCELLED"
    } else if chain.contains("-20101")
        || chain.contains("invalid username or password")
        || chain.contains("username or password is incorrect")
        || chain.contains("invalid credentials")
    {
        "BAD_CREDENTIALS"
    } else if chain.contains("anisette") {
        "ANISETTE"
    } else if chain.contains("connect")
        || chain.contains("dns")
        || chain.contains("timed out")
        || chain.contains("timeout")
        || chain.contains("certificate")
        || chain.contains("tls")
        || chain.contains("network")
    {
        "NETWORK"
    } else {
        "UNKNOWN"
    };
    (code, message)
}

fn err_string(report: &Report) -> String {
    let (code, message) = classify(report);
    format!("ERR\n{}\n{}", code, message.replace('\n', " "))
}

/// Bridge from isideload's async two-factor callback to the blocking
/// Kotlin dialog (`MainActivity.onTwoFactorNeeded`). The JNI call runs
/// inside `tokio::task::block_in_place` on a runtime worker thread.
struct TwoFactorBridge {
    vm: JavaVM,
    callback: GlobalRef,
}

impl TwoFactorBridge {
    fn request(&self, params: &TwoFactorCallbackParams) -> Result<TwoFactorCallbackResponse, Report> {
        let kind = if params.unknown {
            "unknown"
        } else if params.sms {
            "sms"
        } else {
            "device"
        };
        let numbers = params
            .numbers
            .iter()
            .map(|n| format!("{}:{}", n.id, n.last_two_digits))
            .collect::<Vec<_>>()
            .join(",");
        let last_error = params.last_error.clone().unwrap_or_default();
        logf!(
            "2FA requested: kind={} numbers={} (last error present: {})",
            kind,
            numbers,
            !last_error.is_empty()
        );

        let mut env = self
            .vm
            .attach_current_thread()
            .map_err(|e| report!("failed to attach JNI thread: {}", e))?;
        let j_kind = env.new_string(kind)?;
        let j_numbers = env.new_string(numbers)?;
        let j_error = env.new_string(last_error)?;
        let result = env.call_method(
            self.callback.as_obj(),
            "onTwoFactorNeeded",
            "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;",
            &[
                JValue::Object(&j_kind),
                JValue::Object(&j_numbers),
                JValue::Object(&j_error),
            ],
        );
        if env.exception_check().unwrap_or(false) {
            let _ = env.exception_clear();
            logf!("2FA bridge: Kotlin threw an exception, aborting");
            return Ok(TwoFactorCallbackResponse::Abort);
        }
        let result = result?;
        let j_str: JString = result.l()?.into();
        let reply: String = String::from(env.get_string(&j_str)?);
        let response = parse_two_factor_reply(&reply);
        // Log only the chosen action, never the submitted code itself.
        match &response {
            TwoFactorCallbackResponse::SubmitCode(_) => logf!("2FA reply: code submitted"),
            TwoFactorCallbackResponse::SendSms(id) => logf!("2FA reply: send SMS (number id {})", id),
            TwoFactorCallbackResponse::SendToDevices => logf!("2FA reply: send to trusted devices"),
            TwoFactorCallbackResponse::ResendCode => logf!("2FA reply: resend code"),
            TwoFactorCallbackResponse::Abort => logf!("2FA reply: aborted by user"),
        }
        Ok(response)
    }
}

fn parse_two_factor_reply(reply: &str) -> TwoFactorCallbackResponse {
    let reply = reply.trim();
    if let Some(code) = reply.strip_prefix("code:") {
        let code = code.trim().to_string();
        if code.is_empty() {
            return TwoFactorCallbackResponse::Abort;
        }
        return TwoFactorCallbackResponse::SubmitCode(code);
    }
    if let Some(id) = reply.strip_prefix("sms:") {
        if let Ok(id) = id.trim().parse::<u32>() {
            return TwoFactorCallbackResponse::SendSms(id);
        }
        return TwoFactorCallbackResponse::Abort;
    }
    match reply {
        "resend" => TwoFactorCallbackResponse::ResendCode,
        "devices" => TwoFactorCallbackResponse::SendToDevices,
        _ => TwoFactorCallbackResponse::Abort,
    }
}

async fn fetch_team_name(account: &mut AppleAccount) -> String {
    let mut session = match DeveloperSession::from_account(account).await {
        Ok(s) => s,
        Err(e) => {
            let (_, m) = classify(&e);
            logf!("team: developer session failed: {}", m);
            return String::new();
        }
    };
    match session.list_teams().await {
        Ok(teams) => {
            let name = teams
                .into_iter()
                .next()
                .and_then(|t| t.name)
                .unwrap_or_default();
            logf!("team: list ok (name present: {})", !name.is_empty());
            name
        }
        Err(e) => {
            let (_, m) = classify(&e);
            logf!("team: list failed: {}", m);
            String::new()
        }
    }
}

async fn do_login(
    email: String,
    password: String,
    files_dir: String,
    bridge: Arc<TwoFactorBridge>,
) -> Result<String, Report> {
    let callback = move |params: TwoFactorCallbackParams| {
        let bridge = bridge.clone();
        async move { tokio::task::block_in_place(|| bridge.request(&params)) }
    };
    logf!("login start: {}", email);
    let provider = match anisette_provider(&files_dir) {
        Ok(p) => {
            logf!("anisette provider ready (remote v3)");
            p
        }
        Err(e) => {
            logf!("anisette provider failed: {:?}", e);
            return Err(e);
        }
    };
    logf!("login: SRP login begin");
    let mut account = match AppleAccountBuilder::new(&email)
        .anisette_provider(provider)
        .login(&password, callback)
        .await
    {
        Ok(a) => a,
        Err(e) => {
            logf!("login failed: {:?}", e);
            return Err(e);
        }
    };
    logf!("login: SRP/2FA complete, saving session");
    if let Err(e) = save_session(&files_dir, &account) {
        logf!("session save failed: {:?}", e);
        return Err(e);
    }
    logf!("session saved");
    let team = fetch_team_name(&mut account).await;
    logf!("login ok");
    Ok(format!("OK\n{}\n{}", account.email, team.replace('\n', " ")))
}

async fn do_restore(files_dir: String) -> String {
    logf!("restore start");
    let Some((email, spd)) = load_session(&files_dir) else {
        logf!("restore: no saved session");
        return "NONE".to_string();
    };
    logf!("restore: saved session found for {}", email);
    let mut account = match AppleAccountBuilder::new(&email)
        .anisette_provider(match anisette_provider(&files_dir) {
            Ok(p) => p,
            Err(e) => {
                logf!("restore: anisette provider failed: {:?}", e);
                return err_string(&e);
            }
        })
        .build()
        .await
    {
        Ok(a) => a,
        Err(e) => {
            logf!("restore: account build failed: {:?}", e);
            return err_string(&e);
        }
    };
    account.spd = Some(spd);
    // Validate the saved session with a real token exchange + team list;
    // a session that Apple no longer accepts is dropped.
    let mut session = match DeveloperSession::from_account(&mut account).await {
        Ok(s) => s,
        Err(e) => {
            logf!("restore: developer session failed: {:?}", e);
            let chain = format!("{:?}", e).to_lowercase();
            if chain.contains("status code") || chain.contains("401") || chain.contains("403") {
                clear_session(&files_dir);
                logf!("restore: session rejected by Apple, cleared");
                return "NONE".to_string();
            }
            return err_string(&e);
        }
    };
    match session.list_teams().await {
        Ok(teams) => {
            let team = teams
                .into_iter()
                .next()
                .and_then(|t| t.name)
                .unwrap_or_default();
            logf!("restore ok");
            format!("OK\n{}\n{}", email, team.replace('\n', " "))
        }
        Err(e) => {
            logf!("restore: list teams failed: {:?}", e);
            err_string(&e)
        }
    }
}

fn jstring_to_rust(env: &mut JNIEnv, s: &JString) -> String {
    match env.get_string(s) {
        Ok(v) => String::from(v),
        Err(_) => String::new(),
    }
}

fn make_jstring(env: &mut JNIEnv, text: &str) -> jstring {
    env.new_string(text)
        .expect("failed to create JNI string")
        .into_raw()
}

/// `external fun nativeVersion(): String` in `com.yongshuai.ipaloader.MainActivity`.
#[no_mangle]
pub extern "system" fn Java_com_yongshuai_ipaloader_MainActivity_nativeVersion(
    mut env: JNIEnv,
    this: JObject,
) -> jstring {
    // Earliest entry point at app startup: initialize the platform
    // verifier here (result is stashed and flushed to the log by the
    // first files-dir entry point).
    ensure_rpv_init(&mut env, &this);
    let _addr = idevice::usbmuxd::UsbmuxdAddr::from_env_var();
    make_jstring(&mut env, "rust core ok: isideload 0.5.2 / idevice 0.1.68")
}

/// `external fun nativeLogin(email, password, filesDir): String`.
/// Blocking: run on a background thread. `this` must implement
/// `onTwoFactorNeeded(String, String, String): String`.
#[no_mangle]
pub extern "system" fn Java_com_yongshuai_ipaloader_MainActivity_nativeLogin(
    mut env: JNIEnv,
    this: JObject,
    email: JString,
    password: JString,
    files_dir: JString,
) -> jstring {
    let email = jstring_to_rust(&mut env, &email);
    let password = jstring_to_rust(&mut env, &password);
    let files_dir = jstring_to_rust(&mut env, &files_dir);
    init_core(&files_dir);
    ensure_rpv_init(&mut env, &this);
    logf!("nativeLogin called");
    // panic = "unwind" + catch_unwind: a panic anywhere in the login
    // path becomes a logged ERR/PANIC result instead of a crash.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let vm = match env.get_java_vm() {
            Ok(vm) => vm,
            Err(e) => return format!("ERR\nUNKNOWN\nJNI VM: {}", e),
        };
        let callback = match env.new_global_ref(&this) {
            Ok(r) => r,
            Err(e) => return format!("ERR\nUNKNOWN\nJNI ref: {}", e),
        };
        let bridge = Arc::new(TwoFactorBridge { vm, callback });
        match runtime().block_on(do_login(email, password, files_dir, bridge)) {
            Ok(ok) => ok,
            Err(e) => err_string(&e),
        }
    }));
    let text = match result {
        Ok(t) => t,
        Err(payload) => {
            let msg = panic_message(&*payload);
            logf!("nativeLogin PANIC caught: {}", msg);
            format!("ERR\nPANIC\n{}", msg.replace('\n', " "))
        }
    };
    make_jstring(&mut env, &text)
}

/// `external fun nativeRestoreSession(filesDir): String`.
#[no_mangle]
pub extern "system" fn Java_com_yongshuai_ipaloader_MainActivity_nativeRestoreSession(
    mut env: JNIEnv,
    this: JObject,
    files_dir: JString,
) -> jstring {
    let files_dir = jstring_to_rust(&mut env, &files_dir);
    init_core(&files_dir);
    ensure_rpv_init(&mut env, &this);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        runtime().block_on(do_restore(files_dir))
    }));
    let text = match result {
        Ok(t) => t,
        Err(payload) => {
            let msg = panic_message(&*payload);
            logf!("nativeRestoreSession PANIC caught: {}", msg);
            format!("ERR\nPANIC\n{}", msg.replace('\n', " "))
        }
    };
    make_jstring(&mut env, &text)
}

/// `external fun nativeLogout(filesDir): Boolean`.
#[no_mangle]
pub extern "system" fn Java_com_yongshuai_ipaloader_MainActivity_nativeLogout(
    mut env: JNIEnv,
    this: JObject,
    files_dir: JString,
) -> jboolean {
    let files_dir = jstring_to_rust(&mut env, &files_dir);
    init_core(&files_dir);
    ensure_rpv_init(&mut env, &this);
    clear_session(&files_dir);
    logf!("logout: saved session cleared");
    1
}

// ---------------------------------------------------------------------------
// M3: IPA signing (0.5.0).
//
// JNI surface (on `com.yongshuai.ipaloader.SignActivity`):
// - `nativeInspectIpa(ipaPath, filesDir)` — reads the target app's
//   bundle id / name / version out of an IPA without touching the
//   network. Same result protocol as login: `OK\n…` / `ERR\n…`.
// - `nativeSignIpa(ipaPath, outPath, filesDir)` — the full portal
//   flow on the restored M2 session, driven by isideload's own
//   `Sideloader::sign_app` (certificate -> bundle id rewrite ->
//   App IDs -> app group -> team provisioning profile -> bundle
//   signing via apple-codesign-quick), then re-zips the signed .app
//   into the output IPA and reads the embedded profile's expiry.
//   Progress reaches Kotlin through `SignActivity.onSignProgress`
//   with stage keys: cert / appid / profile / sign / zip. Success is
//   `OK\n<output path>\n<profile expiry RFC3339 or empty>\n<new
//   bundle id>`; the certificate-limit stop is
//   `CERTLIMIT\n<json array of {name, machineName, expires}>`.
//
// M3.1 (0.5.1) adds the device stage and the pairing import (see
// the M3.1 section below): stage keys gain "device" in front, the
// no-pairing stop is the literal `NODEVICE`, and portal verdicts
// (DeveloperError) are reported as `ERR\nPORTAL\n<code>：<server
// message>` via sign_err_string instead of the generic classifier.
//
// Certificate hard rules (GOAL.md) and how this code keeps them:
// - The portal certificate is registered under one dedicated
//   machine name, MACHINE_NAME below. `CertificateIdentity::retrieve`
//   reuses the existing certificate when the stored private key
//   still matches it, and only submits a CSR for a new one when no
//   match exists.
// - Nothing here ever revokes a certificate. When Apple refuses a
//   new certificate with error 7460 (the free-account maximum),
//   isideload invokes the MaxCertsBehavior::Prompt callback; the
//   callback below only *records* the existing certificates and
//   returns `Ok(None)` — "revoke nothing" — which stops the library
//   with the 7460 error. The JNI layer then reports CERTLIMIT so
//   the UI can list the certificates and let the user decide
//   manually, outside this app.
// - App IDs: the library's `register_app_ids` lists the team's App
//   IDs first and only creates the ones that are missing.
// ---------------------------------------------------------------------------

/// Dedicated portal-certificate machine name for this app.
const MACHINE_NAME: &str = "ipaloader";

/// isideload-vfs resolves its temp dir through `std::env::temp_dir`,
/// which on Android (no TMPDIR set) is `/tmp` — not writable by an
/// app process. Point it at an app-private directory before any
/// `Application` extraction happens. Done once per process.
fn ensure_tmp_dir(files_dir: &str) {
    static TMP_ONCE: Once = Once::new();
    TMP_ONCE.call_once(|| {
        let dir = PathBuf::from(files_dir).join("core").join("tmp");
        let _ = std::fs::create_dir_all(&dir);
        std::env::set_var("TMPDIR", &dir);
        logf!("sign: TMPDIR -> {}", dir.display());
    });
}

/// Progress bridge from the signing flow to
/// `SignActivity.onSignProgress(String)`. Fire-and-forget: a failed
/// callback is logged, never fatal to signing.
struct SignProgressBridge {
    vm: JavaVM,
    callback: GlobalRef,
}

impl SignProgressBridge {
    fn stage(&self, stage: &str) {
        logf!("sign stage: {}", stage);
        let mut env = match self.vm.attach_current_thread() {
            Ok(e) => e,
            Err(e) => {
                logf!("sign progress: attach failed: {}", e);
                return;
            }
        };
        let Ok(j_stage) = env.new_string(stage) else {
            return;
        };
        let result = env.call_method(
            self.callback.as_obj(),
            "onSignProgress",
            "(Ljava/lang/String;)V",
            &[JValue::Object(&j_stage)],
        );
        if env.exception_check().unwrap_or(false) {
            let _ = env.exception_clear();
            logf!("sign progress: Kotlin callback threw (ignored)");
            return;
        }
        if let Err(e) = result {
            logf!("sign progress: call failed: {}", e);
        }
    }
}

/// One existing portal certificate, as reported to the UI when the
/// certificate limit is hit. `expires` is RFC 3339 (plist::Date's
/// XML format); the UI reformats it for display.
#[derive(Clone)]
struct CertSummary {
    name: String,
    machine_name: String,
    expires: String,
}

fn cert_summary(
    cert: &isideload::dev::certificates::DevelopmentCertificate,
) -> CertSummary {
    CertSummary {
        name: cert.name.clone().unwrap_or_default(),
        machine_name: cert.machine_name.clone().unwrap_or_default(),
        expires: cert
            .expiration_date
            .as_ref()
            .map(|d| d.to_xml_format())
            .unwrap_or_default(),
    }
}

fn cert_limit_json(certs: &[CertSummary]) -> String {
    let list: Vec<serde_json::Value> = certs
        .iter()
        .map(|c| {
            serde_json::json!({
                "name": c.name,
                "machineName": c.machine_name,
                "expires": c.expires,
            })
        })
        .collect();
    serde_json::Value::Array(list).to_string()
}

fn do_inspect(ipa_path: &str, files_dir: &str) -> String {
    use isideload::sideload::application::Application;
    ensure_tmp_dir(files_dir);
    logf!("inspect start: {}", ipa_path);
    let app = match Application::new(PathBuf::from(ipa_path)) {
        Ok(a) => a,
        Err(e) => {
            logf!("inspect failed: {:?}", e);
            return err_string(&e);
        }
    };
    let bundle_id = app.main_bundle_id().unwrap_or_default();
    let name = app.main_app_name().unwrap_or_default();
    let info = &app.bundle.app_info;
    let short = info
        .get("CFBundleShortVersionString")
        .and_then(|v| v.as_string())
        .unwrap_or("");
    let build = info
        .get("CFBundleVersion")
        .and_then(|v| v.as_string())
        .unwrap_or("");
    let version = if short.is_empty() {
        build.to_string()
    } else if build.is_empty() || build == short {
        short.to_string()
    } else {
        format!("{} ({})", short, build)
    };
    logf!(
        "inspect ok: bundle={} name={} version={}",
        bundle_id, name, version
    );
    format!(
        "OK\n{}\n{}\n{}",
        bundle_id.replace('\n', " "),
        name.replace('\n', " "),
        version.replace('\n', " ")
    )
}

fn read_bundle_id(app_dir: &std::path::Path) -> String {
    let info_path = app_dir.join("Info.plist");
    let Ok(bytes) = std::fs::read(&info_path) else {
        return String::new();
    };
    let Ok(value) = plist::Value::from_reader(std::io::Cursor::new(bytes)) else {
        return String::new();
    };
    let plist::Value::Dictionary(dict) = value else {
        return String::new();
    };
    dict.get("CFBundleIdentifier")
        .and_then(|v| v.as_string())
        .unwrap_or("")
        .to_string()
}

// ---------------------------------------------------------------------------
// M3.1: target device from an imported pairing file (0.5.1).
//
// The first real-device sign (0.5.0) died at the profile stage with
// DeveloperError 8220, "Your team has no devices from which to
// generate a provisioning profile": the team had no registered
// device UDIDs and the M3 flow never registered one. The fix pulls
// one small piece of M4 forward — the target device comes from the
// imported lockdown pairing file (the v1 route for device identity):
//
// - `nativeImportPairing(srcPath, filesDir)` parses the pairing
//   plist (XML or binary) with the plist crate, extracts the UDID
//   (and a device name if the file happens to carry one), stores
//   the raw pairing file at `<filesDir>/pairing/pairing.plist` for
//   M4's lockdown connection, and records udid/name alongside it.
//   Parse problems come back as `ERR\nPAIRING\n<人話訊息>`.
// - `nativeGetPairing(filesDir)` reports the stored target as
//   `OK\n<udid>\n<name>` or `NONE`, so the sign page can show the
//   current target device.
// - `do_sign` runs a new "device" stage before anything else: with
//   no imported pairing file it stops there (`NODEVICE`) instead of
//   letting the raw 8220 surface later; otherwise it makes sure the
//   target UDID is registered on the team (see ensure_team_device).
// ---------------------------------------------------------------------------

/// The imported target device: UDID from the pairing file, plus a
/// device name when the file carried one (standard lockdown pairing
/// records do not, so this is usually empty).
struct PairingInfo {
    udid: String,
    name: String,
}

fn pairing_dir(files_dir: &str) -> PathBuf {
    PathBuf::from(files_dir).join("pairing")
}

fn load_pairing(files_dir: &str) -> Option<PairingInfo> {
    let dir = pairing_dir(files_dir);
    let udid = std::fs::read_to_string(dir.join("udid.txt"))
        .ok()?
        .trim()
        .to_string();
    if udid.is_empty() {
        return None;
    }
    let name = std::fs::read_to_string(dir.join("name.txt"))
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    Some(PairingInfo { udid, name })
}

/// UDIDs are device identifiers; logs and stage lines carry only
/// the last 6 characters.
fn mask_udid(udid: &str) -> String {
    let tail: String = udid
        .chars()
        .rev()
        .take(6)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    format!("…{}", tail)
}

fn do_import_pairing(src_path: &str, files_dir: &str) -> String {
    logf!("pairing import start: {}", src_path);
    let bytes = match std::fs::read(src_path) {
        Ok(b) => b,
        Err(e) => {
            return format!("ERR\nPAIRING\n無法讀取所選檔案：{}", e);
        }
    };
    let value = match plist::Value::from_reader(std::io::Cursor::new(&bytes)) {
        Ok(v) => v,
        Err(_) => {
            return "ERR\nPAIRING\n配對檔解析失敗：不是有效的 plist 檔案，請確認選的是配對檔"
                .to_string();
        }
    };
    let plist::Value::Dictionary(dict) = value else {
        return "ERR\nPAIRING\n配對檔解析失敗：內容不是配對記錄的格式".to_string();
    };
    let udid = dict
        .get("UDID")
        .and_then(|v| v.as_string())
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    // UDIDs are 40 hex chars (older devices) or 8 hex + "-" + 16
    // hex (newer). Accept hex/dash only, with a sane length floor.
    let valid = udid.len() >= 20 && udid.chars().all(|c| c.is_ascii_hexdigit() || c == '-');
    if !valid {
        return "ERR\nPAIRING\n配對檔解析失敗：找不到裝置 UDID，請確認選的是與 iPhone 配對產生的配對檔"
            .to_string();
    }
    let name = dict
        .get("DeviceName")
        .and_then(|v| v.as_string())
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let dir = pairing_dir(files_dir);
    let writes = std::fs::create_dir_all(&dir)
        .and_then(|_| std::fs::write(dir.join("pairing.plist"), &bytes))
        .and_then(|_| std::fs::write(dir.join("udid.txt"), &udid))
        .and_then(|_| std::fs::write(dir.join("name.txt"), &name));
    if let Err(e) = writes {
        return format!("ERR\nPAIRING\n配對檔儲存失敗：{}", e);
    }
    logf!("pairing imported: udid {}", mask_udid(&udid));
    format!(
        "OK\n{}\n{}",
        udid.replace('\n', " "),
        name.replace('\n', " ")
    )
}

fn do_pairing_info(files_dir: &str) -> String {
    match load_pairing(files_dir) {
        Some(p) => format!(
            "OK\n{}\n{}",
            p.udid.replace('\n', " "),
            p.name.replace('\n', " ")
        ),
        None => "NONE".to_string(),
    }
}

/// The portal's DeveloperError (a server verdict) buried anywhere
/// in a report's cause chain, if there is one.
fn developer_error(report: &Report) -> Option<(i64, String)> {
    for node in report.iter_reports() {
        if let Some(isideload::SideloadError::DeveloperError(code, message)) =
            node.downcast_current_context::<isideload::SideloadError>()
        {
            return Some((*code, message.clone()));
        }
    }
    None
}

/// Error mapping for the sign flow only (the login flow keeps the
/// shared `classify`). A DeveloperError is Apple's verdict, never a
/// connectivity problem — the generic heuristic used to file 8220
/// under NETWORK because its own message contains the words
/// "Connect" and "Certificates". DeveloperErrors surface as PORTAL
/// with the server's message verbatim; NETWORK is reserved for
/// actual transport failures (reqwest send errors, DNS, timeouts).
fn sign_err_string(report: &Report) -> String {
    if let Some((code, message)) = developer_error(report) {
        return format!("ERR\nPORTAL\n{}：{}", code, message.replace('\n', " "));
    }
    let chain = format!("{:?}", report).to_lowercase();
    let code = if chain.contains("anisette") {
        "ANISETTE"
    } else if chain.contains("error sending request")
        || chain.contains("dns")
        || chain.contains("timed out")
        || chain.contains("timeout")
    {
        "NETWORK"
    } else {
        "UNKNOWN"
    };
    let (_, message) = classify(report);
    format!("ERR\n{}\n{}", code, message)
}

/// Make sure the target device is registered on the team, using
/// the library's own DevicesApi (QH65B2 listDevices / addDevice) —
/// the same calls `Sideloader::install_app` makes before signing,
/// except the device here comes from the imported pairing file
/// instead of a live lockdown connection. Lists first and only
/// registers when the target UDID is genuinely missing (other
/// devices on the team do not count: M4 installs to *this* one).
/// A racing registration (DeveloperError 35) is treated as success,
/// mirroring the library's own `ensure_device_registered`.
async fn ensure_team_device(
    session: &mut DeveloperSession,
    team: &isideload::dev::teams::DeveloperTeam,
    pairing: &PairingInfo,
) -> Result<(), Report> {
    use isideload::dev::devices::DevicesApi;

    let devices = session
        .list_devices(team, None)
        .await
        .context("Failed to list developer devices")?;
    let already = devices
        .iter()
        .any(|d| d.device_number.trim().eq_ignore_ascii_case(&pairing.udid));
    if already {
        logf!(
            "sign device: target {} already registered ({} device(s) on team)",
            mask_udid(&pairing.udid),
            devices.len()
        );
        return Ok(());
    }
    let reg_name = if pairing.name.is_empty() {
        MACHINE_NAME
    } else {
        pairing.name.as_str()
    };
    match session.add_device(team, reg_name, &pairing.udid, None).await {
        Ok(_) => {
            logf!(
                "sign device: registered target {} as \"{}\" ({} device(s) before)",
                mask_udid(&pairing.udid),
                reg_name,
                devices.len()
            );
            Ok(())
        }
        Err(e) => {
            if matches!(developer_error(&e), Some((35, _))) {
                logf!("sign device: add raced an existing registration (35), continuing");
                Ok(())
            } else {
                Err(e)
            }
        }
    }
}

async fn do_sign(
    files_dir: String,
    ipa_path: String,
    out_path: String,
    bridge: Arc<SignProgressBridge>,
) -> String {
    use isideload::dev::certificates::DevelopmentCertificate;
    use isideload::sideload::builder::{
        ExtensionsBehavior, ExtensionsBehaviorSelection, MaxCertsBehavior,
    };
    use isideload::sideload::{SideloaderBuilder, TeamSelection};
    use isideload::util::callbacks::ExtensionsBehaviorCallbackBox;

    ensure_tmp_dir(&files_dir);
    logf!("sign start: {}", ipa_path);
    // M3.1 device stage: without an imported pairing file there is
    // no target device to register — stop here with a clear result
    // instead of dying at the profile stage with Apple's raw 8220.
    bridge.stage("device");
    let Some(pairing) = load_pairing(&files_dir) else {
        logf!("sign: no pairing file imported, stopping at device stage");
        return "NODEVICE".to_string();
    };
    logf!("sign: target device {}", mask_udid(&pairing.udid));
    let Some((email, spd)) = load_session(&files_dir) else {
        logf!("sign: no saved session");
        return "ERR\nNO_SESSION\n尚未登入，請先登入 Apple ID".to_string();
    };
    let provider = match anisette_provider(&files_dir) {
        Ok(p) => p,
        Err(e) => {
            logf!("sign: anisette provider failed: {:?}", e);
            return sign_err_string(&e);
        }
    };
    let mut account = match AppleAccountBuilder::new(&email)
        .anisette_provider(provider)
        .build()
        .await
    {
        Ok(a) => a,
        Err(e) => {
            logf!("sign: account build failed: {:?}", e);
            return sign_err_string(&e);
        }
    };
    account.spd = Some(spd);
    // Same session validation as restore: a real app-token exchange
    // happens inside from_account before any portal mutation.
    let dev_session = match DeveloperSession::from_account(&mut account).await {
        Ok(s) => s,
        Err(e) => {
            logf!("sign: developer session failed: {:?}", e);
            return sign_err_string(&e);
        }
    };
    logf!("sign: developer session ready for {}", email);

    let cert_limit: Arc<Mutex<Option<Vec<CertSummary>>>> = Arc::new(Mutex::new(None));
    let cert_limit_cb = cert_limit.clone();
    // The prompt callback is where the library asks "which of these
    // certificates may I revoke?" — the answer is always "none".
    // Returning Ok(None) stops the flow with Apple's 7460 error and
    // leaves every certificate untouched; the captured list is
    // reported to the UI instead (see the CERTLIMIT result below).
    let max_certs = MaxCertsBehavior::Prompt(
        move |certs: Vec<DevelopmentCertificate>| {
            let slot = cert_limit_cb.clone();
            async move {
                let summaries: Vec<CertSummary> =
                    certs.iter().map(cert_summary).collect();
                logf!(
                    "sign: certificate limit (7460): {} existing certificates, revoking none",
                    summaries.len()
                );
                if let Ok(mut guard) = slot.lock() {
                    *guard = Some(summaries);
                }
                Ok(None)
            }
        },
    );

    let builder: SideloaderBuilder<_, ExtensionsBehaviorCallbackBox> =
        SideloaderBuilder::new(dev_session, email.clone())
            .team_selection(TeamSelection::First)
            .machine_name(MACHINE_NAME.to_string())
            .max_certs_behavior(max_certs)
            .extensions_behavior(ExtensionsBehaviorSelection::Fixed(
                ExtensionsBehavior::RegisterAll,
            ))
            .storage(Box::new(session_storage(&files_dir)));
    let mut loader = builder.build();

    // M3.1: register the target device on the team (list → add
    // only if missing) before sign_app reaches the profile stage,
    // where an unregistered-device team fails with error 8220.
    let team = match loader.get_team().await {
        Ok(t) => t,
        Err(e) => {
            logf!("sign: team resolution failed: {:?}", e);
            return sign_err_string(&e);
        }
    };
    if let Err(e) = ensure_team_device(loader.get_dev_session(), &team, &pairing).await {
        logf!("sign: device registration failed: {:?}", e);
        return sign_err_string(&e);
    }

    bridge.stage("cert");
    let progress_bridge = bridge.clone();
    let progress = move |p: f32| {
        let b = progress_bridge.clone();
        async move {
            let stage = if p < 0.15 {
                "appid"
            } else if p < 0.3 {
                "profile"
            } else {
                "sign"
            };
            b.stage(stage);
        }
    };

    let signed = loader
        .sign_app(
            PathBuf::from(&ipa_path),
            Some(team),
            false,
            Some(progress),
            None,
            None,
        )
        .await;
    let (signed_dir, _special) = match signed {
        Ok(v) => v,
        Err(e) => {
            if let Ok(guard) = cert_limit.lock() {
                if let Some(certs) = guard.as_ref() {
                    logf!("sign stopped: certificate limit");
                    return format!("CERTLIMIT\n{}", cert_limit_json(certs));
                }
            }
            logf!("sign failed: {:?}", e);
            return sign_err_string(&e);
        }
    };
    logf!("sign: bundle signed at {}", signed_dir.display());

    bridge.stage("zip");
    // The embedded profile is the authoritative expiry source: it is
    // what iOS enforces, created moments ago (free-provisioning
    // profiles live 7 days from their creation time).
    let mut expiry_iso = String::new();
    let profile_path = signed_dir.join("embedded.mobileprovision");
    match std::fs::read(&profile_path) {
        Ok(bytes) => match apple_codesign_quick::ProvisioningProfile::parse(&bytes) {
            Ok(profile) => {
                if let Some(plist::Value::Date(d)) = profile.plist().get("ExpirationDate") {
                    expiry_iso = d.to_xml_format();
                }
                logf!(
                    "sign: embedded profile parsed (expiry present: {})",
                    !expiry_iso.is_empty()
                );
            }
            Err(e) => logf!("sign: embedded profile parse failed: {}", e),
        },
        Err(e) => logf!("sign: embedded.mobileprovision unreadable: {}", e),
    }

    let signed_bundle_id = read_bundle_id(&signed_dir);
    let size = match zip_app_bundle(&signed_dir, &out_path) {
        Ok(s) => s,
        Err(e) => {
            logf!("sign: rezip failed: {}", e);
            return format!("ERR\nUNKNOWN\n{}", e.replace('\n', " "));
        }
    };
    logf!(
        "sign ok: {} ({} bytes), bundle {}, profile expiry {}",
        out_path, size, signed_bundle_id, expiry_iso
    );
    // Best-effort cleanup of the extraction tree the signed bundle
    // lives in (<tmp>/<ipa file name>_extracted).
    if let Some(root) = signed_dir.ancestors().nth(2) {
        let root = root.to_path_buf();
        let _ = std::fs::remove_dir_all(&root);
    }
    format!(
        "OK\n{}\n{}\n{}",
        out_path.replace('\n', " "),
        expiry_iso,
        signed_bundle_id.replace('\n', " ")
    )
}

/// Re-zip a signed .app bundle into an installable IPA at
/// `out_path` (`Payload/<App>.app/…`), preserving unix permissions
/// (the main executable must stay executable) and symlinks.
fn zip_app_bundle(app_dir: &std::path::Path, out_path: &str) -> Result<u64, String> {
    let app_name = app_dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .ok_or_else(|| "signed bundle has no name".to_string())?;
    let out = PathBuf::from(out_path);
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create output dir: {}", e))?;
    }
    let file = std::fs::File::create(&out).map_err(|e| format!("create output: {}", e))?;
    let mut writer = zip::ZipWriter::new(file);

    // Collect the tree first so entries land in a stable order.
    let mut paths: Vec<PathBuf> = Vec::new();
    let mut stack: Vec<PathBuf> = vec![app_dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let read = std::fs::read_dir(&dir).map_err(|e| format!("read dir: {}", e))?;
        for entry in read {
            let entry = entry.map_err(|e| format!("read entry: {}", e))?;
            let path = entry.path();
            let is_dir = entry
                .file_type()
                .map(|ft| ft.is_dir())
                .unwrap_or(false);
            paths.push(path.clone());
            if is_dir {
                stack.push(path);
            }
        }
    }
    paths.sort();

    for path in paths {
        let rel = path
            .strip_prefix(app_dir)
            .map_err(|e| format!("strip prefix: {}", e))?;
        let entry_name = format!("Payload/{}/{}", app_name, rel.to_string_lossy());
        let meta = std::fs::symlink_metadata(&path)
            .map_err(|e| format!("stat {}: {}", entry_name, e))?;
        let mode = {
            use std::os::unix::fs::PermissionsExt as _;
            meta.permissions().mode()
        };
        if meta.file_type().is_symlink() {
            let target = std::fs::read_link(&path)
                .map_err(|e| format!("read link {}: {}", entry_name, e))?;
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored)
                .unix_permissions(0o120_777);
            writer
                .start_file(&entry_name, options)
                .map_err(|e| format!("zip link {}: {}", entry_name, e))?;
            writer
                .write_all(target.to_string_lossy().as_bytes())
                .map_err(|e| format!("zip link write {}: {}", entry_name, e))?;
        } else if meta.is_dir() {
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored)
                .unix_permissions(mode & 0o7777);
            writer
                .start_file(format!("{}/", entry_name), options)
                .map_err(|e| format!("zip dir {}: {}", entry_name, e))?;
        } else {
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated)
                .unix_permissions(mode & 0o7777);
            writer
                .start_file(&entry_name, options)
                .map_err(|e| format!("zip file {}: {}", entry_name, e))?;
            let mut f = std::fs::File::open(&path)
                .map_err(|e| format!("open {}: {}", entry_name, e))?;
            std::io::copy(&mut f, &mut writer)
                .map_err(|e| format!("zip write {}: {}", entry_name, e))?;
        }
    }
    writer.finish().map_err(|e| format!("zip finish: {}", e))?;
    let size = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
    Ok(size)
}

/// `external fun nativeInspectIpa(ipaPath, filesDir): String` in
/// `com.yongshuai.ipaloader.SignActivity`.
#[no_mangle]
pub extern "system" fn Java_com_yongshuai_ipaloader_SignActivity_nativeInspectIpa(
    mut env: JNIEnv,
    this: JObject,
    ipa_path: JString,
    files_dir: JString,
) -> jstring {
    let ipa_path = jstring_to_rust(&mut env, &ipa_path);
    let files_dir = jstring_to_rust(&mut env, &files_dir);
    init_core(&files_dir);
    ensure_rpv_init(&mut env, &this);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        do_inspect(&ipa_path, &files_dir)
    }));
    let text = match result {
        Ok(t) => t,
        Err(payload) => {
            let msg = panic_message(&*payload);
            logf!("nativeInspectIpa PANIC caught: {}", msg);
            format!("ERR\nPANIC\n{}", msg.replace('\n', " "))
        }
    };
    make_jstring(&mut env, &text)
}

/// `external fun nativeSignIpa(ipaPath, outPath, filesDir): String`
/// in `com.yongshuai.ipaloader.SignActivity`. Blocking: run on a
/// background thread. `this` must implement
/// `onSignProgress(String)`.
#[no_mangle]
pub extern "system" fn Java_com_yongshuai_ipaloader_SignActivity_nativeSignIpa(
    mut env: JNIEnv,
    this: JObject,
    ipa_path: JString,
    out_path: JString,
    files_dir: JString,
) -> jstring {
    let ipa_path = jstring_to_rust(&mut env, &ipa_path);
    let out_path = jstring_to_rust(&mut env, &out_path);
    let files_dir = jstring_to_rust(&mut env, &files_dir);
    init_core(&files_dir);
    ensure_rpv_init(&mut env, &this);
    logf!("nativeSignIpa called");
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let vm = match env.get_java_vm() {
            Ok(vm) => vm,
            Err(e) => return format!("ERR\nUNKNOWN\nJNI VM: {}", e),
        };
        let callback = match env.new_global_ref(&this) {
            Ok(r) => r,
            Err(e) => return format!("ERR\nUNKNOWN\nJNI ref: {}", e),
        };
        let bridge = Arc::new(SignProgressBridge { vm, callback });
        runtime().block_on(do_sign(files_dir, ipa_path, out_path, bridge))
    }));
    let text = match result {
        Ok(t) => t,
        Err(payload) => {
            let msg = panic_message(&*payload);
            logf!("nativeSignIpa PANIC caught: {}", msg);
            format!("ERR\nPANIC\n{}", msg.replace('\n', " "))
        }
    };
    make_jstring(&mut env, &text)
}

/// `external fun nativeImportPairing(srcPath, filesDir): String` in
/// `com.yongshuai.ipaloader.SignActivity` (M3.1). Parses the picked
/// lockdown pairing file and stores it (plus its UDID) as the sign
/// flow's target device. Result: `OK\n<udid>\n<name>` or
/// `ERR\nPAIRING\n<message>`.
#[no_mangle]
pub extern "system" fn Java_com_yongshuai_ipaloader_SignActivity_nativeImportPairing(
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

/// `external fun nativeGetPairing(filesDir): String` in
/// `com.yongshuai.ipaloader.SignActivity` (M3.1). Reports the
/// stored target device: `OK\n<udid>\n<name>` or `NONE`.
#[no_mangle]
pub extern "system" fn Java_com_yongshuai_ipaloader_SignActivity_nativeGetPairing(
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

// ---------------------------------------------------------------------------
// 0.6.2: portal inventory view (certificates + App IDs) for the
// main screen's inventory tab. Read-only: the session is restored
// exactly the way `do_sign` restores it, then the same portal list
// calls the sign flow itself uses (`list_ios_certs`,
// `list_app_ids`) run against the first team. Nothing is created,
// revoked or deleted here. Result protocol:
// `OK\n<team name>\n<certificates JSON>\n<app ids JSON>`, or the
// sign flow's error strings (`ERR\nNO_SESSION\n…`, PORTAL, …).
// ---------------------------------------------------------------------------

async fn do_list_inventory(files_dir: String) -> String {
    use isideload::dev::app_ids::AppIdsApi;
    use isideload::dev::certificates::CertificatesApi;

    let Some((email, spd)) = load_session(&files_dir) else {
        logf!("inventory: no saved session");
        return "ERR\nNO_SESSION\n尚未登入，請先登入 Apple ID".to_string();
    };
    let provider = match anisette_provider(&files_dir) {
        Ok(p) => p,
        Err(e) => {
            logf!("inventory: anisette provider failed: {:?}", e);
            return sign_err_string(&e);
        }
    };
    let mut account = match AppleAccountBuilder::new(&email)
        .anisette_provider(provider)
        .build()
        .await
    {
        Ok(a) => a,
        Err(e) => {
            logf!("inventory: account build failed: {:?}", e);
            return sign_err_string(&e);
        }
    };
    account.spd = Some(spd);
    let mut session = match DeveloperSession::from_account(&mut account).await {
        Ok(s) => s,
        Err(e) => {
            logf!("inventory: developer session failed: {:?}", e);
            return sign_err_string(&e);
        }
    };
    let teams = match session.list_teams().await {
        Ok(t) => t,
        Err(e) => {
            logf!("inventory: list teams failed: {:?}", e);
            return sign_err_string(&e);
        }
    };
    let Some(team) = teams.into_iter().next() else {
        logf!("inventory: no teams");
        return "ERR\nPORTAL\n找不到開發者團隊".to_string();
    };
    let team_name = team.name.clone().unwrap_or_default();
    let certs = match session.list_ios_certs(&team).await {
        Ok(c) => c,
        Err(e) => {
            logf!("inventory: list certs failed: {:?}", e);
            return sign_err_string(&e);
        }
    };
    let cert_count = certs.len();
    let certs_json = {
        let summaries: Vec<CertSummary> = certs.iter().map(cert_summary).collect();
        cert_limit_json(&summaries)
    };
    let app_ids = match session.list_app_ids(&team, None).await {
        Ok(r) => r,
        Err(e) => {
            logf!("inventory: list app ids failed: {:?}", e);
            return sign_err_string(&e);
        }
    };
    let app_id_count = app_ids.app_ids.len();
    let app_ids_json = {
        let list: Vec<serde_json::Value> = app_ids
            .app_ids
            .iter()
            .map(|a| {
                serde_json::json!({
                    "name": a.name,
                    "identifier": a.identifier,
                    "expires": a
                        .expiration_date
                        .as_ref()
                        .map(|d| d.to_xml_format())
                        .unwrap_or_default(),
                })
            })
            .collect();
        serde_json::Value::Array(list).to_string()
    };
    logf!(
        "inventory: {} cert(s), {} app id(s) on team {}",
        cert_count,
        app_id_count,
        team.team_id
    );
    format!(
        "OK\n{}\n{}\n{}",
        team_name.replace('\n', " "),
        certs_json,
        app_ids_json
    )
}

/// `external fun nativeListInventory(filesDir): String` in
/// `com.yongshuai.ipaloader.MainActivity`. Blocking: run on a
/// background thread.
#[no_mangle]
pub extern "system" fn Java_com_yongshuai_ipaloader_MainActivity_nativeListInventory(
    mut env: JNIEnv,
    this: JObject,
    files_dir: JString,
) -> jstring {
    let files_dir = jstring_to_rust(&mut env, &files_dir);
    init_core(&files_dir);
    ensure_rpv_init(&mut env, &this);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        runtime().block_on(do_list_inventory(files_dir))
    }));
    let text = match result {
        Ok(t) => t,
        Err(payload) => {
            let msg = panic_message(&*payload);
            logf!("nativeListInventory PANIC caught: {}", msg);
            format!("ERR\nPANIC\n{}", msg.replace('\n', " "))
        }
    };
    make_jstring(&mut env, &text)
}

mod install;
