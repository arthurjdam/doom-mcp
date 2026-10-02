//! Live spectator view: a small local web page that plays back every frame
//! the engine renders, in real time, alongside a log of the model's actions.
//!
//! The game itself runs in bursts (an `act` of 30 tics finishes in a few
//! milliseconds), so frames are queued and re-paced to 35 fps here. Log lines,
//! the plan and the minimap data go through the same queue so they stay in
//! sync with the video.
//!
//! The page can also send short messages to the model (POST /command). They
//! are queued here and attached to the model's next tool result. Because that
//! text reaches the model as an instruction, the endpoint requires a random
//! per-run token (only present in the URL we open for the user) and rejects
//! requests from other origins or hosts.

use std::collections::VecDeque;
use std::io::Cursor;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::Result;
use base64::Engine as _;
use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc};

use crate::doom::engine::TICRATE;
use crate::doom::ffi::{self, SCREEN_H, SCREEN_W};
use crate::map;
use crate::session::{NavSummary, Session};

const PAGE: &str = include_str!("viewer.html");
/// Drop the oldest frames once playback falls this far behind (frames).
const MAX_BACKLOG: usize = TICRATE as usize * 6;
const LOG_HISTORY: usize = 100;
/// Longest message the spectator may send, in characters.
pub const MAX_MESSAGE_CHARS: usize = 500;
/// Most messages waiting for delivery at once.
const MAX_PENDING: usize = 20;
/// Largest request body accepted. Leaves room for MAX_MESSAGE_CHARS characters
/// even when every one is JSON-escaped as a surrogate pair (12 bytes each).
const MAX_BODY: usize = 16 * 1024;
/// How long a client may take to send its request.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LogKind {
    /// What the model decided to do.
    Action,
    /// The model's own note to the spectator.
    Comment,
    /// What happened as a result.
    Result,
    /// A message the spectator typed.
    User,
    /// The model received the spectator's message(s).
    Delivered,
}

#[derive(Serialize)]
struct LogLine<'a> {
    kind: LogKind,
    text: &'a str,
}

