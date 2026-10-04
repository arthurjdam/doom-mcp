mod engine;
mod game;
mod mcp;
mod pilot;
mod viewer;
mod world;

use std::os::fd::FromRawFd;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result, bail};
use rmcp::ServiceExt;

const USAGE: &str = "\
Usage: doom-mcp [--mode turn|realtime] [--wad PATH] [--viewer-port PORT] [--no-viewer] [--no-open]
                [-- DOOM_ARGS...]

Runs DOOM as an MCP server over stdio.

  --wad PATH   IWAD to play (default: $DOOM_WAD, else doom1.wad / DOOM1.WAD / doom.wad /
               doom2.wad in the current directory or next to the executable)
  --viewer-port PORT  Port for the live spectator page on 127.0.0.1 (default 6660;
                      falls back to a free port if taken)
  --no-viewer         Don't serve the spectator page
  --no-open           Don't open the spectator page in a browser on the first new_game
  --mode MODE         turn (default): the game waits for each action, the model controls
                      every move. realtime: the game runs continuously, a built-in pilot
                      plays and the model gives orders.
  -- ARGS             Extra arguments passed straight to the engine (e.g. -- -fast)";

const WAD_NAMES: &[&str] = &[
    "doom1.wad",
    "DOOM1.WAD",
    "Doom1.WAD",
    "doom.wad",
    "DOOM.WAD",
    "doom2.wad",
    "DOOM2.WAD",
];

fn find_wad(explicit: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(p) = explicit.or_else(|| std::env::var_os("DOOM_WAD").map(PathBuf::from)) {
        if !p.is_file() {
            bail!("WAD not found: {}", p.display());
        }
        return Ok(p.canonicalize()?);
    }
    let mut dirs = vec![std::env::current_dir()?];
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(PathBuf::from))
    {
        // target/{debug,release}/doom-mcp -> also check the crate root.
        dirs.extend(dir.ancestors().take(3).map(PathBuf::from));
    }
    for dir in &dirs {
        for name in WAD_NAMES {
            let p = dir.join(name);
            if p.is_file() {
                return Ok(p.canonicalize()?);
            }
        }
    }
    bail!("no WAD found; pass --wad PATH or set DOOM_WAD.\n\n{USAGE}")
}

/// Point fd 1 at stderr so the engine's printf chatter can't corrupt the
/// JSON-RPC stream, and return a handle to the real stdout for MCP.
fn take_stdout() -> Result<tokio::fs::File> {
    unsafe {
        let mcp_fd = libc::dup(1);
        if mcp_fd < 0 || libc::dup2(2, 1) < 0 {
            bail!(
                "failed to redirect stdout: {}",
                std::io::Error::last_os_error()
            );
        }
        Ok(tokio::fs::File::from_std(std::fs::File::from_raw_fd(
            mcp_fd,
        )))
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut wad = None;
    let mut extra = Vec::new();
    let mut viewer_port: u16 = 6660;
    let mut viewer_enabled = true;
    let mut auto_open = true;
    let mut realtime = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--wad" => wad = Some(PathBuf::from(args.next().context("--wad needs a path")?)),
            "--viewer-port" => {
                viewer_port = args
                    .next()
                    .context("--viewer-port needs a number")?
                    .parse()
                    .context("--viewer-port needs a number")?
            }
            "--no-viewer" => viewer_enabled = false,
            "--no-open" => auto_open = false,
            "--mode" => {
                realtime = match args.next().as_deref() {
                    Some("turn") => false,
                    Some("realtime") => true,
                    _ => bail!("--mode needs turn or realtime\n\n{USAGE}"),
                }
            }
            "-h" | "--help" => {
                eprintln!("{USAGE}");
                return Ok(());
            }
            "--" => extra.extend(args.by_ref()),
            other => bail!("unknown argument {other:?}\n\n{USAGE}"),
        }
    }
    let wad = find_wad(wad)?;

    let mcp_out = take_stdout()?;

    // The engine writes its config file and savegames to the working directory.
    let data_dir = std::env::temp_dir().join("doom-mcp");
    std::fs::create_dir_all(&data_dir)?;
    std::env::set_current_dir(&data_dir)?;

    let mut doom_args: Vec<String> = ["doom", "-iwad"].map(String::from).to_vec();
    doom_args.push(wad.to_string_lossy().into_owned());
    doom_args.extend(["-nogui", "-nosound", "-nomusic"].map(String::from));
    doom_args.extend(extra);

    // Start the viewer first so it receives the engine's very first frames.
    let viewer = if viewer_enabled {
        let url = viewer::start(viewer_port).await?;
        eprintln!("doom-mcp: spectator view at {url}");
        Some(mcp::common::ViewerConfig {
            url,
            auto_open,
            opened: Arc::new(AtomicBool::new(false)),
        })
    } else {
        None
    };

    eprintln!("doom-mcp: starting DOOM with {}", wad.display());
    let game = game::Game::spawn(doom_args, realtime)?;

    let transport = (tokio::io::stdin(), mcp_out);
    if realtime {
        eprintln!("doom-mcp: real-time mode");
        let core = mcp::common::Core {
            game,
            viewer,
            text: mcp::realtime::TEXT,
        };
        let service = mcp::realtime::RealtimeServer::new(core)
            .serve(transport)
            .await
            .context("starting MCP service")?;
        service.waiting().await?;
    } else {
        let core = mcp::common::Core {
            game,
            viewer,
            text: mcp::turn::TEXT,
        };
        let service = mcp::turn::TurnServer::new(core)
            .serve(transport)
            .await
            .context("starting MCP service")?;
        service.waiting().await?;
    }
    Ok(())
}
