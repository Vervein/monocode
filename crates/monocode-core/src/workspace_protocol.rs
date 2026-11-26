//! Environment workspace protocol: the shared contract between a workspace
//! owner (the desktop's GPUI process today, an independent service later),
//! the Node host gateway in front of it, and remote desktop clients.
//!
//! Everything crossing a process boundary here is JSON. These types pin the
//! envelope shapes; catalog entries and command payloads stay `Value` so the
//! owner's projections can evolve without breaking the wire.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::PathBuf;

pub const WORKSPACE_SCHEMA_VERSION: u32 = 1;

/// RPC method names carried over the host's `/rpc` tunnel and, with the same
/// names, over the local workspace IPC socket.
pub mod methods {
    pub const DESCRIBE: &str = "workspace.describe";
    pub const CATALOG: &str = "workspace.catalog";
    pub const CHANGES: &str = "workspace.changes";
    pub const MONO_GET: &str = "monos.get";
    pub const MONO_PROFILE: &str = "monos.profile";
    pub const MONO_HISTORY: &str = "monos.history";
    pub const MONO_FILE: &str = "monos.file";
    pub const COMMANDS_DISPATCH: &str = "workspace.commands.dispatch";
    pub const COMMANDS_GET: &str = "workspace.commands.get";
    /// Client-initiated binding when several workspace owners are candidates.
    pub const BIND: &str = "workspace.bind";

    pub const ALL: &[&str] = &[
        DESCRIBE,
        CATALOG,
        CHANGES,
        MONO_GET,
        MONO_PROFILE,
        MONO_HISTORY,
        MONO_FILE,
        COMMANDS_DISPATCH,
        COMMANDS_GET,
        BIND,
    ];
}

/// Feature gates announced in `environment.describe` and the workspace
/// descriptor. A missing capability disables the matching control; clients
/// must never fall back to local execution for a remote object.
pub mod capabilities {
    pub const CATALOG: &str = "workspace.catalog.v1";
    pub const CHANGES: &str = "workspace.changes.v1";
    pub const MONO_READ: &str = "mono.read.v1";
    pub const MONO_SEND: &str = "mono.send.v1";
    pub const MONO_PROJECT_LINKS: &str = "mono.projectLinks.v1";
    pub const MONO_FILES: &str = "mono.files.v1";
    pub const MONO_APPROVALS: &str = "mono.approvals.v1";
    pub const RECEIPTS: &str = "workspace.receipts.v1";
}

/// The workspace half of `environment.describe`. `workspaceSourceId` pins the
/// real data source so a reachable host can never silently present a
/// different set of Monos after a reinstall or rebuild.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceDescriptor {
    pub workspace_source_id: Option<String>,
    pub owner_mode: OwnerMode,
    pub owner_epoch: Option<String>,
    pub data_schema_version: Option<u32>,
    pub readiness: WorkspaceReadiness,
    pub capabilities: Vec<String>,
    /// Candidates seen while unbound, so the client can offer a one-time choice.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub candidates: Vec<WorkspaceCandidate>,
    /// Human-readable reason the owner is unavailable, when it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OwnerMode {
    Desktop,
    Service,
    Unbound,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceReadiness {
    Ready,
    Starting,
    OwnerUnavailable,
    Unbound,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceCandidate {
    pub workspace_source_id: String,
    pub owner_mode: OwnerMode,
    pub app_version: Option<String>,
    pub started_at: u64,
}

/// A catalog page. First-version owners always return a single complete page
/// (`complete: true`, no `nextPageToken`); the fields exist so paging can be
/// added without a protocol break. A paged response must pin every page to
/// the same `snapshotId`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogPage {
    pub source_id: String,
    pub owner_epoch: String,
    pub catalog_revision: u64,
    pub snapshot_id: String,
    pub complete: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_page_token: Option<String>,
    pub projects: Vec<Value>,
    pub monos: Vec<Value>,
    pub sessions: Vec<Value>,
    /// References into other environments (e.g. a Mono linked to a `remote://`
    /// project). Shown read-only; never operable in this environment.
    #[serde(default)]
    pub external_references: Vec<Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ChangesAnswer {
    Unchanged {
        catalog_revision: u64,
    },
    /// Anything changed: refetch the catalog. First-version owners report
    /// coarse invalidation only; fine-grained entries arrive with a later
    /// schema version.
    Reset {
        catalog_revision: u64,
        owner_epoch: String,
    },
}

/// The mutation envelope every workspace write travels in. `commandId` is
/// client-stable across retries; the owner deduplicates on it together with
/// the canonical signature, so a retry of the same logical command returns
/// the original receipt while any business-field change is rejected.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceCommand {
    pub command_id: String,
    pub environment_id: String,
    pub workspace_source_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_owner_epoch: Option<String>,
    pub target: CommandTarget,
    pub action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default)]
    pub payload: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandTarget {
    pub kind: String,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
}

