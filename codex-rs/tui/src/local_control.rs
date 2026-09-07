//! Opt-in, same-user control of a live TUI. No conversation content is exposed.
use std::collections::VecDeque;
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

use codex_app_server_client::AppServerEvent;
use codex_app_server_protocol::ServerNotification;
use serde::Deserialize;
use serde_json::Value;
use serde_json::json;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::net::UnixListener;
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::task::JoinSet;
use uuid::Uuid;

#[path = "local_control_request.rs"]
mod request;
pub(crate) use request::EffortRequest;
pub(crate) use request::FastRequest;
pub(crate) use request::ModelRequest;
pub(crate) use request::SettingsChange;
pub(crate) use request::SettingsRequest;

#[derive(Debug, Deserialize)]
#[serde(tag = "method", deny_unknown_fields)]
enum Request {
    #[serde(rename = "status/read")]
    Status,
    #[serde(rename = "status/subscribe")]
    Subscribe,
    #[serde(rename = "effort/set")]
    Set(EffortRequest),
    #[serde(rename = "model/set")]
    Model(ModelRequest),
    #[serde(rename = "fast/set")]
    Fast(FastRequest),
    #[serde(rename = "request/read")]
    Read {
        #[serde(rename = "requestId")]
        request_id: Uuid,
    },
}

#[derive(Default)]
struct Requests(VecDeque<(SettingsRequest, Value)>);

impl Requests {
    fn enqueue(&mut self, request: SettingsRequest, tx: &mpsc::Sender<SettingsRequest>) -> Value {
        if let Some((original, result)) = self
            .0
            .iter()
            .find(|(r, _)| r.request_id == request.request_id)
        {
            return if original == &request {
                result.clone()
            } else {
                json!({"error":"request ID reused with different parameters"})
            };
        }
        if self.0.iter().any(|(_, r)| r["status"] == "pending") {
            return json!({"error":"another settings request is pending"});
        }
        if self.0.len() == 64 {
            self.0.pop_front();
        }
        let result = json!({"requestId":request.request_id,"status":"pending"});
        if tx.try_send(request.clone()).is_err() {
            return json!({"error":"TUI control queue unavailable"});
        }
        self.0.push_back((request, result.clone()));
        result
    }

    fn finish(&mut self, id: Uuid, result: Value) {
        if let Some((_, value)) = self.0.iter_mut().find(|(r, _)| r.request_id == id) {
            *value = json!({"requestId":id,"status":if result["uncertain"] == true {"unconfirmed"}else if result.get("error").is_some(){"rejected"}else{"applied"},"outcome":result});
        }
    }
}

pub(crate) struct LocalControl {
    directory: PathBuf,
    metadata: Value,
    snapshot: watch::Sender<Value>,
    requests: Arc<Mutex<Requests>>,
    pub commands: mpsc::Receiver<SettingsRequest>,
    pub pending: Option<(SettingsRequest, Value)>,
    pub submitted_at: Option<Instant>,
    listener: JoinHandle<()>,
    pub revision: u64,
    previous: Value,
}

