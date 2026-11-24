//! Tether update server.
//!
//! Public (what Tether clients use, set as "Update server" in Tether's settings):
//!   GET /latest.json        the live release (version, notes, per-platform file/sha256/signature)
//!   GET /<installer file>   installer downloads
//!
//! Admin (password login or upload token):
//!   GET  /admin             dashboard
//!   /admin/api/...          releases, upload, make live / unpublish / delete, stats, settings
//!
//! Every uploaded release must be signed with Tether's release key (same key as
//! PUBLIC_KEY in src/tether_update.rs); the server refuses anything else, so a
//! stolen admin password still can't ship an unsigned build.
mod store;

use axum::{
    body::Body,
    extract::{ConnectInfo, DefaultBodyLimit, Multipart, Path, Query, State},
    http::{header, HeaderMap, HeaderValue, Method, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use store::{Catalog, Store};
use tokio::io::AsyncWriteExt;

const DASHBOARD: &str = include_str!("dashboard.html");
const SESSION_COOKIE: &str = "tether_admin";
const SESSION_TTL: Duration = Duration::from_secs(12 * 3600);
const MAX_UPLOAD: usize = 2 * 1024 * 1024 * 1024; // 2 GB per upload request

struct App {
    store: Store,
    tls: bool,
    sessions: Mutex<HashMap<String, Instant>>,
    /// Per-IP failed logins: (count, locked until).
    login_fails: Mutex<HashMap<std::net::IpAddr, (u32, Option<Instant>)>>,
}

type Shared = Arc<App>;

struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

impl<E: std::fmt::Display> From<E> for ApiError {
    fn from(e: E) -> Self {
        ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    }
}

fn bad(msg: impl Into<String>) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, msg.into())
}

fn log(msg: impl AsRef<str>) {
    println!(
        "[{}] {}",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
        msg.as_ref()
    );
}

// ---------------------------------------------------------------- auth

fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v.to_owned())
}

fn is_authed(app: &App, headers: &HeaderMap) -> bool {
    if let Some(tok) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
    {
        return app.store.check_token(tok.trim());
    }
    if let Some(sid) = cookie_value(headers, SESSION_COOKIE) {
        let mut s = app.sessions.lock().unwrap();
        s.retain(|_, t| t.elapsed() < SESSION_TTL);
        return s.contains_key(&sid);
    }
    false
}

fn require_auth(app: &App, headers: &HeaderMap) -> Result<(), ApiError> {
    if is_authed(app, headers) {
        Ok(())
    } else {
        Err(ApiError(StatusCode::UNAUTHORIZED, "login required".into()))
    }
}

#[derive(Deserialize)]
struct LoginReq {
    password: String,
}

async fn login(
    State(app): State<Shared>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(req): Json<LoginReq>,
) -> Result<Response, ApiError> {
    let ip = addr.ip();
    {
        let fails = app.login_fails.lock().unwrap();
        if let Some((_, Some(until))) = fails.get(&ip) {
            if Instant::now() < *until {
                return Err(ApiError(
                    StatusCode::TOO_MANY_REQUESTS,
                    "too many failed logins, try again in a minute".into(),
                ));
            }
        }
    }
    // Argon2 is slow on purpose; keep it off the async workers.
    let ok = {
        let app = app.clone();
        tokio::task::spawn_blocking(move || app.store.check_password(&req.password)).await?
    };
    if !ok {
        let mut fails = app.login_fails.lock().unwrap();
        let e = fails.entry(ip).or_insert((0, None));
        e.0 += 1;
        if e.0 >= 5 {
            *e = (0, Some(Instant::now() + Duration::from_secs(60)));
        }
        log(format!("failed admin login from {ip}"));
        return Err(ApiError(StatusCode::UNAUTHORIZED, "wrong password".into()));
    }
    app.login_fails.lock().unwrap().remove(&ip);
    let sid = store::random_hex(32);
    app.sessions.lock().unwrap().insert(sid.clone(), Instant::now());
    log(format!("admin login from {ip}"));
    let cookie = format!(
        "{SESSION_COOKIE}={sid}; Path=/admin; HttpOnly; SameSite=Strict; Max-Age={}{}",
        SESSION_TTL.as_secs(),
        if app.tls { "; Secure" } else { "" }
    );
    let mut resp = Json(json!({ "ok": true })).into_response();
    resp.headers_mut()
        .insert(header::SET_COOKIE, HeaderValue::from_str(&cookie)?);
    Ok(resp)
}

