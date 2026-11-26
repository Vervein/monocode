//! Same-user workspace IPC: the owner's Unix socket a local gateway (the
//! Node host) proxies workspace requests through. newline-delimited JSON
//! frames, one `IpcRequest`/`IpcResponse` each; the first frame on a
//! connection must be the hello carrying the descriptor token.
//!
//! The transport is deliberately dumb: requests surface as events in one
//! channel, in connection order, and the application answers them whenever it
//! is ready. Replies for one connection are written in the order the
//! application produces them, so a slow read never blocks a fast one.

use crate::workspace_protocol::{
    ensure_workspace_source, workspace_runtime_dir, write_private_atomic, BackendDescriptorFile,
    IpcHello, IpcRequest, IpcResponse, IPC_MAX_MESSAGE_BYTES, IPC_PROTOCOL_VERSION,
};
use serde_json::Value;
use std::io::{BufReader, Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::Arc;

pub struct WorkspaceIpc {
    socket_path: PathBuf,
    descriptor_path: PathBuf,
    shutdown: Arc<AtomicBool>,
    source_id: String,
    epoch: String,
}

pub enum WorkspaceIpcEvent {
    Request(WorkspaceIpcRequest),
}

pub struct WorkspaceIpcRequest {
    pub id: u64,
    pub method: String,
    pub params: Value,
    replies: std_mpsc::Sender<IpcResponse>,
}

impl WorkspaceIpcRequest {
    pub fn reply(self, result: Result<Value, String>) {
        let response = match result {
            Ok(value) => IpcResponse {
                id: self.id,
                result: Some(value),
                error: None,
            },
            Err(error) => IpcResponse {
                id: self.id,
                result: None,
                error: Some(error),
            },
        };
        // A closed connection means nobody is listening; dropping the answer
        // is correct, the gateway reports the disconnect itself.
        let _ = self.replies.send(response);
    }
}

impl std::fmt::Debug for WorkspaceIpcRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkspaceIpcRequest")
            .field("id", &self.id)
            .field("method", &self.method)
            .finish_non_exhaustive()
    }
}

pub struct WorkspaceIpcInfo {
    pub source_id: String,
    pub owner_epoch: String,
    pub socket_path: PathBuf,
}

impl WorkspaceIpc {
    /// Binds the socket and publishes the descriptor file. The source id is
    /// created on first use: enabling workspace access is the explicit act
    /// that persists it, read-only probes never create one.
    #[cfg(unix)]
    pub fn open(
        data_dir: &std::path::Path,
        app_version: &str,
    ) -> Result<(Self, async_channel::Receiver<WorkspaceIpcEvent>), String> {
        Self::open_in(data_dir, app_version, workspace_runtime_dir())
    }

    /// `open` against an explicit runtime root; tests isolate theirs instead
    /// of mutating process-wide environment.
    #[cfg(unix)]
    pub fn open_in(
        data_dir: &std::path::Path,
        app_version: &str,
        runtime: PathBuf,
    ) -> Result<(Self, async_channel::Receiver<WorkspaceIpcEvent>), String> {
        use std::os::unix::net::UnixListener;

        let source_id = ensure_workspace_source(data_dir)?;
        let owner_epoch = uuid::Uuid::new_v4().to_string();
        let backends = runtime.join("workspace-backends");
        std::fs::create_dir_all(&backends).map_err(|error| error.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700)).ok();
            std::fs::set_permissions(&backends, std::fs::Permissions::from_mode(0o700)).ok();
        }

        let socket_path = runtime.join(format!("workspace-{}.sock", std::process::id()));
        // Our own stale socket from a crashed earlier run is safe to replace;
        // a live owner's connect would have failed before we got here.
        if socket_path.exists() {
            std::fs::remove_file(&socket_path).map_err(|error| error.to_string())?;
        }
        let listener = UnixListener::bind(&socket_path)
            .map_err(|error| format!("Cannot bind workspace socket: {error}"))?;
        listener
            .set_nonblocking(true)
            .map_err(|error| error.to_string())?;

        let token =
            uuid::Uuid::new_v4().simple().to_string() + &uuid::Uuid::new_v4().simple().to_string();
        let descriptor_path = backends.join(format!("{}.json", std::process::id()));
        let descriptor = BackendDescriptorFile {
            version: 1,
            pid: std::process::id(),
            socket_path: socket_path.clone(),
            token: token.clone(),
            workspace_source_id: source_id.clone(),
            owner_epoch: owner_epoch.clone(),
            owner_mode: Some("desktop".into()),
            app_version: app_version.into(),
            started_at: crate::time::now_ms(),
        };
        let body = serde_json::to_string(&descriptor).map_err(|error| error.to_string())?;
        write_private_atomic(&descriptor_path, &body)?;

        let (events, receiver) = async_channel::bounded(256);
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread_shutdown = shutdown.clone();
        std::thread::Builder::new()
            .name("monocode-workspace-ipc".into())
            .spawn(move || accept_loop(listener, token, events, thread_shutdown))
            .map_err(|error| format!("Workspace IPC could not start: {error}"))?;
        Ok((
            Self {
                socket_path,
                descriptor_path,
                shutdown,
                source_id,
                epoch: owner_epoch,
            },
            receiver,
        ))
    }

    #[cfg(not(unix))]
    pub fn open(
        _data_dir: &std::path::Path,
        _app_version: &str,
    ) -> Result<(Self, async_channel::Receiver<WorkspaceIpcEvent>), String> {
        Err("Workspace IPC uses Unix sockets on this platform's first version".into())
    }

    pub fn info(&self) -> WorkspaceIpcInfo {
        WorkspaceIpcInfo {
            source_id: self.source_id.clone(),
            owner_epoch: self.epoch.clone(),
            socket_path: self.socket_path.clone(),
        }
    }
}

