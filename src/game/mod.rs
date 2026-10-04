//! The game thread. `Session` (engine, navigation, pilot, the model's plan,
//! and in real-time mode the event log) lives on one dedicated OS thread;
//! `Game` is the cloneable handle the MCP front-ends use to run jobs on it.
//!
//! The thread runs queued jobs. In real-time mode it also runs one game tic
//! every 1/35 s between jobs (see `run_loop`).

pub mod events;
pub mod realtime;
pub mod session;

use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use tokio::sync::oneshot;

use crate::engine::{Engine, TICRATE};
use crate::viewer;
use session::Session;

type Job = Box<dyn FnOnce(&mut Session) + Send>;

/// Cloneable handle to the engine thread. Jobs run one at a time, in order.
#[derive(Clone)]
pub struct Game {
    tx: mpsc::Sender<Job>,
}

impl Game {
    /// Start Doom with the given argv (argv[0] included) on its own thread.
    /// With `realtime`, the game runs at 35 tics per second once a game is in
    /// progress; otherwise it only advances when a job tells it to.
    pub fn spawn(args: Vec<String>, realtime: bool) -> Result<Self> {
        let (tx, rx) = mpsc::channel::<Job>();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();

        std::thread::Builder::new()
            .name("doom".into())
            .stack_size(16 * 1024 * 1024)
            .spawn(move || {
                let mut session = match Engine::start(&args) {
                    Ok(engine) => {
                        let _ = ready_tx.send(Ok(()));
                        Session::new(engine, realtime)
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                if realtime {
                    run_loop(&mut session, &rx);
                } else {
                    for job in rx {
                        job(&mut session);
                    }
                }
            })
            .context("spawning doom thread")?;

        ready_rx
            .recv()
            .context("doom thread died during startup")??;
        Ok(Self { tx })
    }

    /// Run `f` on the engine thread and wait for its result.
    pub async fn with<R, F>(&self, f: F) -> Result<R>
    where
        R: Send + 'static,
        F: FnOnce(&mut Session) -> R + Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Box::new(move |session| {
                let _ = tx.send(f(session));
            }))
            .map_err(|_| anyhow!("doom thread is gone"))?;
        rx.await
            .map_err(|_| anyhow!("doom thread dropped the request"))
    }
}

/// Real-time loop: run jobs as they arrive, and game tics in between.
///
/// With the spectator page open, the video is the clock: a tic runs whenever
/// fewer than `JITTER_FRAMES` frames are waiting to be shown, so the game runs
/// exactly as fast as the video plays and the video never falls behind or
/// stutters (bursts, like a screen melt, are played out before the game
/// continues). Without a viewer, tics run every 1/35 s; a delayed tic makes
/// the game run a moment late rather than catching up. Tics are never skipped.
fn run_loop(session: &mut Session, rx: &mpsc::Receiver<Job>) {
    let period = Duration::from_secs(1) / TICRATE;
    let poll = Duration::from_millis(2);
    let mut next = Instant::now();
    loop {
        session.check_idle();
        let ticking = session.ticking();
        let wait = match (ticking, viewer::backlog()) {
            (false, _) => period * 4,
            (true, Some(_)) => poll,
            (true, None) => next.saturating_duration_since(Instant::now()),
        };
        match rx.recv_timeout(wait) {
            Ok(job) => job(session),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        if !session.ticking() {
            next = Instant::now();
            continue;
        }
        let now = Instant::now();
        let due = match viewer::backlog() {
            Some(waiting) => waiting < viewer::JITTER_FRAMES,
            None => now >= next,
        };
        if due {
            if let Err(e) = session.realtime_tick() {
                eprintln!("doom-mcp: game stopped: {e}");
            }
            next = (next + period).max(now + period);
        }
    }
}
