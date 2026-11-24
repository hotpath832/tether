//! On-disk state of the update server (all under the data folder):
//!
//!   server.json   listen address and optional TLS cert/key paths (edit by hand)
//!   auth.json     Argon2 hash of the admin password, SHA-256 of the upload token
//!   catalog.json  releases and which one is live
//!   stats.json    daily update-check and download counts (no IPs kept)
//!   releases/<version>/<installer files>
use argon2::{
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    cmp::Ordering,
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering as AtomicOrdering},
        Mutex, RwLock,
    },
};

/// Tether's release signing key (public half). Must match PUBLIC_KEY in
/// src/tether_update.rs of the Tether client.
const PUBLIC_KEY: &str = "eI8l+hoK0TuL50cjR+q3U67KLuX0f75SZnlikcZDpnU=";
/// Must match SIGN_DOMAIN in src/tether_update.rs and res/tether_release.py.
const SIGN_DOMAIN: &str = "tether-update-v1";
const STATS_KEEP_DAYS: i64 = 400;

#[derive(Serialize, Deserialize, Clone)]
pub struct ServerConfig {
    pub listen: String,
    pub tls_cert: Option<String>,
    pub tls_key: Option<String>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: "0.0.0.0:8080".into(),
            tls_cert: None,
            tls_key: None,
        }
    }
}

#[derive(Serialize, Deserialize, Default, Clone)]
struct Auth {
    admin_hash: String,
    token_hash: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct PlatformFile {
    pub file: String,
    pub sha256: String,
    pub signature: String,
    pub size: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ReleaseMeta {
    pub version: String,
    pub notes: String,
    pub uploaded_at: String,
    pub platforms: BTreeMap<String, PlatformFile>,
}

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct Catalog {
    pub live: Option<String>,
    pub releases: BTreeMap<String, ReleaseMeta>,
}

#[derive(Serialize, Deserialize, Default)]
struct DayStats {
    /// "client version|platform" -> number of update checks
    checks: BTreeMap<String, u64>,
    /// file name -> number of downloads
    downloads: BTreeMap<String, u64>,
}

#[derive(Serialize, Deserialize, Default)]
struct Stats {
    days: BTreeMap<String, DayStats>,
}

pub struct Store {
    dir: PathBuf,
    config: ServerConfig,
    auth: RwLock<Auth>,
    catalog: RwLock<Catalog>,
    stats: Mutex<Stats>,
    stats_dirty: AtomicBool,
}

#[derive(Default)]
pub struct Hasher(Sha256, u64);

impl Hasher {
    pub fn update(&mut self, b: &[u8]) {
        self.0.update(b);
        self.1 += b.len() as u64;
    }
    /// (sha256 hex, size)
    pub fn finish(self) -> (String, u64) {
        (hex::encode(self.0.finalize()), self.1)
    }
}

pub fn random_hex(bytes: usize) -> String {
    let mut b = vec![0u8; bytes];
    rand::thread_rng().fill_bytes(&mut b);
    hex::encode(b)
}

pub fn valid_file_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 200
        && !n.starts_with('.')
        && n != "latest.json"
        && n.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '+'))
}

fn valid_version(v: &str) -> bool {
    let (main, pre) = v.split_once('-').unwrap_or((v, ""));
    !main.is_empty()
        && v.len() <= 40
        && main.split('.').all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
        && pre
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.')
}

pub fn cmp_version(a: &str, b: &str) -> Ordering {
    let nums = |v: &str| -> Vec<u64> {
        v.split('-')
            .next()
            .unwrap_or_default()
            .split('.')
            .map(|p| p.parse().unwrap_or(0))
            .collect()
    };
    let (na, nb) = (nums(a), nums(b));
    for i in 0..na.len().max(nb.len()) {
        let o = na.get(i).unwrap_or(&0).cmp(nb.get(i).unwrap_or(&0));
        if o != Ordering::Equal {
            return o;
        }
    }
    // 1.5.0-beta < 1.5.0
    match (a.contains('-'), b.contains('-')) {
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
        _ => a.cmp(b),
    }
}

fn signed_message(platform: &str, version: &str, file: &str, sha256: &str) -> Vec<u8> {
    format!("{SIGN_DOMAIN}|{platform}|{version}|{file}|{sha256}").into_bytes()
}

