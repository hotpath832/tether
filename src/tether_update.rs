//! Tether: software updates from the update server set in Settings.
//!
//! The server is any static web host holding `latest.json` plus the
//! installers, as written by `res/tether_release.py`:
//!
//! ```json
//! { "version": "1.4.9", "notes": "...",
//!   "platforms": { "windows-x86_64": {
//!       "file": "tether-1.4.9-install.exe", "sha256": "<hex>", "signature": "<base64>" } } }
//! ```
//!
//! `file` is relative to the server URL (or a full http(s) URL). Each entry is
//! signed with Tether's Ed25519 release key; an installer is only run when its
//! signature matches `PUBLIC_KEY` and the downloaded bytes match `sha256`, so a
//! wrong or hijacked server address can't push code.
use hbb_common::{
    base64, bail,
    config::Config,
    lazy_static, log,
    sodiumoxide::crypto::sign::{self, ed25519},
    ResultType,
};
use sha2::{Digest, Sha256};
use std::{io::Read, path::Path, sync::Mutex};

/// Server option (synced to the service) holding the update server URL.
pub const OPTION_UPDATE_SERVER: &str = "tether-update-server";

/// Public half of Tether's release signing key (raw 32 bytes, base64).
/// The private key stays on the build computer; see res/tether_release.py.
const PUBLIC_KEY: &str = "eI8l+hoK0TuL50cjR+q3U67KLuX0f75SZnlikcZDpnU=";
/// Must match SIGN_DOMAIN in res/tether_release.py.
const SIGN_DOMAIN: &str = "tether-update-v1";

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Release {
    pub version: String,
    pub notes: String,
    /// Full download URL of this platform's installer.
    pub url: String,
    /// Installer file name, the last segment of `url`.
    pub file: String,
    pub sha256: String,
    pub signature: String,
}

lazy_static::lazy_static! {
    /// The newer release found by the last check, if any.
    static ref LATEST: Mutex<Option<Release>> = Default::default();
}

pub fn platform_key() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

/// The configured update server, without a trailing slash ("" = updates off).
pub fn update_server() -> String {
    // The service reads its own config; the UI process asks the service, which
    // owns the server options.
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    let v = if crate::is_server() {
        Config::get_option(OPTION_UPDATE_SERVER)
    } else {
        crate::ipc::get_options()
            .get(OPTION_UPDATE_SERVER)
            .cloned()
            .unwrap_or_default()
    };
    #[cfg(any(target_os = "android", target_os = "ios"))]
    let v = Config::get_option(OPTION_UPDATE_SERVER);
    normalize_server(&v)
}

fn normalize_server(v: &str) -> String {
    let v = v.trim().trim_end_matches('/');
    if v.is_empty() {
        return String::new();
    }
    if v.starts_with("http://") || v.starts_with("https://") {
        v.to_owned()
    } else {
        format!("https://{v}")
    }
}

pub fn latest() -> Option<Release> {
    LATEST.lock().unwrap().clone()
}

fn signed_message(platform: &str, version: &str, file: &str, sha256: &str) -> Vec<u8> {
    format!("{SIGN_DOMAIN}|{platform}|{version}|{file}|{sha256}").into_bytes()
}

fn verify_signature(platform: &str, r: &Release) -> ResultType<()> {
    #[allow(deprecated)]
    let pk = base64::decode(PUBLIC_KEY)?;
    let Some(pk) = ed25519::PublicKey::from_slice(&pk) else {
        bail!("invalid built-in update key");
    };
    #[allow(deprecated)]
    let sig = base64::decode(r.signature.trim())?;
    let Ok(sig) = <[u8; 64]>::try_from(sig.as_slice()) else {
        bail!("invalid update signature");
    };
    let msg = signed_message(platform, &r.version, &r.file, &r.sha256.to_lowercase());
    #[allow(deprecated)]
    if !sign::verify_detached(&ed25519::Signature::new(sig), &msg, &pk) {
        bail!("the update is not signed by Tether's release key");
    }
    Ok(())
}

/// Parse `latest.json` and pick this platform's entry, checking its signature.
fn parse_latest(server: &str, body: &[u8], platform: &str) -> ResultType<Release> {
    let v: serde_json::Value = serde_json::from_slice(body)?;
    let version = v["version"].as_str().unwrap_or_default().trim().to_owned();
    if version.is_empty() {
        bail!("latest.json has no version");
    }
    let p = &v["platforms"][platform];
    if p.is_null() {
        bail!("no {platform} build in latest.json");
    }
    let file_field = p["file"].as_str().unwrap_or_default().trim().to_owned();
    let file = file_field.rsplit('/').next().unwrap_or_default().to_owned();
    if file.is_empty() || file.contains('\\') || file.starts_with('.') {
        bail!("invalid file name in latest.json");
    }
    let url = if file_field.starts_with("http://") || file_field.starts_with("https://") {
        file_field
    } else {
        format!("{server}/{}", file_field.trim_start_matches('/'))
    };
    let r = Release {
        version,
        notes: v["notes"].as_str().unwrap_or_default().to_owned(),
        url,
        file,
        sha256: p["sha256"].as_str().unwrap_or_default().trim().to_lowercase(),
        signature: p["signature"].as_str().unwrap_or_default().to_owned(),
    };
    if r.sha256.len() != 64 || !r.sha256.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("invalid sha256 in latest.json");
    }
    verify_signature(platform, &r)?;
    Ok(r)
}

/// Ask the update server for a newer version. Returns it (also remembered for
/// `latest()` and the home page banner), or None when up to date / no server.
pub fn check() -> ResultType<Option<Release>> {
    check_at(&update_server())
}

