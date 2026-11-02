//! ipaloader Rust core (M1 shell).
//!
//! Exposes a single JNI placeholder, `MainActivity.nativeVersion`, which
//! returns a version string so the Kotlin shell can prove the
//! Kotlin <-> Rust channel works. The real isideload login / signing /
//! install flows land in M2+; the dependencies are already linked so the
//! cross-compile pipeline is exercised end to end.

use jni::objects::JObject;
use jni::sys::jstring;
use jni::JNIEnv;

/// `external fun nativeVersion(): String` in `app.ipaloader.MainActivity`.
#[no_mangle]
pub extern "system" fn Java_app_ipaloader_MainActivity_nativeVersion(
    mut env: JNIEnv,
    _this: JObject,
) -> jstring {
    // Reference the core crates so the linker keeps them in the cdylib;
    // their public entry points are what M2 builds the login flow on.
    let _addr = idevice::usbmuxd::UsbmuxdAddr::from_env_var();

    let text = "rust core ok: isideload 0.5.2 / idevice 0.1.68";
    env.new_string(text)
        .expect("failed to create JNI string")
        .into_raw()
}
