//! Registers the `nbs-native-host` binary as a Native Messaging host so the
//! browser extension can reach (and launch) the app without the WebSocket.

use std::fs;
use std::path::PathBuf;

use serde_json::json;

pub const HOST_NAME: &str = "com.nbs.downloadmanager";
/// Fixed by the `key` field in extension/manifest.json.
pub const CHROME_EXTENSION_ID: &str = "flikhhghfebchbjchkfcdicnjgidonkd";
/// `browser_specific_settings.gecko.id` in extension/manifest.json.
pub const FIREFOX_EXTENSION_ID: &str = "nbs-download-manager@nbs.local";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Browser {
    Chrome,
    Chromium,
    Edge,
    Brave,
    Firefox,
}

impl Browser {
    fn parse(name: &str) -> Result<Self, String> {
        Ok(match name.to_ascii_lowercase().as_str() {
            "chrome" => Self::Chrome,
            "chromium" => Self::Chromium,
            "edge" => Self::Edge,
            "brave" => Self::Brave,
            "firefox" => Self::Firefox,
            other => return Err(format!("unknown browser: {other}")),
        })
    }

    #[cfg(target_os = "linux")]
    fn manifest_dir(self) -> Option<PathBuf> {
        let home = dirs::home_dir()?;
        let config = dirs::config_dir().unwrap_or_else(|| home.join(".config"));
        Some(match self {
            Self::Chrome => config.join("google-chrome/NativeMessagingHosts"),
            Self::Chromium => config.join("chromium/NativeMessagingHosts"),
            Self::Edge => config.join("microsoft-edge/NativeMessagingHosts"),
            Self::Brave => config.join("BraveSoftware/Brave-Browser/NativeMessagingHosts"),
            Self::Firefox => home.join(".mozilla/native-messaging-hosts"),
        })
    }

    #[cfg(target_os = "macos")]
    fn manifest_dir(self) -> Option<PathBuf> {
        let support = dirs::home_dir()?.join("Library/Application Support");
        Some(match self {
            Self::Chrome => support.join("Google/Chrome/NativeMessagingHosts"),
            Self::Chromium => support.join("Chromium/NativeMessagingHosts"),
            Self::Edge => support.join("Microsoft Edge/NativeMessagingHosts"),
            Self::Brave => support.join("BraveSoftware/Brave-Browser/NativeMessagingHosts"),
            Self::Firefox => support.join("Mozilla/NativeMessagingHosts"),
        })
    }

    #[cfg(windows)]
    fn manifest_dir(self) -> Option<PathBuf> {
        Some(dirs::data_local_dir()?.join("NBS Download Manager").join("native-messaging"))
    }

    #[cfg(windows)]
    fn registry_key(self) -> String {
        let vendor = match self {
            Self::Chrome => r"Google\Chrome",
            Self::Chromium => r"Chromium",
            Self::Edge => r"Microsoft\Edge",
            Self::Brave => r"BraveSoftware\Brave-Browser",
            Self::Firefox => r"Mozilla",
        };
        format!(r"HKCU\Software\{vendor}\NativeMessagingHosts\{HOST_NAME}")
    }
}

fn host_binary() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let name = if cfg!(windows) { "nbs-native-host.exe" } else { "nbs-native-host" };
    let path = exe.parent().ok_or("cannot locate the application directory")?.join(name);
    if path.exists() {
        Ok(path)
    } else {
        Err(format!(
            "{} was not found. Build it with `cargo build --release -p nbs-native-host` and place it next to the app.",
            path.display()
        ))
    }
}

/// Writes the host manifest for `browser` and returns its path.
pub fn install(browser: &str) -> Result<PathBuf, String> {
    let browser = Browser::parse(browser)?;
    let host = host_binary()?;
    let mut manifest = json!({
        "name": HOST_NAME,
        "description": "NBS Download Manager bridge",
        "path": host,
        "type": "stdio",
    });
    if browser == Browser::Firefox {
        manifest["allowed_extensions"] = json!([FIREFOX_EXTENSION_ID]);
    } else {
        manifest["allowed_origins"] = json!([format!("chrome-extension://{CHROME_EXTENSION_ID}/")]);
    }

    let dir = browser.manifest_dir().ok_or("cannot determine the browser configuration directory")?;
    fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    #[cfg(windows)]
    let file = dir.join(format!("{HOST_NAME}.{}.json", if browser == Browser::Firefox { "firefox" } else { "chromium" }));
    #[cfg(not(windows))]
    let file = dir.join(format!("{HOST_NAME}.json"));
    fs::write(&file, serde_json::to_vec_pretty(&manifest).expect("manifest serializes")).map_err(|e| format!("{}: {e}", file.display()))?;

    #[cfg(windows)]
    {
        let status = std::process::Command::new("reg")
            .args(["add", &browser.registry_key(), "/ve", "/t", "REG_SZ", "/d"])
            .arg(&file)
            .arg("/f")
            .status()
            .map_err(|e| format!("failed to run reg.exe: {e}"))?;
        if !status.success() {
            return Err("failed to register the native messaging host in the registry".into());
        }
    }
    Ok(file)
}
