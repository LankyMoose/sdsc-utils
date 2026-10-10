//! Unix domain socket transport (non-Windows).
//!
//! Multi-client broadcast so CLI and shell can attach concurrently.

use super::{ServiceMessage, ShellCommand, pipe_endpoint, recv_message, send_message};
use crate::controller::model::ControllerStatus;
use std::io::{BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// Monotonic connection ids. Starts at 1 so 0 stays a valid "nothing seen yet"
/// watermark for [`PipeServer::send_to_newcomers`].
static NEXT_CLIENT_ID: AtomicU64 = AtomicU64::new(1);

type ClientList = Arc<Mutex<Vec<ClientEntry>>>;

/// One attached transport: the shell opens a command connection plus a
/// broadcast-receiver connection, and the CLI attaches transiently.
struct ClientEntry {
    id: u64,
    tx: Sender<ServiceMessage>,
}

/// True when the service unix socket path exists (no connect — see win_pipe note).
pub fn service_endpoint_ready() -> bool {
    Path::new(&pipe_endpoint()).exists()
}

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
        guard.retain(|c| c.tx.send(msg.clone()).is_ok());
        Ok(())
    }

    /// Send `msgs` only to transports that connected after `watermark` last
    /// advanced, then advance it past every client seen.
    ///
    /// The shell's command connection always wins the attach race and nobody
    /// reads its socket, so the initial snapshot (Controllers, StartSelection,
    /// latched OpenStart) must go to the newcomer (the broadcast receiver
    /// landing second), not just on the 0 to 1 transition. Existing clients
    /// already hold that state (pushes are equivalence-gated there). Returns
    /// how many transports were messaged. Dead newcomers are pruned.
    pub fn send_to_newcomers(&self, msgs: &[ServiceMessage], watermark: &mut u64) -> usize {
        let Ok(mut guard) = self.clients.lock() else {
            return 0;
        };
        let mut reached = 0usize;
        let mut seen = *watermark;
        guard.retain(|c| {
            seen = seen.max(c.id);
            if c.id <= *watermark {
                return true;
            }
            for msg in msgs {
                if c.tx.send(msg.clone()).is_err() {
                    return false;
                }
            }
            reached += 1;
            true
        });
        *watermark = seen;
        reached
    }

    pub fn client_count(&self) -> usize {
        self.clients.lock().map(|g| g.len()).unwrap_or(0)
    }

    /// True when at least one connected transport is newer than `watermark` —
    /// a cheap pre-check so the service only builds the snapshot batch when
    /// [`PipeServer::send_to_newcomers`] would have someone to reach.
    pub fn has_newcomers(&self, watermark: u64) -> bool {
        self.clients
            .lock()
            .map(|g| g.iter().any(|c| c.id > watermark))
            .unwrap_or(false)
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
        guard.retain(|c| c.tx.send(msg.clone()).is_ok());
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
        let id = NEXT_CLIENT_ID.fetch_add(1, Ordering::SeqCst);
        let (msg_tx, msg_rx) = mpsc::channel::<ServiceMessage>();
        if let Ok(mut guard) = clients.lock() {
            guard.push(ClientEntry { id, tx: msg_tx });
        }
        let cmd_tx2 = cmd_tx.clone();
        let clients_reader = Arc::clone(&clients);
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
            // Peer went away: drop its broadcast queue promptly so
            // `client_count` stays honest. (The writer thread only notices on
            // its next write; pruning here cascades — its receiver errors and
            // it exits too.)
            if let Ok(mut guard) = clients_reader.lock() {
                guard.retain(|c| c.id != id);
            }
        });
        let clients_writer = Arc::clone(&clients);
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
            if let Ok(mut guard) = clients_writer.lock() {
                guard.retain(|c| c.id != id);
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;

    static TEST_SOCK_SEQ: AtomicU32 = AtomicU32::new(0);

    /// A live server bound to a unique temp socket (never the real endpoint).
    fn spawn_test_server() -> (PipeServer, std::path::PathBuf) {
        let seq = TEST_SOCK_SEQ.fetch_add(1, Ordering::SeqCst);
        let path =
            std::env::temp_dir().join(format!("sdsc-test-ipc-{}-{seq}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let sock: &Path = &path;
        let listener = UnixListener::bind(sock).expect("bind socket");
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let clients: ClientList = Arc::new(Mutex::new(Vec::new()));
        let accept_clients = Arc::clone(&clients);
        thread::Builder::new()
            .name("sdsc-ipc-test-server".into())
            .spawn(move || accept_loop(listener, cmd_tx, accept_clients))
            .expect("spawn accept loop");
        (
            PipeServer {
                from_client: cmd_rx,
                clients,
            },
            path,
        )
    }

    fn wait_for_count(server: &PipeServer, want: usize) {
        for _ in 0..200 {
            if server.client_count() == want {
                return;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(server.client_count(), want, "client count did not settle");
    }

    /// The cold-boot shape: first connection (shell command socket, never
    /// read) then the broadcast receiver. The newcomer sync must reach the
    /// receiver even though it never observes a 0→1 transition.
    #[test]
    fn newcomer_sync_reaches_late_receiver() {
        let (server, path) = spawn_test_server();
        let sock: &Path = &path;
        let unread = UnixStream::connect(sock).expect("connect cmd");
        let mut receiver = UnixStream::connect(sock).expect("connect recv");
        wait_for_count(&server, 2);

        let mut watermark = 0u64;
        let reached = server.send_to_newcomers(&[ServiceMessage::Ack], &mut watermark);
        assert_eq!(reached, 2);
        let msg: ServiceMessage = recv_message(&mut receiver).expect("recv broadcast");
        assert!(matches!(msg, ServiceMessage::Ack));

        // Watermark advanced: steady state sends nothing.
        assert_eq!(
            server.send_to_newcomers(&[ServiceMessage::Ack], &mut watermark),
            0
        );
        drop(unread);
        drop(receiver);
    }

    /// Reader EOF prunes promptly — no stale entries inflating `client_count`.
    #[test]
    fn disconnect_prunes_client_count() {
        let (server, path) = spawn_test_server();
        let sock: &Path = &path;
        let stream = UnixStream::connect(sock).expect("connect");
        wait_for_count(&server, 1);
        drop(stream);
        wait_for_count(&server, 0);
    }
}
