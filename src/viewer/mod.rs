//! Live spectator view: a small local web page that plays back every frame
//! the engine renders, in real time, alongside a log of the model's actions.
//!
//! The game itself runs in bursts (an `act` of 30 tics finishes in a few
//! milliseconds), so frames are queued and re-paced to 35 fps here. Log lines,
//! the plan and the minimap data go through the same queue so they stay in
//! sync with the video. While someone is watching, calls that advance the game
//! first wait for the previous action to finish playing (`wait_for_playback`),
//! so playback never falls behind and no frame is ever dropped.
//!
//! The page is read-only: it cannot send anything back to the model, so the
//! only way to steer the game is through the MCP client.

use std::collections::VecDeque;
use std::io::Cursor;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::Result;
use base64::Engine as _;
use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc};

use crate::engine::TICRATE;
use crate::engine::ffi::{self, SCREEN_H, SCREEN_W};

const PAGE: &str = include_str!("page.html");
/// Memory guard only: with nobody watching (so nothing waits for playback),
/// frames beyond this backlog are dropped, oldest first. With a viewer
/// connected, `wait_for_playback` keeps the backlog to about one action.
const MAX_BACKLOG: usize = TICRATE as usize * 60;
/// Let the next action start this close to the end of the previous one's
/// playback, so consecutive actions play back to back without a gap.
const SYNC_SLACK: usize = TICRATE as usize / 4;
/// Playback starts this many frames behind the game (an 86 ms jitter
/// buffer), so a frame that arrives a little late never stalls the video.
pub const JITTER_FRAMES: usize = 3;
/// Playback only re-buffers after the queue has been empty this many frame
/// periods (a real pause); shorter gaps resume as soon as a frame arrives.
const REBUFFER_AFTER: u32 = 4;
/// Never hold a tool call longer than this waiting for playback.
const MAX_SYNC_WAIT: Duration = Duration::from_secs(15);
const LOG_HISTORY: usize = 100;
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
    /// Something happened in the game (real-time mode).
    Event,
    /// Something important happened (real-time mode): it wakes the model up.
    Alert,
}

#[derive(Serialize)]
struct LogLine<'a> {
    kind: LogKind,
    text: &'a str,
}