fn verify_signature(platform: &str, version: &str, f: &PlatformFile) -> Result<(), String> {
    let pk: [u8; 32] = B64
        .decode(PUBLIC_KEY)
        .ok()
        .and_then(|b| b.try_into().ok())
        .ok_or("bad built-in key")?;
    let pk = VerifyingKey::from_bytes(&pk).map_err(|e| e.to_string())?;
    let sig: [u8; 64] = B64
        .decode(f.signature.trim())
        .ok()
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| format!("{platform}: the signature is not valid base64 Ed25519"))?;
    pk.verify(
        &signed_message(platform, version, &f.file, &f.sha256),
        &Signature::from_bytes(&sig),
    )
    .map_err(|_| {
        format!(
            "{platform}: {} is not signed with Tether's release key (sign it with res/tether_release.py)",
            f.file
        )
    })
}

fn write_json<T: Serialize>(path: &Path, v: &T) -> std::io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(v)?)?;
    std::fs::rename(&tmp, path)
}

fn read_json<T: for<'a> Deserialize<'a> + Default>(path: &Path) -> std::io::Result<T> {
    match std::fs::read(path) {
        Ok(b) => serde_json::from_slice(&b)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{}: {e}", path.display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(e),
    }
}

fn today() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

impl Store {
    pub fn open(dir: &Path) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir.join("releases"))?;
        std::fs::create_dir_all(dir.join("tmp"))?;
        let cfg_path = dir.join("server.json");
        let config: ServerConfig = if cfg_path.exists() {
            read_json(&cfg_path)?
        } else {
            let c = ServerConfig::default();
            write_json(&cfg_path, &c)?;
            c
        };
        // Leftovers of interrupted uploads.
        if let Ok(rd) = std::fs::read_dir(dir.join("tmp")) {
            for e in rd.flatten() {
                let _ = std::fs::remove_dir_all(e.path());
            }
        }
        Ok(Self {
            dir: dir.to_owned(),
            config,
            auth: RwLock::new(read_json(&dir.join("auth.json"))?),
            catalog: RwLock::new(read_json(&dir.join("catalog.json"))?),
            stats: Mutex::new(read_json(&dir.join("stats.json"))?),
            stats_dirty: AtomicBool::new(false),
        })
    }

    pub fn server_config(&self) -> ServerConfig {
        self.config.clone()
    }

    // ------------------------------------------------------------ auth

    fn save_auth(&self, a: &Auth) -> std::io::Result<()> {
        write_json(&self.dir.join("auth.json"), a)
    }

    /// Creates a random admin password on first run and returns it.
    pub fn ensure_admin_password(&self) -> std::io::Result<Option<String>> {
        if !self.auth.read().unwrap().admin_hash.is_empty() {
            return Ok(None);
        }
        let pw = random_hex(9);
        self.set_password(&pw)?;
        std::fs::write(
            self.dir.join("INITIAL_ADMIN_PASSWORD.txt"),
            format!("{pw}\r\nChange it in the dashboard (Settings); this file is then deleted.\r\n"),
        )?;
        Ok(Some(pw))
    }

    pub fn has_initial_password_file(&self) -> bool {
        self.dir.join("INITIAL_ADMIN_PASSWORD.txt").exists()
    }

    pub fn set_password(&self, pw: &str) -> std::io::Result<()> {
        let salt = SaltString::generate(&mut rand::rngs::OsRng);
        let hash = Argon2::default()
            .hash_password(pw.as_bytes(), &salt)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?
            .to_string();
        let mut a = self.auth.write().unwrap();
        let first = a.admin_hash.is_empty();
        a.admin_hash = hash;
        self.save_auth(&a)?;
        if !first {
            let _ = std::fs::remove_file(self.dir.join("INITIAL_ADMIN_PASSWORD.txt"));
        }
        Ok(())
    }

    pub fn check_password(&self, pw: &str) -> bool {
        let hash = self.auth.read().unwrap().admin_hash.clone();
        let Ok(parsed) = PasswordHash::new(&hash) else {
            return false;
        };
        Argon2::default()
            .verify_password(pw.as_bytes(), &parsed)
            .is_ok()
    }

    /// Replaces the upload token (for `tether_release.py upload`).
    pub fn new_token(&self) -> std::io::Result<String> {
        let token = format!("tus_{}", random_hex(24));
        let mut a = self.auth.write().unwrap();
        a.token_hash = hex::encode(Sha256::digest(token.as_bytes()));
        self.save_auth(&a)?;
        Ok(token)
    }

    pub fn check_token(&self, token: &str) -> bool {
        let want = self.auth.read().unwrap().token_hash.clone();
        if want.is_empty() || token.is_empty() {
            return false;
        }
        let got = hex::encode(Sha256::digest(token.as_bytes()));
        // Compare digests in constant time.
        got.len() == want.len()
            && got
                .bytes()
                .zip(want.bytes())
                .fold(0u8, |acc, (x, y)| acc | (x ^ y))
                == 0
    }

    // ------------------------------------------------------------ releases

    pub fn catalog(&self) -> Catalog {
        self.catalog.read().unwrap().clone()
    }

    fn save_catalog(&self, c: &Catalog) -> Result<(), String> {
        write_json(&self.dir.join("catalog.json"), c).map_err(|e| e.to_string())
    }

    /// latest.json for clients, built from the live release.
    pub fn live_latest_json(&self) -> Option<Value> {
        let c = self.catalog.read().unwrap();
        let r = c.releases.get(c.live.as_ref()?)?;
        let platforms: serde_json::Map<String, Value> = r
            .platforms
            .iter()
            .map(|(p, f)| {
                (
                    p.clone(),
                    json!({ "file": f.file, "sha256": f.sha256, "signature": f.signature, "size": f.size }),
                )
            })
            .collect();
        Some(json!({ "version": r.version, "notes": r.notes, "platforms": platforms }))
    }

    /// Where an installer lives, if any release has it.
    pub fn file_path(&self, name: &str) -> Option<PathBuf> {
        if !valid_file_name(name) {
            return None;
        }
        let c = self.catalog.read().unwrap();
        c.releases
            .values()
            .find(|r| r.platforms.values().any(|f| f.file == name))
            .map(|r| self.dir.join("releases").join(&r.version).join(name))
    }

    pub fn new_tmp_dir(&self) -> std::io::Result<PathBuf> {
        let p = self.dir.join("tmp").join(random_hex(8));
        std::fs::create_dir_all(&p)?;
        Ok(p)
    }

    /// Validate an uploaded release (signatures, checksums) and store it.
    /// `files`: uploaded file name -> (sha256, size), stored in `tmp`.
    pub fn add_release(
        &self,
        tmp: &Path,
        latest: &[u8],
        files: &HashMap<String, (String, u64)>,
    ) -> Result<ReleaseMeta, String> {
        let v: Value = serde_json::from_slice(latest).map_err(|e| format!("latest.json: {e}"))?;
        let version = v["version"].as_str().unwrap_or_default().trim().to_owned();
        if !valid_version(&version) {
            return Err(format!("latest.json: invalid version '{version}'"));
        }
        let Some(plats) = v["platforms"].as_object().filter(|m| !m.is_empty()) else {
            return Err("latest.json has no platforms".into());
        };
        let mut platforms = BTreeMap::new();
        for (p, e) in plats {
            let file = e["file"].as_str().unwrap_or_default().to_owned();
            if !valid_file_name(&file) {
                return Err(format!("{p}: invalid file name '{file}'"));
            }
            let Some((sha, size)) = files.get(&file) else {
                return Err(format!("{p}: {file} was not uploaded"));
            };
            let f = PlatformFile {
                file: file.clone(),
                sha256: e["sha256"].as_str().unwrap_or_default().to_lowercase(),
                signature: e["signature"].as_str().unwrap_or_default().to_owned(),
                size: *size,
            };
            if &f.sha256 != sha {
                return Err(format!("{p}: {file} does not match the checksum in latest.json"));
            }
            verify_signature(p, &version, &f)?;
            platforms.insert(p.clone(), f);
        }

        let mut c = self.catalog.write().unwrap();
        if c.releases.contains_key(&version) {
            return Err(format!(
                "release {version} already exists; delete it first or use a new version"
            ));
        }
        for f in platforms.values() {
            if let Some(other) = c
                .releases
                .values()
                .find(|r| r.platforms.values().any(|o| o.file == f.file))
            {
                return Err(format!("{} already belongs to release {}", f.file, other.version));
            }
        }
        let dest = self.dir.join("releases").join(&version);
        std::fs::create_dir_all(&dest).map_err(|e| e.to_string())?;
        for f in platforms.values() {
            std::fs::rename(tmp.join(&f.file), dest.join(&f.file)).map_err(|e| e.to_string())?;
        }
        let meta = ReleaseMeta {
            version: version.clone(),
            notes: v["notes"].as_str().unwrap_or_default().to_owned(),
            uploaded_at: chrono::Local::now().format("%Y-%m-%d %H:%M").to_string(),
            platforms,
        };
        c.releases.insert(version, meta.clone());
        self.save_catalog(&c)?;
        Ok(meta)
    }

    pub fn set_live(&self, version: Option<&str>) -> Result<(), String> {
        let mut c = self.catalog.write().unwrap();
        if let Some(v) = version {
            if !c.releases.contains_key(v) {
                return Err(format!("no release {v}"));
            }
        }
        c.live = version.map(str::to_owned);
        self.save_catalog(&c)
    }

    pub fn delete_release(&self, version: &str) -> Result<(), String> {
        let mut c = self.catalog.write().unwrap();
        if c.live.as_deref() == Some(version) {
            return Err("this release is live; make another one live or unpublish first".into());
        }
        if c.releases.remove(version).is_none() {
            return Err(format!("no release {version}"));
        }
        self.save_catalog(&c)?;
        let _ = std::fs::remove_dir_all(self.dir.join("releases").join(version));
        Ok(())
    }

    // ------------------------------------------------------------ stats

    pub fn record_check(&self, version: &str, platform: &str) {
        let clean = |s: &str| -> String {
            s.chars()
                .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
                .take(40)
                .collect()
        };
        let key = format!("{}|{}", clean(version), clean(platform));
        let mut s = self.stats.lock().unwrap();
        *s.days.entry(today()).or_default().checks.entry(key).or_default() += 1;
        self.stats_dirty.store(true, AtomicOrdering::Relaxed);
    }

    pub fn record_download(&self, file: &str) {
        let mut s = self.stats.lock().unwrap();
        *s.days
            .entry(today())
            .or_default()
            .downloads
            .entry(file.to_owned())
            .or_default() += 1;
        self.stats_dirty.store(true, AtomicOrdering::Relaxed);
    }

    pub fn flush_stats(&self) {
        if !self.stats_dirty.swap(false, AtomicOrdering::Relaxed) {
            return;
        }
        let mut s = self.stats.lock().unwrap();
        let cutoff = (chrono::Local::now() - chrono::Duration::days(STATS_KEEP_DAYS))
            .format("%Y-%m-%d")
            .to_string();
        s.days.retain(|d, _| *d >= cutoff);
        if let Err(e) = write_json(&self.dir.join("stats.json"), &*s) {
            eprintln!("cannot save stats: {e}");
            self.stats_dirty.store(true, AtomicOrdering::Relaxed);
        }
    }

    pub fn stats_summary(&self, days: u32) -> Value {
        let s = self.stats.lock().unwrap();
        let now = chrono::Local::now();
        let mut per_day = vec![];
        let mut by_version: BTreeMap<String, u64> = BTreeMap::new();
        let mut by_platform: BTreeMap<String, u64> = BTreeMap::new();
        let mut by_file: BTreeMap<String, u64> = BTreeMap::new();
        let (mut checks_total, mut downloads_total) = (0u64, 0u64);
        for i in (0..days as i64).rev() {
            let d = (now - chrono::Duration::days(i)).format("%Y-%m-%d").to_string();
            let (mut c, mut dl) = (0u64, 0u64);
            if let Some(ds) = s.days.get(&d) {
                for (k, n) in &ds.checks {
                    let (v, p) = k.split_once('|').unwrap_or((k, ""));
                    *by_version.entry(v.to_owned()).or_default() += n;
                    *by_platform.entry(p.to_owned()).or_default() += n;
                    c += n;
                }
                for (f, n) in &ds.downloads {
                    *by_file.entry(f.clone()).or_default() += n;
                    dl += n;
                }
            }
            checks_total += c;
            downloads_total += dl;
            per_day.push(json!({ "date": d, "checks": c, "downloads": dl }));
        }
        // Today's checks by client version: roughly how many installs run what.
        let mut today_versions: BTreeMap<String, u64> = BTreeMap::new();
        if let Some(ds) = s.days.get(&today()) {
            for (k, n) in &ds.checks {
                let v = k.split_once('|').map(|x| x.0).unwrap_or(k);
                *today_versions.entry(v.to_owned()).or_default() += n;
            }
        }
        json!({
            "days": days,
            "checks_total": checks_total,
            "downloads_total": downloads_total,
            "per_day": per_day,
            "by_version": by_version,
            "by_platform": by_platform,
            "by_file": by_file,
            "today_versions": today_versions,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert_eq!(cmp_version("1.5.0", "1.4.8"), Ordering::Greater);
        assert_eq!(cmp_version("1.10.0", "1.9.9"), Ordering::Greater);
        assert_eq!(cmp_version("1.5.0-beta", "1.5.0"), Ordering::Less);
        assert!(valid_version("1.5.0") && valid_version("2.0.0-rc1"));
        assert!(!valid_version("1..0") && !valid_version("../x") && !valid_version(""));
    }

    #[test]
    fn file_names() {
        assert!(valid_file_name("tether-1.5.0-install.exe"));
        assert!(!valid_file_name("../a.exe") && !valid_file_name("a\\b") && !valid_file_name("latest.json"));
    }
}
