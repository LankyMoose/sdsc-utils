//! Unix domain socket transport (non-Windows).
//!
//! Multi-client broadcast so CLI and shell can attach concurrently.

use super::{ServiceMessage, ShellCommand, pipe_endpoint, recv_message, send_message};
use crate::controller::model::ControllerStatus;
use std::io::{BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

type ClientList = Arc<Mutex<Vec<Sender<ServiceMessage>>>>;

pub struct PipeServer {
    from_client: Receiver<ShellCommand>,
    clients: ClientList,
}

impl PipeServer {
    pub fn start() -> Result<Self, String> {
        let endpoint = pipe_endpoint();
        let path = Path::new(&endpoint);
        if path.exists() {
            let _ = std::fs::remove_file(path);
        }
        let listener = UnixListener::bind(path).map_err(|e| e.to_string())?;
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

fn accept_loop(listener: UnixListener, cmd_tx: Sender<ShellCommand>, clients: ClientList) {
    loop {
        let Ok((stream, _)) = listener.accept() else {
            thread::sleep(Duration::from_millis(50));
            continue;
        };
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

pub struct PipeClient {
    stream: UnixStream,
}

impl PipeClient {
    pub fn connect() -> Result<Self, String> {
        let stream = UnixStream::connect(pipe_endpoint()).map_err(|e| e.to_string())?;
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
    reader: BufReader<UnixStream>,
}

impl PipeClientReader {
    pub fn recv(&mut self) -> Result<ServiceMessage, String> {
        recv_message(&mut self.reader)
    }
}