async fn logout(State(app): State<Shared>, headers: HeaderMap) -> Response {
    if let Some(sid) = cookie_value(&headers, SESSION_COOKIE) {
        app.sessions.lock().unwrap().remove(&sid);
    }
    let mut resp = Json(json!({ "ok": true })).into_response();
    resp.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_static("tether_admin=; Path=/admin; Max-Age=0"),
    );
    resp
}

async fn me(State(app): State<Shared>, headers: HeaderMap) -> Json<Value> {
    Json(json!({
        "authed": is_authed(&app, &headers),
        "initial_password": app.store.has_initial_password_file(),
        "tls": app.tls,
    }))
}

#[derive(Deserialize)]
struct PasswordReq {
    old: String,
    new: String,
}

async fn change_password(
    State(app): State<Shared>,
    headers: HeaderMap,
    Json(req): Json<PasswordReq>,
) -> Result<Json<Value>, ApiError> {
    require_auth(&app, &headers)?;
    if req.new.chars().count() < 10 {
        return Err(bad("the new password must be at least 10 characters"));
    }
    let app2 = app.clone();
    let ok = tokio::task::spawn_blocking(move || {
        if !app2.store.check_password(&req.old) {
            return Ok(false);
        }
        app2.store.set_password(&req.new).map(|_| true)
    })
    .await??;
    if !ok {
        return Err(ApiError(StatusCode::FORBIDDEN, "current password is wrong".into()));
    }
    // Sign out every other session.
    let keep = cookie_value(&headers, SESSION_COOKIE);
    app.sessions
        .lock()
        .unwrap()
        .retain(|k, _| Some(k) == keep.as_ref());
    log("admin password changed");
    Ok(Json(json!({ "ok": true })))
}

async fn new_token(State(app): State<Shared>, headers: HeaderMap) -> Result<Json<Value>, ApiError> {
    require_auth(&app, &headers)?;
    let token = app.store.new_token()?;
    log("upload token regenerated");
    Ok(Json(json!({ "token": token })))
}

// ---------------------------------------------------------------- public

#[derive(Deserialize)]
struct CheckQuery {
    version: Option<String>,
    platform: Option<String>,
}

async fn latest_json(State(app): State<Shared>, Query(q): Query<CheckQuery>) -> Response {
    app.store.record_check(
        q.version.as_deref().unwrap_or("unknown"),
        q.platform.as_deref().unwrap_or("unknown"),
    );
    match app.store.live_latest_json() {
        Some(v) => (
            [
                (header::CACHE_CONTROL, "no-cache"),
                (header::CONTENT_TYPE, "application/json"),
            ],
            serde_json::to_string_pretty(&v).unwrap_or_default(),
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "no release is published").into_response(),
    }
}

async fn download(State(app): State<Shared>, method: Method, Path(name): Path<String>) -> Response {
    let Some(path) = app.store.file_path(&name) else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    let Ok(file) = tokio::fs::File::open(&path).await else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    let len = file.metadata().await.map(|m| m.len()).unwrap_or(0);
    if method == Method::GET {
        app.store.record_download(&name);
    }
    let body = Body::from_stream(tokio_util::io::ReaderStream::new(file));
    (
        [
            (header::CONTENT_TYPE, "application/octet-stream".to_owned()),
            (header::CONTENT_LENGTH, len.to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{name}\""),
            ),
        ],
        body,
    )
        .into_response()
}

// ---------------------------------------------------------------- admin api

async fn list_releases(State(app): State<Shared>, headers: HeaderMap) -> Result<Json<Value>, ApiError> {
    require_auth(&app, &headers)?;
    let c: Catalog = app.store.catalog();
    let mut list: Vec<_> = c.releases.values().cloned().collect();
    list.sort_by(|a, b| store::cmp_version(&b.version, &a.version));
    Ok(Json(json!({ "live": c.live, "releases": list })))
}

