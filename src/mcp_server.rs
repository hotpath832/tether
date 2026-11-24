//! Tether: lets Claude (Claude Code, Claude Desktop, any MCP client) see and
//! operate a remote computer through an open Tether session.
//!
//! * MCP over HTTP on 127.0.0.1 (`/mcp`, JSON-RPC, Bearer token), running in
//!   the Tether app that owns the sessions. Off by default
//!   (Settings > General > Claude control).
//! * `tether.exe --mcp`: a stdio bridge for MCP clients that launch their tools
//!   as programs (Claude Desktop); it forwards each message to that endpoint.
//!
//! Tools: list_sessions, screenshot, click, move_mouse, drag, scroll,
//! type_text, press_keys, wait. Pointer coordinates are pixels of the latest
//! screenshot of that session; they are mapped to the remote display. All input
//! is human-like (see `human_input` and `auto_type`).
use crate::{
    flutter::{sessions, FlutterSession},
    human_input::{Button, HumanInput},
};
use hbb_common::{
    config::LocalConfig,
    log,
    rand::{self, Rng},
    tokio::{
        self,
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
        sync::oneshot,
    },
};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    io::{BufRead, Read, Write},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

pub const OPT_ENABLED: &str = "tether-mcp-enabled";
pub const OPT_TOKEN: &str = "tether-mcp-token";
pub const OPT_PORT: &str = "tether-mcp-port";
pub const OPT_WPM: &str = "tether-mcp-wpm";
pub const OPT_MISTAKES: &str = "tether-mcp-mistakes";
pub const DEFAULT_PORT: u16 = 21190;
const PROTOCOL_VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];
const MAX_BODY: usize = 4 * 1024 * 1024;

struct Running {
    port: u16,
    shutdown: Option<oneshot::Sender<()>>,
}

/// What the last screenshot of a peer showed, to map coordinates back.
#[derive(Clone, Copy)]
struct Shot {
    display: usize,
    /// Display rectangle in remote coordinates.
    rect: (f64, f64, f64, f64),
    w: f64,
    h: f64,
}

lazy_static::lazy_static! {
    static ref SERVER: Mutex<Option<Running>> = Default::default();
    static ref LAST_ERROR: Mutex<String> = Default::default();
    static ref SHOTS: Mutex<HashMap<String, Shot>> = Default::default();
    /// Pointer position we last put the remote pointer at, per peer.
    static ref POINTER: Mutex<HashMap<String, (f64, f64)>> = Default::default();
    /// Stops the action in progress (Claude control turned off).
    static ref STOP: Arc<AtomicBool> = Default::default();
    /// One action at a time.
    static ref ACTION: Mutex<()> = Default::default();
}

// ---------------------------------------------------------------- settings

pub fn port() -> u16 {
    LocalConfig::get_option(OPT_PORT)
        .parse()
        .ok()
        .filter(|p| *p > 1024)
        .unwrap_or(DEFAULT_PORT)
}

pub fn token() -> String {
    let t = LocalConfig::get_option(OPT_TOKEN);
    if !t.is_empty() {
        return t;
    }
    new_token()
}

pub fn new_token() -> String {
    let mut b = [0u8; 24];
    rand::thread_rng().fill(&mut b[..]);
    let t = format!("tmcp_{}", b.iter().map(|x| format!("{x:02x}")).collect::<String>());
    LocalConfig::set_option(OPT_TOKEN.to_owned(), t.clone());
    t
}

fn enabled() -> bool {
    LocalConfig::get_option(OPT_ENABLED) == "Y"
}

/// Status for the settings page.
pub fn info() -> Value {
    let running = SERVER.lock().unwrap().as_ref().map(|r| r.port);
    json!({
        "enabled": enabled(),
        "running": running.is_some(),
        "port": running.unwrap_or_else(port),
        "token": if enabled() { token() } else { String::new() },
        "error": LAST_ERROR.lock().unwrap().clone(),
        "exe": std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default(),
    })
}

pub fn set_enabled(on: bool) -> Value {
    LocalConfig::set_option(OPT_ENABLED.to_owned(), if on { "Y" } else { "" }.to_owned());
    if on {
        start();
    } else {
        stop();
    }
    info()
}

pub fn start_if_enabled() {
    if enabled() {
        start();
    }
}

