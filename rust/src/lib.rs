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
const ANISETTE_URL: &str = "https://ani2.stikstore.app/";

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

fn init_core(files_dir: &str) {
    let _ = LOG_PATH.set(log_path_for(files_dir));
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
    let code = if chain.contains("no 2fa code provided")
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
    _this: JObject,
) -> jstring {
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
    _this: JObject,
    files_dir: JString,
) -> jstring {
    let files_dir = jstring_to_rust(&mut env, &files_dir);
    init_core(&files_dir);
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
    _this: JObject,
    files_dir: JString,
) -> jboolean {
    let files_dir = jstring_to_rust(&mut env, &files_dir);
    init_core(&files_dir);
    clear_session(&files_dir);
    logf!("logout: saved session cleared");
    1
}