/// Deterministic JSON: object keys sorted by UTF-8 bytes at every depth, no
/// insignificant whitespace. Only the owner computes signatures, so the wire
/// never depends on one key order surviving re-serialization in the gateway.
pub fn canonical_json(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let entries: Vec<String> = keys
                .into_iter()
                .map(|key| {
                    format!(
                        "{}:{}",
                        serde_json::to_string(key).unwrap_or_default(),
                        canonical_json(&map[key])
                    )
                })
                .collect();
            format!("{{{}}}", entries.join(","))
        }
        Value::Array(items) => {
            let items: Vec<String> = items.iter().map(canonical_json).collect();
            format!("[{}]", items.join(","))
        }
        _ => serde_json::to_string(value).unwrap_or_default(),
    }
}

/// sha256 of the canonical form, hex-encoded. Used for content revisioning
/// and command signatures alike.
pub fn canonical_hash(value: &Value) -> String {
    let digest = Sha256::digest(canonical_json(value).as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Canonical business signature of a command. Non-business metadata (tracing,
/// client timestamps) must not be part of the envelope at all, so the whole
/// typed envelope is covered.
pub fn command_signature(command: &WorkspaceCommand) -> String {
    let canonical = serde_json::json!({
        "environmentId": command.environment_id,
        "workspaceSourceId": command.workspace_source_id,
        "expectedOwnerEpoch": command.expected_owner_epoch,
        "target": command.target,
        "action": command.action,
        "expectedRevision": command.expected_revision,
        "runId": command.run_id,
        "payload": command.payload,
    });
    let digest = Sha256::digest(canonical_json(&canonical).as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReceiptStatus {
    /// Persistently accepted by the owner; effects may still be in flight.
    Accepted,
    /// The command's own definition of done, e.g. queued or linked. Never a
    /// claim that an agent task finished.
    Completed,
    Rejected,
    Failed,
    /// A crash or disconnect left the effect undecidable.
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandReceipt {
    pub command_id: String,
    pub status: ReceiptStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub owner_epoch: String,
    pub updated_at: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ReceiptLookup {
    Found(CommandReceipt),
    /// Not in the owner's retention window; must not be read as "never ran".
    RetentionExpired,
    NotFound,
}

// MARK: owner discovery (IPC)

/// The file a workspace owner publishes for same-user discovery. The token
/// inside a user-only file authenticates the IPC handshake; possession of the
/// file contents is the credential.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackendDescriptorFile {
    pub version: u32,
    pub pid: u32,
    pub socket_path: PathBuf,
    pub token: String,
    pub workspace_source_id: String,
    pub owner_epoch: String,
    #[serde(default)]
    pub owner_mode: Option<String>,
    pub app_version: String,
    pub started_at: u64,
}

/// First frame every IPC client sends. Servers reject anything else first.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IpcHello {
    pub token: String,
    pub client: String,
    #[serde(default)]
    pub protocol: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IpcRequest {
    pub id: u64,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IpcResponse {
    pub id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub const IPC_PROTOCOL_VERSION: u32 = 1;
/// Catalogs embed many sessions; anything beyond this is a bug or an attack.
pub const IPC_MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;

/// Directory workspace owners publish descriptor files into. Same-user only;
/// the directory itself is created mode 0700.
pub fn workspace_runtime_dir() -> PathBuf {
    if let Some(path) = std::env::var_os("MONOCODE_WORKSPACE_DIR") {
        return PathBuf::from(path);
    }
    #[cfg(target_os = "macos")]
    {
        return PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
            .join("Library/Application Support/Monocode/runtime");
    }
    #[cfg(not(target_os = "macos"))]
    {
        if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
            let runtime = PathBuf::from(runtime);
            if runtime.is_absolute() {
                return runtime.join("monocode");
            }
        }
        PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
            .join(".local/share/monocode/runtime")
    }
}

pub fn workspace_backends_dir() -> PathBuf {
    workspace_runtime_dir().join("workspace-backends")
}

/// The owner's persisted source identity. Generated once when workspace
/// access is first enabled; read-only probing must never create it.
pub fn workspace_source_path(data_dir: &std::path::Path) -> PathBuf {
    data_dir.join("workspace-source.json")
}

pub fn read_workspace_source(data_dir: &std::path::Path) -> Option<String> {
    let raw = std::fs::read_to_string(workspace_source_path(data_dir)).ok()?;
    serde_json::from_str::<Value>(&raw).ok()?["workspaceSourceId"]
        .as_str()
        .map(str::to_owned)
        .filter(|id| !id.is_empty())
}

/// Creates the source file when missing. Returns the stable id either way.
pub fn ensure_workspace_source(data_dir: &std::path::Path) -> Result<String, String> {
    if let Some(existing) = read_workspace_source(data_dir) {
        return Ok(existing);
    }
    let id = uuid::Uuid::new_v4().to_string();
    let body = serde_json::json!({
        "version": 1,
        "workspaceSourceId": id,
        "createdAt": crate::time::now_ms(),
    });
    write_private_atomic(&workspace_source_path(data_dir), &body.to_string())?;
    Ok(id)
}

/// User-only file contents, replaced atomically so a concurrent reader never
/// sees a half-written descriptor or source file.
pub fn write_private_atomic(path: &std::path::Path, text: &str) -> Result<(), String> {
    use std::io::Write;
    let parent = path
        .parent()
        .ok_or_else(|| "Path has no parent directory".to_string())?;
    std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let temp = parent.join(format!(
        ".{}.tmp-{}",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    let write = (|| -> Result<(), String> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp).map_err(|error| error.to_string())?;
        file.write_all(text.as_bytes())
            .and_then(|()| file.sync_all())
            .map_err(|error| error.to_string())?;
        drop(file);
        std::fs::rename(&temp, path).map_err(|error| error.to_string())
    })();
    if write.is_err() {
        std::fs::remove_file(&temp).ok();
    }
    write
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn command(payload: Value) -> WorkspaceCommand {
        WorkspaceCommand {
            command_id: "cmd-1".into(),
            environment_id: "env".into(),
            workspace_source_id: "src".into(),
            expected_owner_epoch: Some("epoch".into()),
            target: CommandTarget {
                kind: "mono".into(),
                id: "m1".into(),
                backend: None,
            },
            action: "mono.send".into(),
            expected_revision: None,
            run_id: None,
            payload,
        }
    }

    #[test]
    fn command_signatures_are_stable_and_business_scoped() {
        let one = command(json!({"text":"hello","attachments":[]}));
        let again = command(json!({"attachments":[],"text":"hello"}));
        assert_eq!(command_signature(&one), command_signature(&again));
        let other = command(json!({"text":"bye"}));
        assert_ne!(command_signature(&one), command_signature(&other));
        let mut retargeted = command(json!({"text":"hello","attachments":[]}));
        retargeted.target.id = "m2".into();
        assert_ne!(command_signature(&one), command_signature(&retargeted));
    }

    #[test]
    fn catalog_round_trips_with_schema_fields() {
        let page = CatalogPage {
            source_id: "src".into(),
            owner_epoch: "epoch".into(),
            catalog_revision: 7,
            snapshot_id: "snap".into(),
            complete: true,
            next_page_token: None,
            projects: vec![json!({"id":"p"})],
            monos: vec![],
            sessions: vec![],
            external_references: vec![],
        };
        let raw = serde_json::to_string(&page).unwrap();
        let back: CatalogPage = serde_json::from_str(&raw).unwrap();
        assert_eq!(back, page);
        assert!(raw.contains("\"catalogRevision\":7"));
    }

    #[test]
    fn descriptor_file_round_trips() {
        let file = BackendDescriptorFile {
            version: 1,
            pid: 42,
            socket_path: PathBuf::from("/tmp/ws.sock"),
            token: "t".into(),
            workspace_source_id: "src".into(),
            owner_epoch: "epoch".into(),
            owner_mode: Some("desktop".into()),
            app_version: "0.16.0".into(),
            started_at: 10,
        };
        let back: BackendDescriptorFile =
            serde_json::from_str(&serde_json::to_string(&file).unwrap()).unwrap();
        assert_eq!(back, file);
    }
}