enum Item {
    /// A frame, the player's (x, y, angle) when it was drawn (if in a
    /// level), and when it was drawn.
    Frame(Vec<u32>, Option<[f64; 3]>, Instant),
    /// The same frame, encoded as its SSE event(s) by the encoder thread.
    Encoded(Arc<str>, Instant),
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

/// A map line for the minimap: (x1, y1, x2, y2, kind) where kind is a
/// `world::map` symbol ('#' wall, 'D' door, 'E' exit...) or '-' for an open
/// two-sided line.
pub type MapLine = (f64, f64, f64, f64, char);

/// Show a new level's geometry on the minimap.
pub fn publish_level(name: &str, lines: &[MapLine]) {
    let lines: Vec<(f64, f64, f64, f64, String)> = lines
        .iter()
        .map(|&(x1, y1, x2, y2, k)| (x1, y1, x2, y2, k.to_string()))
        .collect();
    sticky(
        "level",
        serde_json::json!({ "name": name, "lines": lines }).to_string(),
    );
}

/// What the minimap shows besides the level: the route, goal and player.
#[derive(Serialize)]
pub struct SpectatorStatus {
    pub goal: Option<String>,
    pub route_length: Option<i32>,
    pub note: Option<String>,
    pub route: Vec<(f64, f64)>,
    pub player: [f64; 3],
}

/// Update the minimap; `None` when not in a level.
pub fn publish_status(status: Option<&SpectatorStatus>) {
    let json = match status {
        Some(st) => {
            let mut v = serde_json::to_value(st).unwrap_or_default();
            v["in_level"] = true.into();
            v.to_string()
        }
        None => r#"{"in_level":false}"#.into(),
    };
    sticky("status", json);
}

/// Whether the spectator page is running at all.
pub fn enabled() -> bool {
    SINK.get().is_some()
}

/// Everything for the page goes through here, in order: the encoder thread
/// turns frames into PNG events and forwards all items to the pacer.
static SINK: OnceLock<std::sync::mpsc::Sender<Item>> = OnceLock::new();

/// Frames drawn by the engine but not yet shown on the page.
static FRAMES_PENDING: AtomicUsize = AtomicUsize::new(0);
/// Open spectator streams.
static VIEWERS: AtomicUsize = AtomicUsize::new(0);

/// Frames drawn but not yet shown, if someone is watching. The real-time
/// game loop uses this as its clock: it runs a tic whenever the video is
/// about to run out, so the game goes exactly as fast as the video plays.
pub fn backlog() -> Option<usize> {
    (VIEWERS.load(Ordering::SeqCst) > 0).then(|| FRAMES_PENDING.load(Ordering::SeqCst))
}

/// Wait (up to MAX_SYNC_WAIT) until the page has nearly caught up with the
/// game, so the next action's frames follow straight on from the previous
/// ones. Returns immediately when nobody is watching.
pub async fn wait_for_playback() {
    let deadline = Instant::now() + MAX_SYNC_WAIT;
    while VIEWERS.load(Ordering::SeqCst) > 0
        && FRAMES_PENDING.load(Ordering::SeqCst) > SYNC_SLACK
        && Instant::now() < deadline
    {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
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
        FRAMES_PENDING.fetch_add(1, Ordering::SeqCst);
        if sink.send(Item::Frame(frame, pos, Instant::now())).is_err() {
            FRAMES_PENDING.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

struct Shared {
    events: broadcast::Sender<Arc<str>>,
    /// Latest value per sticky event name ("frame", "level", "status", "plan").
    sticky: Mutex<Vec<(&'static str, Arc<str>)>>,
    recent_logs: Mutex<VecDeque<Arc<str>>>,
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
/// the engine starts. Returns the page URL.
pub async fn start(port: u16) -> Result<String> {
    let listener = match TcpListener::bind(("127.0.0.1", port)).await {
        Ok(l) => l,
        // Probably another doom-mcp instance; take any free port.
        Err(_) => TcpListener::bind(("127.0.0.1", 0)).await?,
    };
    let port = listener.local_addr()?.port();
    let url = format!("http://127.0.0.1:{port}/");

    let (tx, encoder_rx) = std::sync::mpsc::channel::<Item>();
    let (pace_tx, rx) = mpsc::unbounded_channel();
    let _ = SINK.set(tx);
    std::thread::Builder::new()
        .name("viewer-encoder".into())
        .spawn(move || {
            // PNG-encode frames as they arrive (about 40% of their raw size),
            // passing every item on in its original order.
            for item in encoder_rx {
                let item = match item {
                    Item::Frame(frame, pos, drawn) => {
                        let mut event = frame_event(&frame);
                        if let Some([x, y, a]) = pos {
                            event
                                .push_str(&format!("event: pos\ndata: [{x:.0},{y:.0},{a:.1}]\n\n"));
                        }
                        Item::Encoded(event.into(), drawn)
                    }
                    other => other,
                };
                if pace_tx.send(item).is_err() {
                    break;
                }
            }
        })?;
    unsafe { ffi::dmcp_set_frame_callback(on_frame) };

    let shared = Arc::new(Shared {
        events: broadcast::channel(512).0,
        sticky: Mutex::new(Vec::new()),
        recent_logs: Mutex::new(VecDeque::new()),
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
    // Whether playback is running, as opposed to filling the jitter buffer,
    // and since when the queue has been out of frames.
    let mut playing = false;
    let mut dry_since: Option<Instant> = None;
    let period = Duration::from_secs(1) / TICRATE;
    // Skip keeps the deadlines anchored to the clock: timers fire up to 1 ms
    // late, and with Delay every late tick would push the schedule back
    // (34.5 fps instead of 35, so the video drifts ~1 s behind per minute of
    // real-time play). Skip also doesn't burst through ticks missed while the
    // queue was empty.
    let mut tick = tokio::time::interval(Duration::from_secs(1) / TICRATE);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            item = rx.recv() => {
                let Some(item) = item else { return };
                if matches!(item, Item::Encoded(..)) {
                    frames_queued += 1;
                }
                queue.push_back(item);
                // Memory guard (only reachable with nobody watching): drop the
                // oldest frames, keeping the log lines.
                while frames_queued > MAX_BACKLOG {
                    let i = queue.iter().position(|it| matches!(it, Item::Encoded(..))).unwrap();
                    queue.remove(i);
                    frames_queued -= 1;
                    FRAMES_PENDING.fetch_sub(1, Ordering::SeqCst);
                }
            }
            _ = tick.tick(), if !queue.is_empty() => {
                // Fill the jitter buffer before (re)starting playback: wait for a
                // few frames, or until the first has waited that long.
                if dry_since.is_some_and(|t| t.elapsed() >= period * REBUFFER_AFTER) {
                    playing = false;
                }
                if !playing && frames_queued > 0 {
                    let oldest = queue.iter().find_map(|it| match it {
                        Item::Encoded(_, drawn) => Some(*drawn),
                        _ => None,
                    });
                    let waited = oldest.is_some_and(|t| t.elapsed() >= period * JITTER_FRAMES as u32);
                    if frames_queued < JITTER_FRAMES && !waited {
                        continue;
                    }
                    playing = true;
                }
                // Emit log lines up to and including the next frame.
                while let Some(item) = queue.pop_front() {
                    match item {
                        Item::Log(event) => shared.log_now(event),
                        Item::Sticky(name, event) => shared.set_sticky(name, event),
                        Item::Encoded(event, _) => {
                            frames_queued -= 1;
                            shared.set_sticky("frame", event);
                            FRAMES_PENDING.fetch_sub(1, Ordering::SeqCst);
                            break;
                        }
                        // The encoder thread converts every frame before it gets here.
                        Item::Frame(..) => unreachable!("frames are encoded before pacing"),
                    }
                }
                if frames_queued == 0 {
                    dry_since.get_or_insert_with(Instant::now);
                } else {
                    dry_since = None;
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

struct Request {
    method: String,
    path: String,
    /// Header names are lowercased.
    headers: Vec<(String, String)>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
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
    // Every endpoint is a GET; nothing here accepts a body.
    if content_length > 0 {
        return Err(ReadError::TooLarge);
    }
    Ok(Request {
        method,
        path,
        headers,
    })
}

async fn respond(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body).await
}

async fn respond_json(
    stream: &mut TcpStream,
    status: &str,
    json: serde_json::Value,
) -> std::io::Result<()> {
    respond(
        stream,
        status,
        "application/json",
        json.to_string().as_bytes(),
    )
    .await
}

/// Close after replying early to a request whose body we didn't read: stop
/// sending, then drain (bounded) what the client is still uploading.
/// Closing with unread data makes the OS send a reset, and the client would
/// lose our reply.
async fn linger(mut stream: TcpStream) -> std::io::Result<()> {
    stream.shutdown().await?;
    let mut sink = [0u8; 8192];
    let mut drained = 0;
    let _ = tokio::time::timeout(Duration::from_secs(2), async {
        while drained < 1024 * 1024 {
            match stream.read(&mut sink).await {
                Ok(0) | Err(_) => break,
                Ok(n) => drained += n,
            }
        }
    })
    .await;
    Ok(())
}

async fn handle(mut stream: TcpStream, shared: Arc<Shared>) -> std::io::Result<()> {
    let request = match tokio::time::timeout(REQUEST_TIMEOUT, read_request(&mut stream)).await {
        Ok(Ok(r)) => r,
        Ok(Err(ReadError::TooLarge)) => {
            respond_json(
                &mut stream,
                "413 Payload Too Large",
                serde_json::json!({"error": "request too large"}),
            )
            .await?;
            return linger(stream).await;
        }
        Ok(Err(ReadError::BadRequest)) => {
            respond_json(
                &mut stream,
                "400 Bad Request",
                serde_json::json!({"error": "malformed request"}),
            )
            .await?;
            return linger(stream).await;
        }
        Ok(Err(ReadError::Io)) | Err(_) => return Ok(()),
    };

    // Only answer to our own host name: blocks DNS-rebinding pages.
    if !request
        .header("host")
        .is_some_and(|h| shared.hosts.iter().any(|ok| ok == h))
    {
        return respond_json(
            &mut stream,
            "403 Forbidden",
            serde_json::json!({"error": "unexpected host"}),
        )
        .await;
    }

    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/") => {
            respond(
                &mut stream,
                "200 OK",
                "text/html; charset=utf-8",
                PAGE.as_bytes(),
            )
            .await
        }
        ("GET", "/events") => stream_events(stream, shared).await,
        (_, "/" | "/events") => {
            respond_json(
                &mut stream,
                "405 Method Not Allowed",
                serde_json::json!({"error": "method not allowed"}),
            )
            .await
        }
        _ => {
            respond_json(
                &mut stream,
                "404 Not Found",
                serde_json::json!({"error": "not found"}),
            )
            .await
        }
    }
}

/// Counts an open spectator stream for as long as it lives.
struct ViewerGuard;

impl ViewerGuard {
    fn new() -> Self {
        VIEWERS.fetch_add(1, Ordering::SeqCst);
        Self
    }
}

impl Drop for ViewerGuard {
    fn drop(&mut self) {
        VIEWERS.fetch_sub(1, Ordering::SeqCst);
    }
}

async fn stream_events(mut stream: TcpStream, shared: Arc<Shared>) -> std::io::Result<()> {
    let _viewer = ViewerGuard::new();
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