fn check_at(server: &str) -> ResultType<Option<Release>> {
    let server = normalize_server(server);
    let found = if server.is_empty() {
        None
    } else {
        // The version/platform query feeds the update server's statistics;
        // plain static hosts ignore it.
        let url = format!(
            "{server}/latest.json?version={}&platform={}",
            crate::VERSION,
            platform_key()
        );
        let client = crate::hbbs_http::create_http_client_with_url(&url);
        let resp = client.get(&url).send()?;
        if !resp.status().is_success() {
            bail!("{url}: HTTP {}", resp.status());
        }
        let body = resp.bytes()?;
        let r = parse_latest(&server, &body, &platform_key())?;
        if hbb_common::get_version_number(&r.version)
            > hbb_common::get_version_number(crate::VERSION)
        {
            log::info!("Tether update available: {} at {}", r.version, r.url);
            Some(r)
        } else {
            None
        }
    };
    *LATEST.lock().unwrap() = found.clone();
    *crate::common::SOFTWARE_UPDATE_URL.lock().unwrap() =
        found.as_ref().map(|r| r.url.clone()).unwrap_or_default();
    Ok(found)
}

fn sha256_file(path: &Path) -> ResultType<String> {
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Check a downloaded installer against the signed release it came from.
/// Must pass before the file is run.
pub fn verify_file(path: &Path, r: &Release) -> ResultType<()> {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    if name != r.file {
        bail!("unexpected update file {name}");
    }
    let actual = sha256_file(path)?;
    if actual != r.sha256 {
        bail!("the downloaded update is damaged or was changed (checksum mismatch)");
    }
    Ok(())
}

/// Verify the installer downloaded from `url` against the last check.
pub fn verify_download(url: &str, path: &Path) -> ResultType<()> {
    let Some(r) = latest() else {
        bail!("no checked update to verify against");
    };
    if r.url != url {
        bail!("the download is not the checked update");
    }
    verify_file(path, &r)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Signed by res/tether_release.py with a throwaway key would need that key
    // here; instead check that tampering is always refused.
    #[test]
    fn rejects_unsigned_and_tampered() {
        let body = br#"{"version":"9.9.9","platforms":{"windows-x86_64":{
            "file":"tether-9.9.9-install.exe",
            "sha256":"0000000000000000000000000000000000000000000000000000000000000000",
            "signature":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=="}}}"#;
        assert!(parse_latest("https://x", body, "windows-x86_64").is_err());
        assert!(parse_latest("https://x", body, "linux-x86_64").is_err());
        let bad_name = br#"{"version":"9.9.9","platforms":{"windows-x86_64":{
            "file":"..","sha256":"00","signature":""}}}"#;
        assert!(parse_latest("https://x", bad_name, "windows-x86_64").is_err());
    }

    // Written by `res/tether_release.py sign` with the real release key for a
    // 21-byte file "tether test installer".
    const SIGNED: &[u8] = br#"{"version": "9.9.9", "platforms": {"windows-x86_64": {
        "file": "tether-9.9.9-install.exe",
        "sha256": "6f35be18b9ff4a8ff5f8d9b44cb820706d9d47392c2ffb61d4bbee920bb405d6",
        "signature": "bxfTo9COtfTqWuGk1vVaKukK8d0bmlDIb78hmAbeL335JwKnDe+e/ABOx2t3XfBuOKk8aVxdpv7WDxOM1T7jAg=="}}}"#;

    #[test]
    fn accepts_release_signed_by_script() {
        let r = parse_latest("https://u.example/t", SIGNED, "windows-x86_64").unwrap();
        assert_eq!(r.version, "9.9.9");
        assert_eq!(r.url, "https://u.example/t/tether-9.9.9-install.exe");

        let dir = std::env::temp_dir().join("tether-update-test");
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("tether-9.9.9-install.exe");
        std::fs::write(&f, b"tether test installer").unwrap();
        verify_file(&f, &r).unwrap();
        std::fs::write(&f, b"tether test installeR").unwrap();
        assert!(verify_file(&f, &r).is_err());
        std::fs::remove_dir_all(&dir).ok();

        // The signature covers the version and platform too.
        let bumped = String::from_utf8_lossy(SIGNED).replace("9.9.9\"", "9.9.10\"");
        assert!(parse_latest("https://u", bumped.as_bytes(), "windows-x86_64").is_err());
    }

    /// End-to-end against a real server: serve a release folder made by
    /// `res/tether_release.py sign --version 9.9.9` and run
    /// `TETHER_UPDATE_TEST_SERVER=http://127.0.0.1:8765 cargo test ... -- --ignored`.
    #[test]
    #[ignore]
    fn live_server_check_and_download() {
        let server = std::env::var("TETHER_UPDATE_TEST_SERVER").unwrap();
        let r = check_at(&server).unwrap().expect("server should offer a newer version");
        assert_eq!(latest().as_ref(), Some(&r));
        let client = crate::hbbs_http::create_http_client_with_url(&r.url);
        let bytes = client.get(&r.url).send().unwrap().bytes().unwrap();
        let path = crate::updater::get_download_file_from_url(&r.url).unwrap();
        std::fs::write(&path, &bytes).unwrap();
        verify_download(&r.url, &path).unwrap();
        std::fs::write(&path, b"tampered").unwrap();
        assert!(verify_download(&r.url, &path).is_err());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn server_normalized() {
        assert_eq!(normalize_server(" example.com/tether/ "), "https://example.com/tether");
        assert_eq!(normalize_server("http://1.2.3.4:8080"), "http://1.2.3.4:8080");
        assert_eq!(normalize_server("  "), "");
    }
}
