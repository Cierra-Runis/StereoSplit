//! Embeds the icon and version info into the exe (shown by Explorer, Task Manager and shortcuts).
//! The name and icon resource are passed on to the code as `APP_NAME` and `ICON_RESOURCE`, so each
//! is defined only here.

const APP_NAME: &str = "Stereo Split";
const ICON_RESOURCE: &str = "APP_ICON";
const ICON_FILE: &str = "assets/icon/icon.ico";

fn main() {
    println!("cargo:rustc-env=APP_NAME={APP_NAME}");
    println!("cargo:rustc-env=ICON_RESOURCE={ICON_RESOURCE}");
    println!("cargo:rerun-if-changed={ICON_FILE}");

    winresource::WindowsResource::new()
        .set_icon_with_id(ICON_FILE, ICON_RESOURCE)
        // Windows shows FileDescription as the program's name, e.g. in Task Manager
        .set("FileDescription", APP_NAME)
        .set("ProductName", APP_NAME)
        .compile()
        .expect("failed to embed the Windows resources");
}
