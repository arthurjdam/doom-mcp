//! The Doom engine, owned by a dedicated OS thread.

pub mod engine;
pub mod ffi;
pub mod things;

use std::sync::mpsc;

use anyhow::{Context, Result, anyhow};
use tokio::sync::oneshot;

pub use engine::Engine;

use crate::session::Session;

type Job = Box<dyn FnOnce(&mut Session) + Send>;

/// Cloneable handle to the engine thread. Jobs run one at a time, in order.
#[derive(Clone)]
pub struct Doom {
    tx: mpsc::Sender<Job>,
}

impl Doom {
    /// Start Doom with the given argv (argv[0] included) on its own thread.
    pub fn spawn(args: Vec<String>) -> Result<Self> {
        let (tx, rx) = mpsc::channel::<Job>();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();

        std::thread::Builder::new()
            .name("doom".into())
            .stack_size(16 * 1024 * 1024)
            .spawn(move || {
                let mut session = match Engine::start(&args) {
                    Ok(engine) => {
                        let _ = ready_tx.send(Ok(()));
                        Session::new(engine)
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                for job in rx {
                    job(&mut session);
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