#[derive(Deserialize)]
struct UploadQuery {
    live: Option<bool>,
}

async fn upload(
    State(app): State<Shared>,
    headers: HeaderMap,
    Query(q): Query<UploadQuery>,
    mut mp: Multipart,
) -> Result<Json<Value>, ApiError> {
    require_auth(&app, &headers)?;
    let tmp = app.store.new_tmp_dir()?;
    let res = async {
        let mut latest: Option<Vec<u8>> = None;
        let mut files = HashMap::new();
        while let Some(mut field) = mp.next_field().await.map_err(|e| bad(e.to_string()))? {
            let name = field
                .file_name()
                .map(|s| s.rsplit(['/', '\\']).next().unwrap_or_default().to_owned())
                .unwrap_or_default();
            if name.is_empty() {
                continue;
            }
            if name == "latest.json" {
                latest = Some(field.bytes().await.map_err(|e| bad(e.to_string()))?.to_vec());
                continue;
            }
            if !store::valid_file_name(&name) {
                return Err(bad(format!("invalid file name: {name}")));
            }
            let path = tmp.join(&name);
            let mut f = tokio::fs::File::create(&path).await?;
            let mut hasher = store::Hasher::default();
            while let Some(chunk) = field.chunk().await.map_err(|e| bad(e.to_string()))? {
                hasher.update(&chunk);
                f.write_all(&chunk).await?;
            }
            f.flush().await?;
            files.insert(name, hasher.finish());
        }
        let Some(latest) = latest else {
            return Err(bad("latest.json is missing from the upload"));
        };
        let meta = app.store.add_release(&tmp, &latest, &files).map_err(bad)?;
        if q.live.unwrap_or(false) {
            app.store.set_live(Some(&meta.version)).map_err(bad)?;
        }
        log(format!(
            "release {} uploaded ({} platform(s)){}",
            meta.version,
            meta.platforms.len(),
            if q.live.unwrap_or(false) { ", now live" } else { "" }
        ));
        Ok(Json(json!({ "ok": true, "release": meta })))
    }
    .await;
    let _ = tokio::fs::remove_dir_all(&tmp).await;
    res
}

async fn make_live(
    State(app): State<Shared>,
    headers: HeaderMap,
    Path(version): Path<String>,
) -> Result<Json<Value>, ApiError> {
    require_auth(&app, &headers)?;
    app.store.set_live(Some(&version)).map_err(bad)?;
    log(format!("release {version} is now live"));
    Ok(Json(json!({ "ok": true })))
}

async fn unpublish(State(app): State<Shared>, headers: HeaderMap) -> Result<Json<Value>, ApiError> {
    require_auth(&app, &headers)?;
    app.store.set_live(None).map_err(bad)?;
    log("live release unpublished; clients see no update");
    Ok(Json(json!({ "ok": true })))
}