pub fn start() {
    let mut server = SERVER.lock().unwrap();
    if server.is_some() {
        return;
    }
    let port = port();
    let token = token();
    STOP.store(false, Ordering::SeqCst);
    let (tx, rx) = oneshot::channel();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("tether-mcp".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = ready_tx.send(Err(e.to_string()));
                    return;
                }
            };
            rt.block_on(serve(port, token, rx, ready_tx));
        });
    if let Err(e) = spawned {
        *LAST_ERROR.lock().unwrap() = e.to_string();
        return;
    }
    match ready_rx.recv_timeout(Duration::from_secs(3)) {
        Ok(Ok(())) => {
            LAST_ERROR.lock().unwrap().clear();
            *server = Some(Running {
                port,
                shutdown: Some(tx),
            });
            log::info!("Claude control (MCP) listening on 127.0.0.1:{port}");
        }
        Ok(Err(e)) => {
            log::error!("Claude control (MCP) failed to start: {e}");
            *LAST_ERROR.lock().unwrap() = e;
        }
        Err(_) => {
            *LAST_ERROR.lock().unwrap() = "the server did not start".into();
        }
    }
}

pub fn stop() {
    STOP.store(true, Ordering::SeqCst);
    if let Some(mut r) = SERVER.lock().unwrap().take() {
        if let Some(tx) = r.shutdown.take() {
            let _ = tx.send(());
        }
        log::info!("Claude control (MCP) stopped");
    }
    for s in sessions::get_sessions() {
        s.ui_handler.mcp.enabled.store(false, Ordering::Relaxed);
        s.ui_handler.mcp.frames.lock().unwrap().clear();
    }
}

// ---------------------------------------------------------------- http

async fn serve(
    port: u16,
    token: String,
    mut shutdown: oneshot::Receiver<()>,
    ready: std::sync::mpsc::Sender<Result<(), String>>,
) {
    let listener = match TcpListener::bind(("127.0.0.1", port)).await {
        Ok(l) => {
            let _ = ready.send(Ok(()));
            l
        }
        Err(e) => {
            let _ = ready.send(Err(format!("port {port}: {e}")));
            return;
        }
    };
    let token = Arc::new(token);
    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            acc = listener.accept() => {
                if let Ok((stream, _)) = acc {
                    let token = token.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_conn(stream, port, &token).await {
                            log::debug!("mcp connection: {e}");
                        }
                    });
                }
            }
        }
    }
}

fn eq_ct(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn respond(stream: &mut TcpStream, status: &str, ctype: &str, body: &[u8], extra: &str) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.flush().await
}

async fn handle_conn(mut stream: TcpStream, port: u16, token: &str) -> std::io::Result<()> {
    // Headers
    let mut buf = Vec::with_capacity(4096);
    let header_end = loop {
        let mut chunk = [0u8; 4096];
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        if buf.len() > 32 * 1024 {
            return respond(&mut stream, "431 Request Header Fields Too Large", "text/plain", b"", "").await;
        }
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_owned();
    let path = parts.next().unwrap_or_default().split('?').next().unwrap_or_default().to_owned();
    let mut headers = HashMap::new();
    for l in lines {
        if let Some((k, v)) = l.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_owned());
        }
    }

    // Only local, token-bearing callers (DNS-rebinding and browser safe).
    let host = headers.get("host").map(String::as_str).unwrap_or_default();
    let host_ok = [format!("127.0.0.1:{port}"), format!("localhost:{port}"), "127.0.0.1".into(), "localhost".into()]
        .iter()
        .any(|h| h == host);
    if !host_ok {
        return respond(&mut stream, "403 Forbidden", "text/plain", b"bad host", "").await;
    }
    if let Some(origin) = headers.get("origin") {
        let local = origin.starts_with("http://127.0.0.1") || origin.starts_with("http://localhost");
        if !local {
            return respond(&mut stream, "403 Forbidden", "text/plain", b"bad origin", "").await;
        }
    }
    let auth_ok = headers
        .get("authorization")
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|t| eq_ct(t.trim(), token));
    if !auth_ok {
        return respond(&mut stream, "401 Unauthorized", "application/json", br#"{"error":"missing or wrong token"}"#, "").await;
    }
    if path != "/mcp" && path != "/" {
        return respond(&mut stream, "404 Not Found", "text/plain", b"", "").await;
    }
    if method != "POST" {
        return respond(&mut stream, "405 Method Not Allowed", "text/plain", b"", "Allow: POST\r\n").await;
    }

    // Body
    let len: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    if len > MAX_BODY {
        return respond(&mut stream, "413 Payload Too Large", "text/plain", b"", "").await;
    }
    let mut body = buf[header_end..].to_vec();
    while body.len() < len {
        let mut chunk = vec![0u8; (len - body.len()).min(64 * 1024)];
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(len);

    let msg: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            let err = rpc_error(Value::Null, -32700, &format!("parse error: {e}"));
            return respond(&mut stream, "400 Bad Request", "application/json", err.to_string().as_bytes(), "").await;
        }
    };
    // Tool calls sleep (human timing), so run them off the async workers.
    let reply = tokio::task::spawn_blocking(move || handle_message(msg))
        .await
        .unwrap_or_else(|e| Some(rpc_error(Value::Null, -32603, &e.to_string())));
    match reply {
        Some(v) => respond(&mut stream, "200 OK", "application/json", v.to_string().as_bytes(), "").await,
        None => respond(&mut stream, "202 Accepted", "text/plain", b"", "").await,
    }
}