impl Drop for WorkspaceIpc {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        std::fs::remove_file(&self.descriptor_path).ok();
        std::fs::remove_file(&self.socket_path).ok();
    }
}

#[cfg(unix)]
fn accept_loop(
    listener: std::os::unix::net::UnixListener,
    token: String,
    events: async_channel::Sender<WorkspaceIpcEvent>,
    shutdown: Arc<AtomicBool>,
) {
    while !shutdown.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => {
                let token = token.clone();
                let events = events.clone();
                std::thread::Builder::new()
                    .name("monocode-workspace-peer".into())
                    .spawn(move || serve_connection(stream, token, events))
                    .ok();
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
            Err(_) => std::thread::sleep(std::time::Duration::from_millis(500)),
        }
    }
}

#[cfg(unix)]
fn serve_connection(
    stream: std::os::unix::net::UnixStream,
    token: String,
    events: async_channel::Sender<WorkspaceIpcEvent>,
) {
    use std::os::unix::net::UnixStream;
    let writer_stream: UnixStream = match stream.try_clone() {
        Ok(clone) => clone,
        Err(_) => return,
    };
    let (replies, reply_source) = std_mpsc::channel::<IpcResponse>();
    let writer = std::thread::Builder::new()
        .name("monocode-workspace-write".into())
        .spawn(move || {
            let mut stream = writer_stream;
            while let Ok(response) = reply_source.recv() {
                let mut line = serde_json::to_vec(&response).unwrap_or_default();
                line.push(b'\n');
                if stream.write_all(&line).is_err() {
                    return;
                }
            }
        });

    let mut reader = BufReader::new(stream);
    // The hello frame is the only authentication: the token lives in a file
    // only this user can read, inside a directory only this user can list.
    let authenticated = match read_frame(&mut reader) {
        Ok(Some(line)) => serde_json::from_slice::<IpcHello>(&line)
            .map(|hello| hello.token == token && hello.protocol <= IPC_PROTOCOL_VERSION)
            .unwrap_or(false),
        _ => false,
    };
    if !authenticated {
        return;
    }
    while let Ok(Some(line)) = read_frame(&mut reader) {
        let request = match serde_json::from_slice::<IpcRequest>(&line) {
            Ok(request) if !request.method.is_empty() => request,
            _ => continue,
        };
        let event = WorkspaceIpcEvent::Request(WorkspaceIpcRequest {
            id: request.id,
            method: request.method,
            params: request.params,
            replies: replies.clone(),
        });
        // Backpressure: a full application queue parks this connection's
        // reader instead of growing memory. Other connections keep flowing.
        if events.send_blocking(event).is_err() {
            break;
        }
    }
    drop(replies);
    if let Ok(handle) = writer {
        handle.join().ok();
    }
}

