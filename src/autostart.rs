//! "Start with Windows": an entry for this exe under the current user's Run key

use crate::APP_NAME;
use winreg::enums::{HKEY_CURRENT_USER, KEY_SET_VALUE};
use winreg::RegKey;

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

pub fn enabled() -> bool {
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(RUN_KEY)
        .and_then(|k| k.get_value::<String, _>(APP_NAME))
        .is_ok()
}

pub fn set(on: bool) -> std::io::Result<()> {
    let key = RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags(RUN_KEY, KEY_SET_VALUE)?;
    if on {
        let exe = std::env::current_exe()?;
        key.set_value(APP_NAME, &format!("\"{}\"", exe.display()))
    } else {
        key.delete_value(APP_NAME)
    }
}