enum Item {
    /// A frame plus the player's (x, y, angle) when it was drawn, if in a level.
    Frame(Vec<u32>, Option<[f64; 3]>),
    Log(Arc<str>),
    /// Latest-value-wins state (level geometry, route, plan) replayed to new viewers.
    Sticky(&'static str, Arc<str>),
}

fn sse(event: &str, json: &str) -> Arc<str> {
    format!("event: {event}\ndata: {json}\n\n").into()
}

fn sticky(name: &'static str, json: String) {
    if let Some(sink) = SINK.get() {
        let _ = sink.send(Item::Sticky(name, sse(name, &json)));
    }
}

/// Show the model's plan on the spectator page.
pub fn publish_plan(plan: &str) {
    sticky("plan", serde_json::to_string(plan).unwrap_or_default());
}

static LAST_LEVEL: Mutex<Option<(i32, i32)>> = Mutex::new(None);

/// Send the minimap data: level geometry (when the level changes) and the
/// current route.
pub fn publish_status(session: &mut Session, nav: Option<&NavSummary>) {
    if SINK.get().is_none() {
        return;
    }
    let s = session.state();
    if s.in_level == 0 || s.demoplayback != 0 || s.gamestate != 0 {
        sticky("status", r#"{"in_level":false}"#.into());
        return;
    }
    let id = (s.episode, s.map);
    let changed = LAST_LEVEL.lock().unwrap().replace(id) != Some(id);
    if changed {
        let lines: Vec<(f64, f64, f64, f64, String)> = session
            .lines()
            .iter()
            .map(|l| {
                let kind = map::classify(l).unwrap_or(if l.two_sided != 0 { '-' } else { '#' });
                (l.x1, l.y1, l.x2, l.y2, kind.to_string())
            })
            .collect();
        let json = serde_json::json!({ "name": crate::observe::level_name(&s), "lines": lines });
        sticky("level", json.to_string());
    }
    let route: Vec<(f64, f64)> = session
        .route()
        .map(|r| {
            r.waypoints
                .iter()
                .map(|w| (w.x.round(), w.y.round()))
                .collect()
        })
        .unwrap_or_default();
    let json = serde_json::json!({
        "in_level": true,
        "goal": nav.map(|n| n.goal.clone()),
        "route_length": nav.and_then(|n| n.route_length),
        "note": nav.and_then(|n| n.note.clone()),
        "route": route,
        "player": [s.x, s.y, s.angle],
    });
    sticky("status", json.to_string());
}

static SINK: OnceLock<mpsc::UnboundedSender<Item>> = OnceLock::new();

/// Spectator messages waiting for the model's next tool result.
static INBOX: Mutex<VecDeque<String>> = Mutex::new(VecDeque::new());

/// Take every message waiting for the model, oldest first, and note the
/// delivery in the spectator log.
pub fn take_messages() -> Vec<String> {
    let messages: Vec<String> = INBOX.lock().unwrap().drain(..).collect();
    match messages.len() {
        0 => {}
        1 => log(LogKind::Delivered, "Claude received your message"),
        n => log(LogKind::Delivered, &format!("Claude received your {n} messages")),
    }
    messages
}

fn log_event(kind: LogKind, text: &str) -> Arc<str> {
    let json = serde_json::to_string(&LogLine { kind, text }).unwrap_or_default();
    format!("event: log\ndata: {json}\n\n").into()
}

/// Append a line to the spectator log. A no-op when the viewer is off.
/// Call it from the engine thread so it lines up with the frames around it.
pub fn log(kind: LogKind, text: &str) {
    if let Some(sink) = SINK.get() {
        let _ = sink.send(Item::Log(log_event(kind, text)));
    }
}

extern "C" fn on_frame(fb: *const u32) {
    if let Some(sink) = SINK.get() {
        let frame = unsafe { std::slice::from_raw_parts(fb, SCREEN_W * SCREEN_H) }.to_vec();
        let mut st = ffi::State::default();
        unsafe { ffi::dmcp_state(&mut st) };
        let pos = (st.in_level != 0).then_some([st.x, st.y, st.angle]);
        let _ = sink.send(Item::Frame(frame, pos));
    }
}

struct Shared {
    events: broadcast::Sender<Arc<str>>,
    /// Latest value per sticky event name ("frame", "level", "status", "plan").
    sticky: Mutex<Vec<(&'static str, Arc<str>)>>,
    recent_logs: Mutex<VecDeque<Arc<str>>>,
    /// Required in the X-Doom-Token header to send messages.
    token: String,
    /// Host header values (and origins, with "http://") we answer to.
    hosts: [String; 2],
}

impl Shared {
    /// Add a log line to the history and send it right away, bypassing playback pacing.
    fn log_now(&self, event: Arc<str>) {
        let mut logs = self.recent_logs.lock().unwrap();
        logs.push_back(event.clone());
        if logs.len() > LOG_HISTORY {
            logs.pop_front();
        }
        drop(logs);
        let _ = self.events.send(event);
    }

    fn set_sticky(&self, name: &'static str, event: Arc<str>) {
        let mut sticky = self.sticky.lock().unwrap();
        match sticky.iter_mut().find(|(n, _)| *n == name) {
            Some(slot) => slot.1 = event.clone(),
            None => sticky.push((name, event.clone())),
        }
        drop(sticky);
        let _ = self.events.send(event);
    }
}

/// Start the viewer on 127.0.0.1, preferring `port`. Must be called before
/// the engine starts. Returns the page URL, including the token that lets the
/// page send messages to the model.
pub async fn start(port: u16) -> Result<String> {
    let listener = match TcpListener::bind(("127.0.0.1", port)).await {
        Ok(l) => l,
        // Probably another doom-mcp instance; take any free port.
        Err(_) => TcpListener::bind(("127.0.0.1", 0)).await?,
    };
    let port = listener.local_addr()?.port();
    let token = random_token()?;
    let url = format!("http://127.0.0.1:{port}/?token={token}");

    let (tx, rx) = mpsc::unbounded_channel();
    let _ = SINK.set(tx);
    unsafe { ffi::dmcp_set_frame_callback(on_frame) };

    let shared = Arc::new(Shared {
        events: broadcast::channel(512).0,
        sticky: Mutex::new(Vec::new()),
        recent_logs: Mutex::new(VecDeque::new()),
        token,
        hosts: [format!("127.0.0.1:{port}"), format!("localhost:{port}")],
    });
    tokio::spawn(pace(rx, shared.clone()));
    tokio::spawn(serve(listener, shared));
    Ok(url)
}

/// Open `url` in the user's browser.
pub fn open_browser(url: &str) {
    let mut cmd = if cfg!(target_os = "macos") {
        std::process::Command::new("open")
    } else if cfg!(windows) {
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", "start", ""]);
        c
    } else {
        std::process::Command::new("xdg-open")
    };
    // Keep the child off our stdout, which carries MCP traffic.
    let spawned = cmd
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    if let Err(e) = spawned {
        eprintln!("doom-mcp: couldn't open a browser ({e}); visit {url}");
    }
}

/// Replays queued frames at the game's own rate.
async fn pace(mut rx: mpsc::UnboundedReceiver<Item>, shared: Arc<Shared>) {
    let mut queue: VecDeque<Item> = VecDeque::new();
    let mut frames_queued = 0usize;
    let mut tick = tokio::time::interval(Duration::from_secs(1) / TICRATE);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            item = rx.recv() => {
                let Some(item) = item else { return };
                if matches!(item, Item::Frame(..)) {
                    frames_queued += 1;
                }
                queue.push_back(item);
                // Too far behind: skip ahead, keeping the log lines.
                while frames_queued > MAX_BACKLOG {
                    let i = queue.iter().position(|it| matches!(it, Item::Frame(..))).unwrap();
                    queue.remove(i);
                    frames_queued -= 1;
                }
            }
            _ = tick.tick(), if !queue.is_empty() => {
                // Emit log lines up to and including the next frame.
                while let Some(item) = queue.pop_front() {
                    match item {
                        Item::Log(event) => shared.log_now(event),
                        Item::Sticky(name, event) => shared.set_sticky(name, event),
                        Item::Frame(frame, pos) => {
                            frames_queued -= 1;
                            let mut event = frame_event(&frame);
                            if let Some([x, y, a]) = pos {
                                event.push_str(&format!(
                                    "event: pos\ndata: [{x:.0},{y:.0},{a:.1}]\n\n"
                                ));
                            }
                            shared.set_sticky("frame", event.into());
                            break;
                        }
                    }
                }
            }
        }
    }
}

fn frame_event(frame: &[u32]) -> String {
    let mut rgb = Vec::with_capacity(frame.len() * 3);
    for &px in frame {
        rgb.extend_from_slice(&[(px >> 16) as u8, (px >> 8) as u8, px as u8]);
    }
    let mut png_bytes = Vec::new();
    let mut encoder = png::Encoder::new(
        Cursor::new(&mut png_bytes),
        SCREEN_W as u32,
        SCREEN_H as u32,
    );
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(png::Compression::Fast);
    if let Ok(mut writer) = encoder.write_header() {
        let _ = writer.write_image_data(&rgb);
    }
    let b64 = base64::engine::general_purpose::STANDARD.encode(&png_bytes);
    format!("event: frame\ndata: {b64}\n\n")
}

async fn serve(listener: TcpListener, shared: Arc<Shared>) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        let shared = shared.clone();
        tokio::spawn(async move {
            let _ = handle(stream, shared).await;
        });
    }
}