/// One newline-terminated frame with a hard size cap. `Ok(None)` is EOF.
fn read_frame(reader: &mut BufReader<impl Read>) -> Result<Option<Vec<u8>>, std::io::Error> {
    let mut bytes = Vec::new();
    loop {
        let mut byte = [0u8; 1];
        let read = reader.read(&mut byte)?;
        if read == 0 {
            if bytes.is_empty() {
                return Ok(None);
            }
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "connection closed mid-frame",
            ));
        }
        if byte[0] == b'\n' {
            return Ok(Some(bytes));
        }
        bytes.push(byte[0]);
        if bytes.len() > IPC_MAX_MESSAGE_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "workspace IPC frame too large",
            ));
        }
    }
}

/// Client half for tests and the diagnostic harness. The real gateway client
/// is the Node host's TypeScript implementation.
#[cfg(unix)]
pub struct WorkspaceIpcClient {
    reader: BufReader<std::os::unix::net::UnixStream>,
    writer: std::os::unix::net::UnixStream,
    next_id: u64,
}

#[cfg(unix)]
impl WorkspaceIpcClient {
    pub fn connect(socket_path: &std::path::Path, token: &str) -> Result<Self, String> {
        use std::os::unix::net::UnixStream;
        let stream = UnixStream::connect(socket_path).map_err(|error| error.to_string())?;
        let mut writer = stream.try_clone().map_err(|error| error.to_string())?;
        let hello = serde_json::to_vec(&IpcHello {
            token: token.into(),
            client: "rust-test".into(),
            protocol: IPC_PROTOCOL_VERSION,
        })
        .map_err(|error| error.to_string())?;
        writer
            .write_all(&hello)
            .and_then(|()| writer.write_all(b"\n"))
            .map_err(|error| error.to_string())?;
        Ok(Self {
            reader: BufReader::new(stream),
            writer,
            next_id: 0,
        })
    }

    pub fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.next_id += 1;
        let id = self.next_id;
        let request = IpcRequest {
            id,
            method: method.into(),
            params,
        };
        let mut line = serde_json::to_vec(&request).map_err(|error| error.to_string())?;
        line.push(b'\n');
        self.writer
            .write_all(&line)
            .map_err(|error| error.to_string())?;
        loop {
            let frame = read_frame(&mut self.reader)
                .map_err(|error| error.to_string())?
                .ok_or("Workspace owner closed the connection")?;
            let response: IpcResponse =
                serde_json::from_slice(&frame).map_err(|error| error.to_string())?;
            if response.id != id {
                continue;
            }
            if let Some(error) = response.error {
                return Err(error);
            }
            return Ok(response.result.unwrap_or(Value::Null));
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("monocode-ws-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn authenticated_requests_round_trip() {
        let data = temp_dir();
        let runtime = data.join("runtime");
        let (server, events) = WorkspaceIpc::open_in(&data, "test", runtime.clone()).unwrap();
        let info = server.info();
        let descriptor: BackendDescriptorFile = serde_json::from_str(
            &std::fs::read_to_string(
                runtime
                    .join("workspace-backends")
                    .join(format!("{}.json", std::process::id())),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(descriptor.workspace_source_id, info.source_id);
        assert_eq!(descriptor.socket_path, info.socket_path);

        let answering = std::thread::spawn(move || {
            let mut answers = Vec::new();
            while let Ok(event) = events.recv_blocking() {
                let WorkspaceIpcEvent::Request(request) = event;
                let method = request.method.clone();
                let params = request.params.clone();
                answers.push(method);
                request.reply(Ok(json!({"echo": params})));
                if answers.len() == 2 {
                    break;
                }
            }
            answers
        });

        let mut client = WorkspaceIpcClient::connect(&info.socket_path, &descriptor.token).unwrap();
        let answer = client.request("workspace.describe", json!({})).unwrap();
        assert_eq!(answer["echo"], json!({}));
        client.request("workspace.catalog", json!({"x":1})).unwrap();
        assert_eq!(
            answering.join().unwrap(),
            ["workspace.describe", "workspace.catalog"]
        );

        // A wrong token never reaches the application: the server closes the
        // connection, so the first request dies unanswered.
        if let Ok(mut bad) = WorkspaceIpcClient::connect(&info.socket_path, "wrong") {
            assert!(bad.request("workspace.describe", json!({})).is_err());
        }
        std::fs::remove_dir_all(&data).ok();
    }

    #[test]
    fn source_ids_are_stable_across_reopens() {
        let data = temp_dir();
        let first = ensure_workspace_source(&data).unwrap();
        let second = ensure_workspace_source(&data).unwrap();
        assert_eq!(first, second);
        std::fs::remove_dir_all(&data).ok();
    }
}
