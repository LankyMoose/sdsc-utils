//! Local IPC transport (length-prefixed JSON).
//!
//! Uses a loopback TCP listener whose port is published under the app data dir
//! so the shell and CLI can attach. The frame protocol matches the named-pipe
//! plan (`u32` LE length + JSON). Supports multiple concurrent clients so the
//! CLI can talk to the service while the shell stays connected.

use super::{SHELL_PIPE_ENV, ServiceMessage, ShellCommand, recv_message, send_message};
use crate::controller::model::ControllerStatus;
use crate::persist::paths;
use std::io::{BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

static BOUND_PORT: AtomicU16 = AtomicU16::new(0);

/// Port the service listener is bound to (0 before start).
pub fn bound_port() -> u16 {
    BOUND_PORT.load(Ordering::SeqCst)
}

fn port_file() -> std::path::PathBuf {
    paths::data_dir().join("ipc-port")
}

fn write_port(port: u16) -> Result<(), String> {
    let path = port_file();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(&path, port.to_string()).map_err(|e| e.to_string())
}

fn read_port() -> Result<u16, String> {
    if let Ok(endpoint) = std::env::var(SHELL_PIPE_ENV) {
        // Service sets this to the decimal port for the shell child.
        if let Ok(port) = endpoint.parse::<u16>() {
            if port > 1 {
                return Ok(port);
            }
        }
        if let Some(port) = endpoint.rsplit(':').next().and_then(|s| s.parse().ok()) {
            if port > 1 {
                return Ok(port);
            }
        }
    }
    let bound = BOUND_PORT.load(Ordering::SeqCst);
    if bound > 1 {
        return Ok(bound);
    }
    let text = std::fs::read_to_string(port_file()).map_err(|e| e.to_string())?;
    text.trim()
        .parse()
        .map_err(|_| format!("invalid ipc port file: {text}"))
}

type ClientList = Arc<Mutex<Vec<Sender<ServiceMessage>>>>;

/// Accepts shell/CLI clients; reconnects after disconnect. Broadcasts to all.
pub struct PipeServer {
    from_client: Receiver<ShellCommand>,
    clients: ClientList,
}

impl PipeServer {
    pub fn start() -> Result<Self, String> {
        let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();
        BOUND_PORT.store(port, Ordering::SeqCst);
        write_port(port)?;
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let clients: ClientList = Arc::new(Mutex::new(Vec::new()));
        let clients_accept = Arc::clone(&clients);
        thread::Builder::new()
            .name("sdsc-ipc-server".into())
            .spawn(move || accept_loop(listener, cmd_tx, clients_accept))
            .map_err(|e| e.to_string())?;
        Ok(Self {
            from_client: cmd_rx,
            clients,
        })
    }

    pub fn try_recv_command(&self) -> Option<ShellCommand> {
        match self.from_client.try_recv() {
            Ok(cmd) => Some(cmd),
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => None,
        }
    }

    pub fn send(&self, msg: ServiceMessage) -> Result<(), String> {
        let Ok(mut guard) = self.clients.lock() else {
            return Err("ipc client list poisoned".into());
        };
        guard.retain(|tx| tx.send(msg.clone()).is_ok());
        if guard.is_empty() {
            // No shell attached yet — not an error (startup / restart window).
            return Ok(());
        }
        Ok(())
    }

    pub fn client_count(&self) -> usize {
        self.clients.lock().map(|g| g.len()).unwrap_or(0)
    }

    /// Cloneable handle for background threads (pad-edge forwarder).
    pub fn handle(&self) -> PipeServerHandle {
        PipeServerHandle {
            clients: Arc::clone(&self.clients),
        }
    }
}

/// Broadcast handle that can be moved into worker threads.
#[derive(Clone)]
pub struct PipeServerHandle {
    clients: ClientList,
}

impl PipeServerHandle {
    pub fn send(&self, msg: ServiceMessage) -> Result<(), String> {
        let Ok(mut guard) = self.clients.lock() else {
            return Err("ipc client list poisoned".into());
        };
        guard.retain(|tx| tx.send(msg.clone()).is_ok());
        Ok(())
    }
}

fn accept_loop(listener: TcpListener, cmd_tx: Sender<ShellCommand>, clients: ClientList) {
    let _ = listener.set_nonblocking(false);
    loop {
        let Ok((stream, _)) = listener.accept() else {
            thread::sleep(Duration::from_millis(50));
            continue;
        };
        let _ = stream.set_nodelay(true);
        let mut writer = match stream.try_clone() {
            Ok(s) => s,
            Err(_) => continue,
        };
        let mut reader = BufReader::new(stream);
        let (msg_tx, msg_rx) = mpsc::channel::<ServiceMessage>();
        if let Ok(mut guard) = clients.lock() {
            guard.push(msg_tx);
        }
        let cmd_tx2 = cmd_tx.clone();
        thread::spawn(move || {
            loop {
                match recv_message::<_, ShellCommand>(&mut reader) {
                    Ok(cmd) => {
                        if cmd_tx2.send(cmd).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        thread::spawn(move || {
            while let Ok(msg) = msg_rx.recv() {
                let shutdown = matches!(msg, ServiceMessage::Shutdown);
                if send_message(&mut writer, &msg).is_err() {
                    break;
                }
                let _ = writer.flush();
                if shutdown {
                    break;
                }
            }
        });
    }
}

/// Client connection used by the shell / CLI.
pub struct PipeClient {
    stream: TcpStream,
}

impl PipeClient {
    pub fn connect() -> Result<Self, String> {
        let port = read_port()?;
        let stream =
            TcpStream::connect(("127.0.0.1", port)).map_err(|e| format!("ipc connect: {e}"))?;
        let _ = stream.set_nodelay(true);
        Ok(Self { stream })
    }

    pub fn send_command(&mut self, cmd: &ShellCommand) -> Result<(), String> {
        send_message(&mut self.stream, cmd)
    }

    pub fn recv(&mut self) -> Result<ServiceMessage, String> {
        recv_message(&mut self.stream)
    }

    pub fn try_clone_reader(&self) -> Result<PipeClientReader, String> {
        let stream = self.stream.try_clone().map_err(|e| e.to_string())?;
        Ok(PipeClientReader {
            reader: BufReader::new(stream),
        })
    }

    pub fn request_list_controllers(&mut self) -> Result<Vec<ControllerStatus>, String> {
        self.send_command(&ShellCommand::ListControllers)?;
        match super::recv_matching(self, |m| matches!(m, ServiceMessage::ControllerList(_)))? {
            ServiceMessage::ControllerList(result) => result,
            other => Err(format!("unexpected ipc reply: {other:?}")),
        }
    }
}

pub struct PipeClientReader {
    reader: BufReader<TcpStream>,
}

impl PipeClientReader {
    pub fn recv(&mut self) -> Result<ServiceMessage, String> {
        recv_message(&mut self.reader)
    }
}