/// 128 random bits as hex, from the OS.
fn random_token() -> Result<String> {
    use std::io::Read;
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Compare secrets without an early exit on the first differing byte.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

struct Request {
    method: String,
    path: String,
    /// Header names are lowercased.
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }
}

enum ReadError {
    Io,
    BadRequest,
    TooLarge,
}

async fn read_request(stream: &mut TcpStream) -> Result<Request, ReadError> {
    let mut buf = vec![0u8; 8192];
    let mut len = 0;
    let head_end = loop {
        if let Some(i) = buf[..len].windows(4).position(|w| w == b"\r\n\r\n") {
            break i;
        }
        if len == buf.len() {
            return Err(ReadError::TooLarge);
        }
        match stream.read(&mut buf[len..]).await {
            Ok(0) | Err(_) => return Err(ReadError::Io),
            Ok(n) => len += n,
        }
    };
    let head = std::str::from_utf8(&buf[..head_end]).map_err(|_| ReadError::BadRequest)?;
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next().unwrap_or("").split(' ');
    let method = request_line.next().unwrap_or("").to_string();
    let target = request_line.next().ok_or(ReadError::BadRequest)?;
    let path = target.split('?').next().unwrap_or("").to_string();
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(n, v)| (n.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();

    let content_length = match headers.iter().find(|(n, _)| n == "content-length") {
        Some((_, v)) => v.parse::<usize>().map_err(|_| ReadError::BadRequest)?,
        None => 0,
    };
    if content_length > MAX_BODY {
        return Err(ReadError::TooLarge);
    }
    let mut body = buf[head_end + 4..len].to_vec();
    while body.len() < content_length {
        let mut chunk = vec![0u8; content_length - body.len()];
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => return Err(ReadError::Io),
            Ok(n) => body.extend_from_slice(&chunk[..n]),
        }
    }
    body.truncate(content_length);
    Ok(Request { method, path, headers, body })
}

async fn respond(stream: &mut TcpStream, status: &str, content_type: &str, body: &[u8]) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body).await
}

async fn respond_json(stream: &mut TcpStream, status: &str, json: serde_json::Value) -> std::io::Result<()> {
    respond(stream, status, "application/json", json.to_string().as_bytes()).await
}