// ---------------------------------------------------------------- json-rpc

fn rpc_error(id: Value, code: i32, msg: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": msg } })
}

/// One JSON-RPC message (or a batch); None when nothing is to be sent back.
pub fn handle_message(msg: Value) -> Option<Value> {
    if let Value::Array(items) = msg {
        let out: Vec<Value> = items.into_iter().filter_map(handle_one).collect();
        return if out.is_empty() { None } else { Some(Value::Array(out)) };
    }
    handle_one(msg)
}

fn handle_one(msg: Value) -> Option<Value> {
    let id = msg.get("id").cloned();
    let method = msg.get("method").and_then(Value::as_str).unwrap_or_default();
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    let id = match id {
        Some(id) if !id.is_null() => id,
        // Notifications (initialized, cancelled, ...) and responses: nothing to answer.
        _ => return None,
    };
    let result = match method {
        "initialize" => {
            let asked = params["protocolVersion"].as_str().unwrap_or_default();
            let version = if PROTOCOL_VERSIONS.contains(&asked) { asked } else { PROTOCOL_VERSIONS[0] };
            json!({
                "protocolVersion": version,
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": { "name": "tether", "title": "Tether remote control", "version": crate::VERSION },
                "instructions": INSTRUCTIONS,
            })
        }
        "ping" => json!({}),
        "tools/list" => json!({ "tools": tool_list() }),
        "tools/call" => {
            let name = params["name"].as_str().unwrap_or_default();
            let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
            match call_tool(name, &args) {
                Ok(content) => json!({ "content": content, "isError": false }),
                Err(e) => json!({ "content": [{ "type": "text", "text": e }], "isError": true }),
            }
        }
        "resources/list" => json!({ "resources": [] }),
        "prompts/list" => json!({ "prompts": [] }),
        _ => return Some(rpc_error(id, -32601, &format!("method not found: {method}"))),
    };
    Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

const INSTRUCTIONS: &str = "Controls remote computers through the user's open Tether sessions. \
Start with list_sessions, then take a screenshot. All x/y coordinates are pixels of the most \
recent screenshot of that session, so take a new screenshot whenever the screen may have \
changed and before clicking. Mouse and keyboard input is performed like a person (curved \
pointer movement, natural click and typing timing), so actions take a moment. Use press_keys \
for shortcuts (\"ctrl+s\", \"alt+tab\", \"enter\") and type_text for text.";

fn session_prop() -> Value {
    json!({ "type": "string", "description": "Peer ID from list_sessions. Optional when only one session is open." })
}

fn tool_list() -> Value {
    let s = session_prop();
    let xy = |what: &str| {
        json!({ "type": "number", "description": format!("{what} in pixels of the latest screenshot") })
    };
    json!([
        {
            "name": "list_sessions",
            "description": "List the remote computers open in Tether (peer ID, name, OS, displays, whether keyboard/mouse control is allowed).",
            "inputSchema": { "type": "object", "properties": {} },
            "annotations": { "readOnlyHint": true }
        },
        {
            "name": "screenshot",
            "description": "Take a screenshot of the remote screen. Returns a JPEG; its pixel coordinates are what the other tools use.",
            "inputSchema": { "type": "object", "properties": {
                "session": s,
                "display": { "type": "integer", "description": "Display index (default: the one shown in the session)" },
                "max_width": { "type": "integer", "description": "Scale down to at most this width (default 1280, 640-1920)" }
            } },
            "annotations": { "readOnlyHint": true }
        },
        {
            "name": "click",
            "description": "Move the pointer to (x, y) like a person and click.",
            "inputSchema": { "type": "object", "properties": {
                "session": s, "x": xy("X"), "y": xy("Y"),
                "button": { "type": "string", "enum": ["left", "right", "middle"], "description": "Default left" },
                "count": { "type": "integer", "description": "1 = click (default), 2 = double click, 3 = triple click" }
            }, "required": ["x", "y"] }
        },
        {
            "name": "move_mouse",
            "description": "Move the pointer to (x, y) like a person (e.g. to hover).",
            "inputSchema": { "type": "object", "properties": {
                "session": s, "x": xy("X"), "y": xy("Y")
            }, "required": ["x", "y"] }
        },
        {
            "name": "drag",
            "description": "Press at (from_x, from_y), drag to (to_x, to_y), release.",
            "inputSchema": { "type": "object", "properties": {
                "session": s,
                "from_x": xy("Start X"), "from_y": xy("Start Y"),
                "to_x": xy("End X"), "to_y": xy("End Y"),
                "button": { "type": "string", "enum": ["left", "right", "middle"] }
            }, "required": ["from_x", "from_y", "to_x", "to_y"] }
        },
        {
            "name": "scroll",
            "description": "Scroll the mouse wheel at (x, y).",
            "inputSchema": { "type": "object", "properties": {
                "session": s, "x": xy("X"), "y": xy("Y"),
                "direction": { "type": "string", "enum": ["up", "down", "left", "right"] },
                "amount": { "type": "integer", "description": "Wheel notches (default 3)" }
            }, "required": ["x", "y", "direction"] }
        },
        {
            "name": "type_text",
            "description": "Type text into the focused field, key by key like a person (natural rhythm; occasional typos are corrected). Click the field first. Newlines press Enter.",
            "inputSchema": { "type": "object", "properties": {
                "session": s, "text": { "type": "string" }
            }, "required": ["text"] }
        },
        {
            "name": "press_keys",
            "description": "Press keys or shortcuts, e.g. \"enter\", \"ctrl+s\", \"alt+f4\", \"win+r\", \"ctrl+a delete\" (space-separated sequence). Keys: letters, digits, punctuation, enter, tab, esc, backspace, delete, space, home, end, pageup, pagedown, insert, up, down, left, right, f1-f12; modifiers ctrl, shift, alt, win/cmd.",
            "inputSchema": { "type": "object", "properties": {
                "session": s, "keys": { "type": "string" }
            }, "required": ["keys"] }
        },
        {
            "name": "wait",
            "description": "Wait before the next step, e.g. while a program starts (max 30 s).",
            "inputSchema": { "type": "object", "properties": {
                "seconds": { "type": "number" }
            }, "required": ["seconds"] },
            "annotations": { "readOnlyHint": true }
        }
    ])
}

// ---------------------------------------------------------------- tools

fn text(t: impl Into<String>) -> Value {
    json!([{ "type": "text", "text": t.into() }])
}

fn peer_id(s: &FlutterSession) -> String {
    s.lc.read().unwrap().get_id().to_owned()
}

fn desktop_sessions() -> Vec<FlutterSession> {
    sessions::get_sessions()
        .into_iter()
        .filter(|s| !(s.is_file_transfer() || s.is_port_forward() || s.is_terminal() || s.is_view_camera()))
        .filter(|s| !s.ui_handler.mcp_session_ids().is_empty())
        .collect()
}

fn find_session(args: &Value) -> Result<FlutterSession, String> {
    let all = desktop_sessions();
    match args.get("session").and_then(Value::as_str).filter(|s| !s.trim().is_empty()) {
        Some(id) => {
            let id = id.replace(' ', "");
            all.into_iter()
                .find(|s| peer_id(s).replace(' ', "") == id)
                .ok_or_else(|| format!("no open session for '{id}' (see list_sessions)"))
        }
        None => match all.len() {
            0 => Err("no remote session is open in Tether".into()),
            1 => Ok(all.into_iter().next().unwrap()),
            _ => Err("several sessions are open; pass `session` (see list_sessions)".into()),
        },
    }
}

fn require_control(s: &FlutterSession) -> Result<(), String> {
    if s.sender.read().unwrap().is_none() {
        return Err("the session is not connected".into());
    }
    if !*s.server_keyboard_enabled.read().unwrap() {
        return Err("the remote side does not allow keyboard/mouse control in this session".into());
    }
    Ok(())
}

fn num(args: &Value, k: &str) -> Result<f64, String> {
    args.get(k)
        .and_then(Value::as_f64)
        .ok_or_else(|| format!("`{k}` is required"))
}

fn call_tool(name: &str, args: &Value) -> Result<Value, String> {
    if STOP.load(Ordering::SeqCst) && SERVER.lock().unwrap().is_none() {
        return Err("Claude control is turned off in Tether".into());
    }
    match name {
        "list_sessions" => list_sessions(),
        "screenshot" => screenshot(args),
        "wait" => {
            let secs = num(args, "seconds")?.clamp(0.0, 30.0);
            let end = Instant::now() + Duration::from_secs_f64(secs);
            while Instant::now() < end {
                if STOP.load(Ordering::SeqCst) {
                    return Err("stopped".into());
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Ok(text(format!("Waited {secs:.1} s")))
        }
        "click" | "move_mouse" | "drag" | "scroll" | "type_text" | "press_keys" => {
            let s = find_session(args)?;
            require_control(&s)?;
            let _one_at_a_time = ACTION.lock().unwrap();
            act(name, &s, args)
        }
        _ => Err(format!("unknown tool {name}")),
    }
}

fn list_sessions() -> Result<Value, String> {
    let list: Vec<Value> = desktop_sessions()
        .iter()
        .map(|s| {
            let displays: Vec<Value> = s
                .ui_handler
                .mcp_displays()
                .iter()
                .enumerate()
                .map(|(i, d)| json!({ "index": i, "x": d.0, "y": d.1, "width": d.2, "height": d.3 }))
                .collect();
            json!({
                "session": peer_id(s),
                "name": s.ui_handler.mcp_hostname(),
                "os": s.peer_platform(),
                "connected": s.sender.read().unwrap().is_some(),
                "control_allowed": *s.server_keyboard_enabled.read().unwrap(),
                "displays": displays,
            })
        })
        .collect();
    if list.is_empty() {
        return Ok(text("No remote session is open in Tether. Ask the user to connect to the computer first."));
    }
    Ok(text(serde_json::to_string_pretty(&list).unwrap_or_default()))
}

fn screenshot(args: &Value) -> Result<Value, String> {
    let s = find_session(args)?;
    let mcp = s.ui_handler.mcp.clone();
    let first_use = !mcp.enabled.swap(true, Ordering::Relaxed);
    let asked = Instant::now();
    mcp.want.store(true, Ordering::Relaxed);
    // Wait for a fresh frame; a still screen sends none, so fall back to the
    // latest copy after a short while.
    let deadline = asked + Duration::from_millis(if first_use { 3000 } else { 500 });
    let wanted_display = args.get("display").and_then(Value::as_u64).map(|d| d as usize);
    loop {
        {
            let frames = mcp.frames.lock().unwrap();
            let fresh = frames
                .iter()
                .filter(|(d, _)| wanted_display.map_or(true, |w| w == **d))
                .any(|(_, f)| f.at.is_some_and(|t| t >= asked));
            if fresh || (Instant::now() >= deadline && !frames.is_empty()) {
                break;
            }
        }
        if Instant::now() >= deadline + Duration::from_secs(2) {
            return Err("no picture from the remote screen yet; make sure the session window is open and showing the screen".into());
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    let max_w = args
        .get("max_width")
        .and_then(Value::as_u64)
        .unwrap_or(1280)
        .clamp(640, 1920) as u32;

    let (display, img) = {
        let frames = mcp.frames.lock().unwrap();
        let (display, f) = match wanted_display {
            Some(d) => (d, frames.get(&d).ok_or_else(|| format!("display {d} is not being shown in the session"))?),
            None => frames
                .iter()
                .max_by_key(|(_, f)| f.at)
                .map(|(d, f)| (*d, f))
                .ok_or("no picture yet")?,
        };
        (display, frame_to_rgb(f)?)
    };
    let (fw, fh) = (img.width(), img.height());
    let img = if fw > max_w {
        let nh = ((fh as f64) * (max_w as f64) / (fw as f64)).round().max(1.0) as u32;
        image::imageops::resize(&img, max_w, nh, image::imageops::FilterType::Triangle)
    } else {
        img
    };
    let (w, h) = (img.width(), img.height());
    let mut jpeg = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 75)
        .encode(img.as_raw(), w, h, image::ColorType::Rgb8)
        .map_err(|e| e.to_string())?;

    let rect = s
        .ui_handler
        .mcp_displays()
        .get(display)
        .map(|d| (d.0 as f64, d.1 as f64, d.2 as f64, d.3 as f64))
        .unwrap_or((0.0, 0.0, fw as f64, fh as f64));
    let id = peer_id(&s);
    SHOTS.lock().unwrap().insert(
        id.clone(),
        Shot { display, rect, w: w as f64, h: h as f64 },
    );
    #[allow(deprecated)]
    let data = hbb_common::base64::encode(&jpeg);
    Ok(json!([
        { "type": "text", "text": format!(
            "Screenshot of {id} ({}), display {display}: {w}x{h} pixels. Use these pixel coordinates for click, move_mouse, drag and scroll.",
            s.ui_handler.mcp_hostname()) },
        { "type": "image", "data": data, "mimeType": "image/jpeg" }
    ]))
}

fn frame_to_rgb(f: &crate::flutter::McpFrame) -> Result<image::RgbImage, String> {
    if f.w == 0 || f.h == 0 || f.stride < f.w * 4 || f.data.len() < f.stride * f.h {
        return Err("the remote picture is not available".into());
    }
    let mut out = Vec::with_capacity(f.w * f.h * 3);
    for row in f.data.chunks(f.stride).take(f.h) {
        for px in row[..f.w * 4].chunks_exact(4) {
            if f.rgba_order {
                out.extend_from_slice(&[px[0], px[1], px[2]]);
            } else {
                out.extend_from_slice(&[px[2], px[1], px[0]]);
            }
        }
    }
    image::RgbImage::from_raw(f.w as u32, f.h as u32, out).ok_or_else(|| "bad picture".into())
}

/// Screenshot pixels -> remote coordinates.
fn to_remote(id: &str, x: f64, y: f64) -> Result<(f64, f64), String> {
    let shot = SHOTS
        .lock()
        .unwrap()
        .get(id)
        .copied()
        .ok_or("take a screenshot of this session first; coordinates refer to it")?;
    if !(0.0..=shot.w).contains(&x) || !(0.0..=shot.h).contains(&y) {
        return Err(format!(
            "({x}, {y}) is outside the screenshot ({}x{})",
            shot.w, shot.h
        ));
    }
    let (rx, ry, rw, rh) = shot.rect;
    // Keep inside the display (the right/bottom edge belongs to the next one).
    let px = (rx + x * rw / shot.w).min(rx + rw - 1.0);
    let py = (ry + y * rh / shot.h).min(ry + rh - 1.0);
    let _ = shot.display;
    Ok((px.round(), py.round()))
}

fn start_pointer(s: &FlutterSession, id: &str) -> (f64, f64) {
    if let Some(p) = POINTER.lock().unwrap().get(id) {
        return *p;
    }
    if let Some((x, y)) = *s.ui_handler.mcp.cursor.lock().unwrap() {
        return (x as f64, y as f64);
    }
    SHOTS
        .lock()
        .unwrap()
        .get(id)
        .map(|sh| (sh.rect.0 + sh.rect.2 / 2.0, sh.rect.1 + sh.rect.3 / 2.0))
        .unwrap_or((0.0, 0.0))
}

fn act(name: &str, s: &FlutterSession, args: &Value) -> Result<Value, String> {
    let id = peer_id(s);
    let stop: &AtomicBool = &STOP;
    let stopped = |_| "stopped".to_owned();
    match name {
        "type_text" => {
            let t = args.get("text").and_then(Value::as_str).ok_or("`text` is required")?;
            let sid = *s
                .ui_handler
                .mcp_session_ids()
                .first()
                .ok_or("the session window is closed")?;
            let wpm = LocalConfig::get_option(OPT_WPM).parse::<f64>().unwrap_or(70.0);
            let mistakes = LocalConfig::get_option(OPT_MISTAKES).parse::<f64>().unwrap_or(2.0);
            crate::auto_type::type_blocking(
                sid,
                t,
                crate::auto_type::Settings {
                    wpm,
                    error_rate: mistakes / 100.0,
                    start_delay: 0,
                    think_pauses: true,
                },
                STOP.clone(),
            )?;
            return Ok(text(format!("Typed {} characters", t.chars().count())));
        }
        "press_keys" => {
            let keys = args.get("keys").and_then(Value::as_str).ok_or("`keys` is required")?;
            let mut hi = HumanInput::new(s, stop, start_pointer(s, &id));
            hi.press_keys(keys)?;
            return Ok(text(format!("Pressed {keys}")));
        }
        _ => {}
    }
    let mut hi = HumanInput::new(s, stop, start_pointer(s, &id));
    let button = Button::parse(args.get("button").and_then(Value::as_str).unwrap_or("left"))
        .ok_or("`button` must be left, right or middle")?;
    let result = match name {
        "move_mouse" => {
            let (x, y) = to_remote(&id, num(args, "x")?, num(args, "y")?)?;
            hi.move_to(x, y).map_err(stopped)?;
            "Moved the pointer".to_owned()
        }
        "click" => {
            let (x, y) = to_remote(&id, num(args, "x")?, num(args, "y")?)?;
            let count = args.get("count").and_then(Value::as_u64).unwrap_or(1).clamp(1, 3) as u32;
            hi.click(x, y, button, count).map_err(stopped)?;
            match count {
                2 => "Double-clicked".to_owned(),
                3 => "Triple-clicked".to_owned(),
                _ => "Clicked".to_owned(),
            }
        }
        "drag" => {
            let from = to_remote(&id, num(args, "from_x")?, num(args, "from_y")?)?;
            let to = to_remote(&id, num(args, "to_x")?, num(args, "to_y")?)?;
            hi.drag(from, to, button).map_err(stopped)?;
            "Dragged".to_owned()
        }
        "scroll" => {
            let (x, y) = to_remote(&id, num(args, "x")?, num(args, "y")?)?;
            let amount = args.get("amount").and_then(Value::as_u64).unwrap_or(3).clamp(1, 50) as u32;
            let (dx, dy) = match args.get("direction").and_then(Value::as_str).unwrap_or_default() {
                "up" => (0, 1),
                "down" => (0, -1),
                "left" => (1, 0),
                "right" => (-1, 0),
                _ => return Err("`direction` must be up, down, left or right".into()),
            };
            hi.scroll(x, y, dx, dy, amount).map_err(stopped)?;
            format!("Scrolled {amount} notch(es)")
        }
        _ => unreachable!(),
    };
    POINTER.lock().unwrap().insert(id, hi.position());
    Ok(text(result))
}

// ---------------------------------------------------------------- stdio bridge

/// `tether.exe --mcp`: MCP over stdio for clients that start their tools as a
/// program (Claude Desktop). Forwards every line to the running Tether.
pub fn run_stdio_bridge() {
    let port = port();
    let token = LocalConfig::get_option(OPT_TOKEN);
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let reply = match post_local(port, &token, &line) {
            Ok(r) => r,
            Err(e) => {
                let id = serde_json::from_str::<Value>(&line)
                    .ok()
                    .and_then(|v| v.get("id").cloned())
                    .filter(|v| !v.is_null());
                id.map(|id| {
                    rpc_error(
                        id,
                        -32000,
                        &format!("Tether is not reachable ({e}). Open Tether and turn on Settings > General > Claude control."),
                    )
                    .to_string()
                })
            }
        };
        if let Some(r) = reply {
            if writeln!(stdout, "{r}").and_then(|_| stdout.flush()).is_err() {
                break;
            }
        }
    }
}

fn post_local(port: u16, token: &str, body: &str) -> std::io::Result<Option<String>> {
    use std::net::TcpStream as StdTcp;
    let mut s = StdTcp::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_secs(3),
    )?;
    let req = format!(
        "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    s.write_all(req.as_bytes())?;
    let mut resp = Vec::new();
    s.read_to_end(&mut resp)?;
    let Some(split) = resp.windows(4).position(|w| w == b"\r\n\r\n") else {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "bad response"));
    };
    let head = String::from_utf8_lossy(&resp[..split]);
    let status: u16 = head
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    let body = String::from_utf8_lossy(&resp[split + 4..]).to_string();
    match status {
        200 => Ok(Some(
            // One line per message on stdio.
            serde_json::from_str::<Value>(&body)
                .map(|v| v.to_string())
                .unwrap_or(body),
        )),
        202 => Ok(None),
        401 => Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "token mismatch")),
        c => Err(std::io::Error::new(std::io::ErrorKind::Other, format!("HTTP {c}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialize_and_list_tools() {
        let r = handle_message(json!({"jsonrpc":"2.0","id":1,"method":"initialize",
            "params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}))
            .unwrap();
        assert_eq!(r["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(r["result"]["serverInfo"]["name"], "tether");
        assert!(handle_message(json!({"jsonrpc":"2.0","method":"notifications/initialized"})).is_none());
        let r = handle_message(json!({"jsonrpc":"2.0","id":2,"method":"tools/list"})).unwrap();
        let names: Vec<_> = r["result"]["tools"].as_array().unwrap().iter()
            .map(|t| t["name"].as_str().unwrap().to_owned()).collect();
        for n in ["list_sessions", "screenshot", "click", "move_mouse", "drag", "scroll", "type_text", "press_keys", "wait"] {
            assert!(names.contains(&n.to_owned()), "{n}");
        }
        let r = handle_message(json!({"jsonrpc":"2.0","id":3,"method":"nope"})).unwrap();
        assert_eq!(r["error"]["code"], -32601);
        let r = handle_message(json!({"jsonrpc":"2.0","id":4,"method":"initialize","params":{"protocolVersion":"1999-01-01"}})).unwrap();
        assert_eq!(r["result"]["protocolVersion"], PROTOCOL_VERSIONS[0]);
    }

    #[test]
    fn tool_errors_are_results() {
        let r = handle_message(json!({"jsonrpc":"2.0","id":5,"method":"tools/call",
            "params":{"name":"click","arguments":{"x":1,"y":1}}})).unwrap();
        assert_eq!(r["result"]["isError"], true);
    }

    #[test]
    fn http_server_and_bridge_round_trip() {
        let port = 47_191;
        let token = "tmcp_test_token".to_owned();
        let (_tx, rx) = oneshot::channel();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let t = token.clone();
        std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(serve(port, t, rx, ready_tx));
        });
        ready_rx.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();

        let init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#;
        let reply = post_local(port, &token, init).unwrap().unwrap();
        assert!(!reply.contains('\n'), "one line per message");
        let v: Value = serde_json::from_str(&reply).unwrap();
        assert_eq!(v["result"]["serverInfo"]["name"], "tether");

        let note = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        assert!(post_local(port, &token, note).unwrap().is_none());

        let list = r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"list_sessions","arguments":{}}}"#;
        let v: Value = serde_json::from_str(&post_local(port, &token, list).unwrap().unwrap()).unwrap();
        assert_eq!(v["result"]["isError"], false);

        assert!(post_local(port, "wrong", init).is_err());
    }

    #[test]
    fn maps_screenshot_pixels_to_remote_display() {
        SHOTS.lock().unwrap().insert(
            "peer".into(),
            Shot { display: 1, rect: (1920.0, 0.0, 2560.0, 1440.0), w: 1280.0, h: 720.0 },
        );
        assert_eq!(to_remote("peer", 640.0, 360.0).unwrap(), (3200.0, 720.0));
        assert_eq!(to_remote("peer", 1280.0, 720.0).unwrap(), (4479.0, 1439.0));
        assert!(to_remote("peer", 1300.0, 10.0).is_err());
        assert!(to_remote("other", 1.0, 1.0).is_err());
    }
}
