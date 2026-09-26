//! A Unix socket through which `gemini-dictation ctl` controls the running instance, so that
//! compositor key bindings can be used instead of, or next to, the evdev push-to-talk key.

use std::env;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc::UnboundedSender;

use crate::cli::CtlAction;

fn socket_path() -> PathBuf {
    let dir = env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(env::temp_dir);
    dir.join("gemini-dictation.sock")
}

/// Listens on the socket until dropped, which removes it.
pub struct Server {
    path: PathBuf,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

pub async fn serve(actions: UnboundedSender<CtlAction>) -> Result<Server> {
    let path = socket_path();
    let display = path.display();
    if UnixStream::connect(&path).await.is_ok() {
        bail!("gemini-dictation is already running ({display} is in use)");
    }
    let _ = fs::remove_file(&path);
    let listener =
        UnixListener::bind(&path).with_context(|| format!("cannot listen on {display}"))?;
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    tokio::spawn(handle(stream, actions.clone()));
                }
                Err(err) => {
                    tracing::warn!("cannot accept a control connection: {err}");
                    return;
                }
            }
        }
    });
    Ok(Server { path })
}

async fn handle(stream: UnixStream, actions: UnboundedSender<CtlAction>) {
    let (reader, mut writer) = stream.into_split();
    let mut line = String::new();
    let reply = match BufReader::new(reader).read_line(&mut line).await {
        Ok(_) => match CtlAction::parse(line.trim()) {
            Some(action) if actions.send(action).is_ok() => "ok".to_owned(),
            Some(_) => "error: shutting down".to_owned(),
            None => format!("error: unknown action {:?}", line.trim()),
        },
        Err(err) => format!("error: {err}"),
    };
    let _ = writer.write_all(format!("{reply}\n").as_bytes()).await;
}

/// Sends `action` to the running instance.
pub async fn send(action: CtlAction) -> Result<()> {
    let path = socket_path();
    let stream = match UnixStream::connect(&path).await {
        Ok(stream) => stream,
        Err(err) => bail!(
            "cannot connect to {} ({err}); is gemini-dictation running?",
            path.display()
        ),
    };
    let (reader, mut writer) = stream.into_split();
    writer
        .write_all(format!("{}\n", action.as_str()).as_bytes())
        .await?;
    let mut reply = String::new();
    BufReader::new(reader).read_line(&mut reply).await?;
    match reply.trim() {
        "ok" => Ok(()),
        // The instance may exit before its reply is written.
        "" if action == CtlAction::Quit => Ok(()),
        "" => bail!("gemini-dictation closed the connection"),
        reply => bail!("gemini-dictation replied: {reply}"),
    }
}