async fn handle(mut stream: TcpStream, shared: Arc<Shared>) -> std::io::Result<()> {
    let request = match tokio::time::timeout(REQUEST_TIMEOUT, read_request(&mut stream)).await {
        Ok(Ok(r)) => r,
        Ok(Err(ReadError::TooLarge)) => {
            return respond_json(&mut stream, "413 Payload Too Large", serde_json::json!({"error": "request too large"})).await;
        }
        Ok(Err(ReadError::BadRequest)) => {
            return respond_json(&mut stream, "400 Bad Request", serde_json::json!({"error": "malformed request"})).await;
        }
        Ok(Err(ReadError::Io)) | Err(_) => return Ok(()),
    };

    // Only answer to our own host name: blocks DNS-rebinding pages.
    if !request.header("host").is_some_and(|h| shared.hosts.iter().any(|ok| ok == h)) {
        return respond_json(&mut stream, "403 Forbidden", serde_json::json!({"error": "unexpected host"})).await;
    }

    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/") => respond(&mut stream, "200 OK", "text/html; charset=utf-8", PAGE.as_bytes()).await,
        ("GET", "/events") => stream_events(stream, shared).await,
        ("POST", "/command") => {
            let (status, json) = accept_command(&request, &shared);
            respond_json(&mut stream, status, json).await
        }
        (_, "/" | "/events" | "/command") => {
            respond_json(&mut stream, "405 Method Not Allowed", serde_json::json!({"error": "method not allowed"})).await
        }
        _ => respond_json(&mut stream, "404 Not Found", serde_json::json!({"error": "not found"})).await,
    }
}

/// Validate a spectator message and queue it for the model.
fn accept_command(request: &Request, shared: &Shared) -> (&'static str, serde_json::Value) {
    use serde_json::json;
    let forbidden = |why: &str| ("403 Forbidden", json!({ "error": why }));
    // Browsers always send Origin on POST; it must be this page.
    if let Some(origin) = request.header("origin")
        && !shared.hosts.iter().any(|h| origin == format!("http://{h}"))
    {
        return forbidden("cross-origin requests are not allowed");
    }
    let token_ok = request
        .header("x-doom-token")
        .is_some_and(|t| constant_time_eq(t.as_bytes(), shared.token.as_bytes()));
    if !token_ok {
        return forbidden("missing or wrong token; open the spectator link printed by the server");
    }
    if !request.header("content-type").is_some_and(|ct| ct.starts_with("application/json")) {
        return ("415 Unsupported Media Type", json!({ "error": "send JSON" }));
    }
    #[derive(serde::Deserialize)]
    struct Body {
        text: String,
    }
    let Ok(body) = serde_json::from_slice::<Body>(&request.body) else {
        return ("400 Bad Request", json!({ "error": "expected {\"text\": \"...\"}" }));
    };
    // One line of plain text: control characters (newlines included) become spaces.
    let text: String = body.text.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.is_empty() {
        return ("400 Bad Request", json!({ "error": "message is empty" }));
    }
    if text.chars().count() > MAX_MESSAGE_CHARS {
        return ("400 Bad Request", json!({ "error": format!("message is longer than {MAX_MESSAGE_CHARS} characters") }));
    }
    let mut inbox = INBOX.lock().unwrap();
    if inbox.len() >= MAX_PENDING {
        return ("429 Too Many Requests", json!({ "error": "too many messages waiting; let Claude catch up" }));
    }
    inbox.push_back(text.clone());
    let pending = inbox.len();
    drop(inbox);
    // Through the paced queue, so it lands after the moves still being replayed:
    // that's when the model will actually see it. (The page confirms instantly.)
    log(LogKind::User, &text);
    ("200 OK", json!({ "ok": true, "pending": pending }))
}

async fn stream_events(mut stream: TcpStream, shared: Arc<Shared>) -> std::io::Result<()> {
    let mut rx = shared.events.subscribe();
    stream
        .write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
              Cache-Control: no-store\r\nConnection: keep-alive\r\n\r\n",
        )
        .await?;

    // Catch up: recent log history, then the latest level, route, plan and frame.
    let backlog: Vec<Arc<str>> = shared.recent_logs.lock().unwrap().iter().cloned().collect();
    for event in backlog {
        stream.write_all(event.as_bytes()).await?;
    }
    let sticky: Vec<Arc<str>> = shared
        .sticky
        .lock()
        .unwrap()
        .iter()
        .map(|(_, e)| e.clone())
        .collect();
    for event in sticky {
        stream.write_all(event.as_bytes()).await?;
    }

    let mut keepalive = tokio::time::interval(Duration::from_secs(15));
    loop {
        tokio::select! {
            event = rx.recv() => match event {
                Ok(event) => stream.write_all(event.as_bytes()).await?,
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return Ok(()),
            },
            _ = keepalive.tick() => stream.write_all(b": keepalive\n\n").await?,
        }
    }
}