async fn delete_release(
    State(app): State<Shared>,
    headers: HeaderMap,
    Path(version): Path<String>,
) -> Result<Json<Value>, ApiError> {
    require_auth(&app, &headers)?;
    app.store.delete_release(&version).map_err(bad)?;
    log(format!("release {version} deleted"));
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct StatsQuery {
    days: Option<u32>,
}

async fn stats(
    State(app): State<Shared>,
    headers: HeaderMap,
    Query(q): Query<StatsQuery>,
) -> Result<Json<Value>, ApiError> {
    require_auth(&app, &headers)?;
    Ok(Json(app.store.stats_summary(q.days.unwrap_or(30).clamp(1, 365))))
}

// ---------------------------------------------------------------- main

fn usage() -> ! {
    eprintln!(
        "Tether update server

USAGE:
  tether-update-server [--data <dir>]              run the server
  tether-update-server [--data <dir>] set-password <new password>
  tether-update-server [--data <dir>] new-token    print a new upload token

The data folder (default: 'data' next to the exe) holds server.json (listen
address, optional TLS cert/key), the releases and the stats. On first run an
admin password is generated and written to data\\INITIAL_ADMIN_PASSWORD.txt."
    );
    std::process::exit(2)
}

#[tokio::main]
async fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut data: PathBuf = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.join("data")))
        .unwrap_or_else(|| PathBuf::from("data"));
    if let Some(i) = args.iter().position(|a| a == "--data") {
        if i + 1 >= args.len() {
            usage();
        }
        data = PathBuf::from(args.remove(i + 1));
        args.remove(i);
    }
    if args.iter().any(|a| a == "-h" || a == "--help") {
        usage();
    }

    let store = match Store::open(&data) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Cannot open data folder {}: {e}", data.display());
            std::process::exit(1);
        }
    };

    match args.first().map(String::as_str) {
        Some("set-password") => {
            let Some(pw) = args.get(1) else { usage() };
            if pw.chars().count() < 10 {
                eprintln!("The password must be at least 10 characters.");
                std::process::exit(1);
            }
            store.set_password(pw).expect("save password");
            println!("Admin password changed.");
            return;
        }
        Some("new-token") => {
            println!("{}", store.new_token().expect("save token"));
            return;
        }
        Some(other) => {
            eprintln!("Unknown command: {other}");
            usage();
        }
        None => {}
    }

    if let Some(pw) = store.ensure_admin_password().expect("init admin password") {
        log(format!(
            "First run: admin password is {pw} (also in {}). Change it in the dashboard.",
            data.join("INITIAL_ADMIN_PASSWORD.txt").display()
        ));
    }

    let cfg = store.server_config();
    let addr: SocketAddr = match cfg.listen.parse() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("Bad listen address '{}' in server.json: {e}", cfg.listen);
            std::process::exit(1);
        }
    };
    let tls = cfg.tls_cert.is_some() && cfg.tls_key.is_some();
    let app: Shared = Arc::new(App {
        store,
        tls,
        sessions: Default::default(),
        login_fails: Default::default(),
    });

    // Flush stats to disk now and then.
    {
        let app = app.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(30)).await;
                app.store.flush_stats();
            }
        });
    }

    let router = Router::new()
        .route("/", get(|| async { Redirect::to("/admin") }))
        .route("/latest.json", get(latest_json))
        .route("/admin", get(|| async { Html(DASHBOARD) }))
        .route("/admin/", get(|| async { Redirect::to("/admin") }))
        .route("/admin/api/me", get(me))
        .route("/admin/api/login", post(login))
        .route("/admin/api/logout", post(logout))
        .route("/admin/api/password", post(change_password))
        .route("/admin/api/token", post(new_token))
        .route("/admin/api/releases", get(list_releases))
        .route(
            "/admin/api/releases",
            post(upload).layer(DefaultBodyLimit::max(MAX_UPLOAD)),
        )
        .route("/admin/api/releases/:version/live", post(make_live))
        .route("/admin/api/releases/:version/delete", post(delete_release))
        .route("/admin/api/unpublish", post(unpublish))
        .route("/admin/api/stats", get(stats))
        .route("/:file", get(download))
        .with_state(app.clone());
    let service = router.into_make_service_with_connect_info::<SocketAddr>();

    let handle = axum_server::Handle::new();
    {
        let handle = handle.clone();
        let app = app.clone();
        tokio::spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            log("stopping");
            app.store.flush_stats();
            handle.graceful_shutdown(Some(Duration::from_secs(5)));
        });
    }

    if tls {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let tls_cfg = match axum_server::tls_rustls::RustlsConfig::from_pem_file(
            cfg.tls_cert.clone().unwrap(),
            cfg.tls_key.clone().unwrap(),
        )
        .await
        {
            Ok(c) => c,
            Err(e) => {
                eprintln!("Cannot load the TLS certificate/key from server.json: {e}");
                std::process::exit(1);
            }
        };
        log(format!("Tether update server on https://{addr} (dashboard: /admin)"));
        if let Err(e) = axum_server::bind_rustls(addr, tls_cfg)
            .handle(handle)
            .serve(service)
            .await
        {
            eprintln!("Server error: {e}");
            std::process::exit(1);
        }
    } else {
        log(format!("Tether update server on http://{addr} (dashboard: /admin)"));
        if let Err(e) = axum_server::bind(addr).handle(handle).serve(service).await {
            eprintln!("Server error: {e}");
            std::process::exit(1);
        }
    }
    app.store.flush_stats();
}
