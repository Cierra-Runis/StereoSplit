//! Windows notifications (toasts) for errors and hints. Unlike a message box they don't wait
//! for the user, so the thread that shows one carries on right away.
//!
//! A program that isn't installed from a package needs an AppUserModelID registered under
//! HKCU for Windows to show its notifications; `register` writes it on first use.

use std::sync::Once;

use tracing::warn;
use windows::core::{Interface, HSTRING};
use windows::Data::Xml::Dom::IXmlNode;
use windows::UI::Notifications::{ToastNotification, ToastNotificationManager, ToastTemplateType};

use crate::APP_NAME;

const APP_USER_MODEL_ID: &str = "StereoSplit";

/// Show a notification with a bold `title` line and `body` below it. Failures are only logged.
pub fn show(title: &str, body: &str) {
    if let Err(e) = try_show(title, body) {
        warn!(error = %e, title, body, "failed to show a notification");
    }
}

fn try_show(title: &str, body: &str) -> windows::core::Result<()> {
    static REGISTER: Once = Once::new();
    REGISTER.call_once(|| {
        if let Err(e) = register() {
            warn!(error = %e, "failed to register for notifications");
        }
    });

    let xml = ToastNotificationManager::GetTemplateContent(ToastTemplateType::ToastText02)?;
    let texts = xml.GetElementsByTagName(&HSTRING::from("text"))?;
    for (i, s) in [title, body].into_iter().enumerate() {
        let node = xml.CreateTextNode(&HSTRING::from(s))?.cast::<IXmlNode>()?;
        texts.Item(i as u32)?.AppendChild(&node)?;
    }
    let toast = ToastNotification::CreateToastNotification(&xml)?;
    ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(APP_USER_MODEL_ID))?
        .Show(&toast)
}

/// Register the AppUserModelID with the name shown on the notifications. They show the
/// default icon: an icon needs an image file on disk (`IconUri`), which is left to packaging.
fn register() -> std::io::Result<()> {
    use winreg::enums::HKEY_CURRENT_USER;
    let (key, _) = winreg::RegKey::predef(HKEY_CURRENT_USER).create_subkey(format!(
        r"Software\Classes\AppUserModelId\{APP_USER_MODEL_ID}"
    ))?;
    key.set_value("DisplayName", &APP_NAME)
}
