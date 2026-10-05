use anyhow::Result;
use axum::extract::ws::{Message, WebSocket};
use directories::BaseDirs;
use futures_util::{SinkExt, StreamExt};
use portable_pty::{CommandBuilder, PtySize};
use serde::Deserialize;
use std::{
    io::{Read, Write},
    sync::{Arc, Mutex, mpsc},
    time::{Duration, Instant},
};
use tokio::sync::{OwnedSemaphorePermit, mpsc as async_mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::{auth::Session, server::App};

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Input {
    Input { data: String },
    Resize { cols: u16, rows: u16 },
}

struct Terminal {
    input: mpsc::SyncSender<Input>,
    output: async_mpsc::Receiver<Vec<u8>>,
    exited: oneshot::Receiver<()>,
    stop: CancellationToken,
    child: Arc<Mutex<Box<dyn portable_pty::Child + Send + Sync>>>,
}

impl Drop for Terminal {
    fn drop(&mut self) {
        self.stop.cancel();
        let child = self.child.clone();
        std::thread::spawn(move || {
            let mut child = child.lock().unwrap();
            // Serialize reaping and killing: never signal a reused Unix PID.
            if matches!(child.try_wait(), Ok(None)) {
                let _ = child.kill();
                let _ = child.wait();
            }
        });
    }
}

fn spawn(app: &App, permit: OwnedSemaphorePermit) -> Result<Terminal> {
    let pair = portable_pty::native_pty_system().openpty(PtySize {
        rows: 30,
        cols: 120,
        pixel_width: 0,
        pixel_height: 0,
    })?;
    // cmd.exe parses a forward slash in argv[0] as a switch even when
    // CreateProcess accepts that spelling as an executable path.
    #[cfg(windows)]
    let shell = {
        use std::os::windows::ffi::{OsStrExt, OsStringExt};
        std::ffi::OsString::from_wide(
            &app.shell
                .as_os_str()
                .encode_wide()
                .map(|c| if c == b'/' as u16 { b'\\' as u16 } else { c })
                .collect::<Vec<_>>(),
        )
    };
    #[cfg(not(windows))]
    let shell = app.shell.as_os_str();
    let mut cmd = CommandBuilder::new(shell);
    let basename = app
        .shell
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_lowercase();
    if basename.contains("powershell") || basename.starts_with("pwsh") {
        cmd.arg("-NoLogo");
    } else if !cfg!(windows) {
        cmd.arg("-i");
    }
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    cmd.env_remove("RWEBMAN_PASSWORD");
    if let Some(base) = BaseDirs::new() {
        cmd.cwd(base.home_dir());
    }
    let mut reader = pair.master.try_clone_reader()?;
    let mut writer = pair.master.take_writer()?;
    let child = Arc::new(Mutex::new(pair.slave.spawn_command(cmd)?));
    drop(pair.slave);
    let (input_tx, input_rx) = mpsc::sync_channel::<Input>(32);
    let (output_tx, output_rx) = async_mpsc::channel::<Vec<u8>>(32);
    let (exit_tx, exit_rx) = oneshot::channel();
    let stop = CancellationToken::new();
    let control_stop = stop.clone();
    std::thread::spawn(move || {
        while !control_stop.is_cancelled() {
            match input_rx.recv_timeout(Duration::from_millis(100)) {
                Ok(Input::Input { data }) => {
                    if let Err(error) = writer
                        .write_all(data.as_bytes())
                        .and_then(|_| writer.flush())
                    {
                        tracing::debug!(%error, "PTY input closed");
                        break;
                    }
                }
                Ok(Input::Resize { cols, rows }) => {
                    if let Err(error) = pair.master.resize(PtySize {
                        cols: cols.clamp(2, 500),
                        rows: rows.clamp(2, 200),
                        pixel_width: 0,
                        pixel_height: 0,
                    }) {
                        tracing::debug!(%error, "PTY resize failed");
                        break;
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        drop(writer);
        drop(pair.master);
    });
    std::thread::spawn(move || {
        let mut bytes = [0u8; 8192];
        loop {
            match reader.read(&mut bytes) {
                Ok(0) => {
                    tracing::debug!("PTY output reached EOF");
                    break;
                }
                Err(error) => {
                    tracing::debug!(%error, "PTY output closed");
                    break;
                }
                Ok(size) => {
                    if output_tx.blocking_send(bytes[..size].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });
    let wait_child = child.clone();
    std::thread::spawn(move || {
        let _permit = permit;
        loop {
            let result = wait_child.lock().unwrap().try_wait();
            match result {
                Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                result => {
                    tracing::debug!(?result, "Shell exited");
                    break;
                }
            }
        }
        let _ = exit_tx.send(());
    });
    Ok(Terminal {
        input: input_tx,
        output: output_rx,
        exited: exit_rx,
        stop,
        child,
    })
}

pub async fn run(
    mut socket: WebSocket,
    app: Arc<App>,
    session: Arc<Session>,
    permit: OwnedSemaphorePermit,
) {
    if session.cancelled.is_cancelled() || session.expires <= Instant::now() {
        return;
    }
    let result = tokio::task::spawn_blocking(move || spawn(&app, permit)).await;
    let mut terminal = match result {
        Ok(Ok(terminal)) => terminal,
        failure => {
            let detail = match failure {
                Ok(Err(e)) => e.to_string(),
                Err(e) => e.to_string(),
                _ => unreachable!(),
            };
            tracing::error!(error = %detail, "Could not start terminal");
            let _ = socket.send(Message::Text("{\"type\":\"error\",\"message\":\"Unable to start the shell; check the server log\"}".into())).await;
            return;
        }
    };
    let (mut sender, mut receiver) = socket.split();
    let expires = tokio::time::Instant::from_std(session.expires);
    let mut heartbeat = tokio::time::interval(Duration::from_secs(20));
    let mut last_seen = Instant::now();
    loop {
        tokio::select! {
            _ = session.cancelled.cancelled() => break,
            _ = tokio::time::sleep_until(expires) => break,
            output = terminal.output.recv() => {
                let Some(bytes) = output else { break; };
                if !matches!(tokio::time::timeout(Duration::from_secs(5), sender.send(Message::Binary(bytes.into()))).await, Ok(Ok(()))) { break; }
            }
            _ = &mut terminal.exited => {
                while let Ok(Some(bytes)) = tokio::time::timeout(Duration::from_millis(150), terminal.output.recv()).await {
                    if !matches!(tokio::time::timeout(Duration::from_secs(2), sender.send(Message::Binary(bytes.into()))).await, Ok(Ok(()))) { break; }
                }
                break;
            }
            _ = heartbeat.tick() => {
                if last_seen.elapsed() > Duration::from_secs(60) { break; }
                if !matches!(tokio::time::timeout(Duration::from_secs(5), sender.send(Message::Ping(Vec::new().into()))).await, Ok(Ok(()))) { break; }
            }
            message = receiver.next() => {
                last_seen = Instant::now();
                match message {
                    Some(Ok(Message::Text(text))) => {
                        let Ok(input) = serde_json::from_str::<Input>(&text) else { tracing::debug!("Invalid terminal input message"); break; };
                        if matches!(&input, Input::Input { data } if data.len() > 8192) { break; }
                        if terminal.input.try_send(input).is_err() { break; }
                    }
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => {},
                    _ => break,
                }
            }
        }
    }
    drop(terminal);
    let _ = tokio::time::timeout(Duration::from_secs(1), sender.close()).await;
}
