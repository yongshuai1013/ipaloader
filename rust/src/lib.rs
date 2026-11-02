//! ipaloader Rust core (M2: real Apple ID login).
//!
//! JNI surface (all on `app.ipaloader.MainActivity`):
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
//!
//! The password and 2FA codes are never logged.

use std::path::PathBuf;
use std::sync::{Arc, Once, OnceLock};

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

fn init_isideload() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
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
        message.truncate(220);
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
            return Ok(TwoFactorCallbackResponse::Abort);
        }
        let result = result?;
        let j_str: JString = result.l()?.into();
        let reply: String = String::from(env.get_string(&j_str)?);
        Ok(parse_two_factor_reply(&reply))
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
        Err(_) => return String::new(),
    };
    match session.list_teams().await {
        Ok(teams) => teams
            .into_iter()
            .next()
            .and_then(|t| t.name)
            .unwrap_or_default(),
        Err(_) => String::new(),
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
    let mut account = AppleAccountBuilder::new(&email)
        .anisette_provider(anisette_provider(&files_dir)?)
        .login(&password, callback)
        .await?;
    save_session(&files_dir, &account)?;
    let team = fetch_team_name(&mut account).await;
    Ok(format!("OK\n{}\n{}", account.email, team.replace('\n', " ")))
}

async fn do_restore(files_dir: String) -> String {
    let Some((email, spd)) = load_session(&files_dir) else {
        return "NONE".to_string();
    };
    let mut account = match AppleAccountBuilder::new(&email)
        .anisette_provider(match anisette_provider(&files_dir) {
            Ok(p) => p,
            Err(e) => return err_string(&e),
        })
        .build()
        .await
    {
        Ok(a) => a,
        Err(e) => return err_string(&e),
    };
    account.spd = Some(spd);
    // Validate the saved session with a real token exchange + team list;
    // a session that Apple no longer accepts is dropped.
    let mut session = match DeveloperSession::from_account(&mut account).await {
        Ok(s) => s,
        Err(e) => {
            let chain = format!("{:?}", e).to_lowercase();
            if chain.contains("status code") || chain.contains("401") || chain.contains("403") {
                clear_session(&files_dir);
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
            format!("OK\n{}\n{}", email, team.replace('\n', " "))
        }
        Err(e) => err_string(&e),
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

/// `external fun nativeVersion(): String` in `app.ipaloader.MainActivity`.
#[no_mangle]
pub extern "system" fn Java_app_ipaloader_MainActivity_nativeVersion(
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
pub extern "system" fn Java_app_ipaloader_MainActivity_nativeLogin(
    mut env: JNIEnv,
    this: JObject,
    email: JString,
    password: JString,
    files_dir: JString,
) -> jstring {
    init_isideload();
    let email = jstring_to_rust(&mut env, &email);
    let password = jstring_to_rust(&mut env, &password);
    let files_dir = jstring_to_rust(&mut env, &files_dir);
    let vm = match env.get_java_vm() {
        Ok(vm) => vm,
        Err(e) => return make_jstring(&mut env, &format!("ERR\nUNKNOWN\nJNI VM: {}", e)),
    };
    let callback = match env.new_global_ref(&this) {
        Ok(r) => r,
        Err(e) => return make_jstring(&mut env, &format!("ERR\nUNKNOWN\nJNI ref: {}", e)),
    };
    let bridge = Arc::new(TwoFactorBridge { vm, callback });
    let result = runtime().block_on(do_login(email, password, files_dir, bridge));
    let text = match result {
        Ok(ok) => ok,
        Err(e) => err_string(&e),
    };
    make_jstring(&mut env, &text)
}

/// `external fun nativeRestoreSession(filesDir): String`.
#[no_mangle]
pub extern "system" fn Java_app_ipaloader_MainActivity_nativeRestoreSession(
    mut env: JNIEnv,
    _this: JObject,
    files_dir: JString,
) -> jstring {
    init_isideload();
    let files_dir = jstring_to_rust(&mut env, &files_dir);
    let text = runtime().block_on(do_restore(files_dir));
    make_jstring(&mut env, &text)
}

/// `external fun nativeLogout(filesDir): Boolean`.
#[no_mangle]
pub extern "system" fn Java_app_ipaloader_MainActivity_nativeLogout(
    mut env: JNIEnv,
    _this: JObject,
    files_dir: JString,
) -> jboolean {
    let files_dir = jstring_to_rust(&mut env, &files_dir);
    clear_session(&files_dir);
    1
}
