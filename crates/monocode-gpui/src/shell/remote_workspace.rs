//! The client side of a remote workspace: for a connected machine, keeps a
//! local replica of the owner's catalog current and answers reads from it.
//! The replica is never authority — every mutation goes through workspace
//! commands and the owner's receipt is the only truth about acceptance.
use super::*;
use serde_json::{Value, json};
use std::collections::HashMap;

/// How often an idle replica checks for catalog invalidation. The owner
/// rebuilds its projection on demand, so polling is cheap correctness rather
/// than a substitute for a push channel the protocol does not have yet.
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(3);

#[derive(Clone, Debug, Default)]
pub(super) struct WorkspaceReplica {
    /// The host-reported workspace section of environment.describe.
    pub(super) readiness: String,
    pub(super) source_id: Option<String>,
    pub(super) owner_epoch: String,
    pub(super) capabilities: Vec<String>,
    /// The last complete catalog page, exactly as the owner projected it.
    pub(super) catalog: Option<Value>,
    pub(super) catalog_revision: u64,
    /// The machine is reachable but its workspace is not (unbound, owner
    /// down): distinct from a transport failure, which keeps the replica.
    pub(super) detail: String,
    pub(super) offline: bool,
}

impl WorkspaceReplica {
    /// True when the remote workspace answers and the replica holds a
    /// complete catalog to scope the rails with.
    pub(super) fn usable(&self) -> bool {
        self.readiness == "ready" && self.catalog.is_some()
    }
}

#[derive(Default)]
pub(super) struct RemoteWorkspaces {
    pub(super) replicas: HashMap<String, WorkspaceReplica>,
    /// Which environment's workspace currently scopes the projects and Monos
    /// rails. None is this machine itself — the local workspace the shell has
    /// always shown.
    pub(super) selected: Option<String>,
}

impl Shell {
    /// Starts the sync loop for a machine's workspace. Idempotent per
    /// machine: re-entry only re-homes the loop when the machine changed.
    pub(super) fn sync_remote_workspace(&mut self, machine_id: &str, cx: &mut Context<Self>) {
        let machine_id = machine_id.to_string();
        self.remote_workspaces
            .replicas
            .entry(machine_id.clone())
            .or_default();
        let remote = self.remote.clone();
        cx.spawn(async move |this, cx| {
            loop {
                let machine = machine_id.clone();
                let remote = remote.clone();
                let outcome = cx
                    .background_executor()
                    .spawn(async move { poll_workspace_once(&remote, &machine) })
                    .await;
                if this
                    .update(cx, |this, cx| {
                        this.apply_workspace_poll(&machine_id, outcome, cx)
                    })
                    .is_err()
                {
                    break;
                }
                cx.background_executor().timer(POLL_INTERVAL).await;
            }
        })
        .detach();
    }

    fn apply_workspace_poll(
        &mut self,
        machine_id: &str,
        outcome: Result<Poll, String>,
        cx: &mut Context<Self>,
    ) {
        let replica = self.remote_workspaces.replicas.entry(machine_id.into()).or_default();
        match outcome {
            Ok(poll) => {
                replica.offline = false;
                replica.readiness = poll.readiness;
                replica.source_id = poll.source_id;
                replica.owner_epoch = poll.owner_epoch;
                replica.capabilities = poll.capabilities;
                replica.detail = poll.detail;
                if let Some(catalog) = poll.catalog {
                    replica.catalog_revision = catalog
                        .get("catalogRevision")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    replica.catalog = Some(catalog);
                }
            }
            Err(error) => {
                // A transport failure never discards the last good replica:
                // the rails keep showing what the workspace held, marked as
                // disconnected rather than empty.
                replica.offline = true;
                replica.detail = error;
            }
        }
        cx.notify();
    }

    /// The projects a machine's workspace reports, as the catalog projects
    /// them. Empty both for an empty workspace and for one that has not
    /// synced yet; the readiness and detail say which.
    pub(super) fn remote_workspace_catalog(&self, machine_id: &str) -> Option<&Value> {
        self.remote_workspaces.replicas.get(machine_id)?.catalog.as_ref()
    }
}

struct Poll {
    readiness: String,
    source_id: Option<String>,
    owner_epoch: String,
    capabilities: Vec<String>,
    detail: String,
    catalog: Option<Value>,
}

/// One synchronisation round: describe the environment, then refill the
/// replica when the owner's catalog moved. Blocking host RPCs stay on the
/// background executor; the shell only ever applies their results.
fn poll_workspace_once(remote: &Arc<monocode_core::remote::Connections>, machine_id: &str) -> Result<Poll, String> {
    let describe = remote.request(machine_id, "environment.describe", json!({}))?;
    let workspace = &describe["workspace"];
    let readiness = workspace["readiness"].as_str().unwrap_or("unbound").to_string();
    let capabilities: Vec<String> = workspace["capabilities"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect();
    let mut poll = Poll {
        readiness: readiness.clone(),
        source_id: workspace["workspaceSourceId"].as_str().map(str::to_owned),
        owner_epoch: workspace["ownerEpoch"].as_str().unwrap_or_default().to_string(),
        capabilities: capabilities.clone(),
        detail: workspace["detail"].as_str().unwrap_or_default().to_string(),
        catalog: None,
    };
    if readiness != "ready" || !capabilities.iter().any(|capability| capability == "workspace.catalog.v1") {
        return Ok(poll);
    }
    let catalog = remote.request(machine_id, "workspace.catalog", json!({}))?;
    if catalog.get("complete").and_then(Value::as_bool) == Some(true) {
        poll.catalog = Some(catalog);
    }
    Ok(poll)
}