impl LocalControl {
    pub fn start(home: &Path, terminal: String) -> io::Result<Self> {
        let root = home.join("tui-control");
        match std::fs::DirBuilder::new().mode(0o700).create(&root) {
            Ok(()) => (),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => (),
            Err(error) => return Err(error),
        }
        let info = std::fs::symlink_metadata(&root)?;
        // SAFETY: geteuid has no arguments or memory preconditions.
        let uid = unsafe { libc::geteuid() };
        if !info.is_dir() || info.uid() != uid || info.mode() & 0o077 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "TUI control directory must be private and owned by this user",
            ));
        }
        let instance = Uuid::new_v4();
        let directory = root.join(format!(
            "{}-{}",
            std::process::id(),
            &instance.to_string()[..8]
        ));
        std::fs::DirBuilder::new().mode(0o700).create(&directory)?;
        let path = directory.join("control.sock");
        let listener = match UnixListener::bind(&path) {
            Ok(listener) => listener,
            Err(error) => {
                let _ = std::fs::remove_dir(&directory);
                return Err(error);
            }
        };
        let metadata = json!({"protocolVersion":1,"instanceId":instance,"pid":std::process::id(),"terminal":terminal,"socket":path});
        let mut initial = metadata.clone();
        initial["ready"] = false.into();
        initial["threadId"] = Value::Null;
        initial["revision"] = 0.into();
        let (snapshot, _) = watch::channel(initial);
        let (tx, commands) = mpsc::channel(/*buffer*/ 8);
        let requests = Arc::new(Mutex::new(Requests::default()));
        let shared = Arc::clone(&requests);
        let state = snapshot.subscribe();
        let listener = tokio::spawn(async move {
            let mut clients = JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let Ok((stream, _)) = accepted else { break };
                        if clients.len() < 8 && stream.peer_cred().is_ok_and(|cred| cred.uid() == uid) {
                            clients.spawn(serve(stream, tx.clone(), state.clone(), Arc::clone(&shared)));
                        }
                    }
                    _ = clients.join_next(), if !clients.is_empty() => {}
                }
            }
        });
        Ok(Self {
            directory,
            metadata,
            snapshot,
            requests,
            commands,
            pending: None,
            submitted_at: None,
            listener,
            revision: 0,
            previous: Value::Null,
        })
    }

    pub fn publish(&mut self, state: Value) {
        if let Some((request, _)) = &self.pending
            && self
                .submitted_at
                .is_some_and(|at| at.elapsed() > Duration::from_secs(/*secs*/ 30))
        {
            self.finish(
                request.request_id,
                json!({"error":"settings confirmation unavailable", "uncertain":true}),
            );
        }
        if state == self.previous {
            return;
        }
        if [
            "threadId",
            "model",
            "effort",
            "serviceTier",
            "fastServiceTier",
            "collaborationMode",
            "supportedEfforts",
            "models",
            "ready",
            "focused",
        ]
        .iter()
        .any(|key| self.previous[*key] != state[*key])
        {
            self.revision += 1;
        }
        self.previous = state.clone();
        let mut snapshot = self.metadata.clone();
        let (Some(target), Some(state)) = (snapshot.as_object_mut(), state.as_object()) else {
            return;
        };
        target.extend(state.clone());
        snapshot["revision"] = self.revision.into();
        self.snapshot.send_replace(snapshot.clone());
        let temp = self.directory.join("session.tmp");
        if let Ok(data) = serde_json::to_vec(&snapshot)
            && std::fs::write(&temp, data).is_ok()
        {
            let _ = std::fs::rename(temp, self.directory.join("session.json"));
        }
    }

    pub fn finish(&mut self, id: Uuid, result: Value) {
        let Ok(mut requests) = self.requests.lock() else {
            return;
        };
        requests.finish(id, result);
        self.pending = None;
        self.submitted_at = None;
    }

    pub fn observe(&mut self, event: &AppServerEvent) {
        let Some((request, expected)) = &self.pending else {
            return;
        };
        match event {
            AppServerEvent::ServerNotification(notification) => match notification.as_ref() {
                ServerNotification::ThreadSettingsUpdated(updated)
                    if updated.thread_id == request.expected_thread_id =>
                {
                    let settings = &updated.thread_settings;
                    let actual = json!({"model":settings.model,"effort":settings.effort,
                        "serviceTier":settings.service_tier});
                    if expected.as_object().is_some_and(|fields| {
                        fields.iter().all(|(key, value)| actual[key] == *value)
                    }) {
                        let mut outcome = expected.clone();
                        outcome["threadId"] = json!(updated.thread_id);
                        self.finish(request.request_id, outcome);
                    }
                }
                ServerNotification::ThreadClosed(closed)
                    if closed.thread_id == request.expected_thread_id =>
                {
                    self.finish(
                        request.request_id,
                        json!({"error":"task closed before confirmation"}),
                    );
                }
                _ => (),
            },
            AppServerEvent::Disconnected { .. } | AppServerEvent::Lagged { .. } => {
                self.finish(request.request_id, json!({"error":"confirmation stream unavailable; read current settings","uncertain":true}));
            }
            AppServerEvent::ServerRequest(_) => (),
        }
    }
}

impl Drop for LocalControl {
    fn drop(&mut self) {
        self.listener.abort();
        let _ = std::fs::remove_file(self.directory.join("control.sock"));
        let _ = std::fs::remove_file(self.directory.join("session.json"));
        let _ = std::fs::remove_file(self.directory.join("session.tmp"));
        let _ = std::fs::remove_dir(&self.directory);
    }
}

async fn serve(
    stream: UnixStream,
    tx: mpsc::Sender<SettingsRequest>,
    mut state: watch::Receiver<Value>,
    requests: Arc<Mutex<Requests>>,
) -> io::Result<()> {
    let (read, mut write) = stream.into_split();
    let mut read = BufReader::new(read);
    loop {
        let line = tokio::time::timeout(Duration::from_secs(/*secs*/ 30), async {
            let mut line = Vec::new();
            loop {
                let byte = read.read_u8().await?;
                if byte == b'\n' {
                    return Ok::<_, io::Error>(line);
                }
                if line.len() == 4096 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "control frame too large",
                    ));
                }
                line.push(byte);
            }
        })
        .await??;
        let request = serde_json::from_slice::<Request>(&line);
        let subscribe = matches!(request, Ok(Request::Subscribe));
        let result = match request {
            Ok(Request::Status | Request::Subscribe) => state.borrow_and_update().clone(),
            Ok(Request::Set(request)) => requests
                .lock()
                .map_err(|_| io::Error::other("request state unavailable"))?
                .enqueue(request.into(), &tx),
            Ok(Request::Model(request)) => requests
                .lock()
                .map_err(|_| io::Error::other("request state unavailable"))?
                .enqueue(request.into(), &tx),
            Ok(Request::Fast(request)) => requests
                .lock()
                .map_err(|_| io::Error::other("request state unavailable"))?
                .enqueue(request.into(), &tx),
            Ok(Request::Read { request_id }) => requests
                .lock()
                .map_err(|_| io::Error::other("request state unavailable"))?
                .0
                .iter()
                .find(|(r, _)| r.request_id == request_id)
                .map(|(_, result)| result.clone())
                .unwrap_or_else(|| json!({"error":"unknown or expired request ID"})),
            Err(_) => json!({"error":"invalid control request"}),
        };
        tokio::time::timeout(
            Duration::from_secs(/*secs*/ 2),
            write.write_all(format!("{}\n", json!({"result":result})).as_bytes()),
        )
        .await??;
        if subscribe {
            while state.changed().await.is_ok() {
                let value = state.borrow_and_update().clone();
                tokio::time::timeout(
                    Duration::from_secs(/*secs*/ 2),
                    write.write_all(format!("{}\n", json!({"result":value})).as_bytes()),
                )
                .await??;
            }
            return Ok(());
        }
    }
}

#[cfg(test)]
#[path = "local_control_tests.rs"]
mod tests;
