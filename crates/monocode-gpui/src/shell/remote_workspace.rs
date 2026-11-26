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
    /// The environment the host reported for this machine. Project keys pin
    /// this identity, rather than the SSH endpoint the user happened to use.
    pub(super) environment_id: String,
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
                replica.environment_id = poll.environment_id;
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
        self.sidebar_list.invalidate_structure();
        self.invalidate_chrome(cx);
        cx.notify();
    }

    /// Selects the workspace scope used by the original navigation. `None`
    /// is this machine; a machine id scopes the rails to that workspace's
    /// catalog without making its objects local scheduler state.
    pub(super) fn select_workspace_scope(
        &mut self,
        machine_id: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if self.remote_workspaces.selected == machine_id {
            return;
        }
        self.remote_workspaces.selected = machine_id.clone();
        if let Some(machine_id) = machine_id {
            self.sync_remote_workspace(&machine_id, cx);
        }
        self.sidebar_list.invalidate_structure();
        self.invalidate_chrome(cx);
        cx.notify();
    }

    pub(super) fn selected_workspace_replica(&self) -> Option<(&str, &WorkspaceReplica)> {
        let machine_id = self.remote_workspaces.selected.as_deref()?;
        Some((machine_id, self.remote_workspaces.replicas.get(machine_id)?))
    }

    /// The projects a machine's workspace reports, as the catalog projects
    /// them. Empty both for an empty workspace and for one that has not
    /// synced yet; the readiness and detail say which.
    pub(super) fn remote_workspace_catalog(&self, machine_id: &str) -> Option<&Value> {
        self.remote_workspaces.replicas.get(machine_id)?.catalog.as_ref()
    }

    /// Opens a catalog Mono through the remote workspace surface. The detail
    /// remains a remote view model: it is backed by workspace commands and
    /// never enters the local Mono manager or scheduler.
    pub(super) fn open_remote_workspace_mono(
        &mut self,
        machine_id: String,
        entry: Value,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_machines(window, cx);
        if let Some(view) = self.machines.clone() {
            view.update(cx, |view, cx| {
                view.open_machine_mono_entry(machine_id, entry, cx)
            });
        }
    }

    /// Opens a catalog project through the existing remote-project runtime.
    /// Registration happens only after the user picks the project; the rail's
    /// read-only catalog sync itself never writes host project state.
    pub(super) fn open_remote_workspace_project(
        &mut self,
        machine_id: String,
        entry: Value,
        cx: &mut Context<Self>,
    ) {
        let Some(path) = entry["path"].as_str().map(str::to_owned) else {
            return;
        };
        let environment_id = self
            .remote_workspaces
            .replicas
            .get(&machine_id)
            .map(|replica| replica.environment_id.clone())
            .filter(|id| !id.is_empty())
            .or_else(|| {
                self.remote
                    .list()
                    .ok()
                    .and_then(|machines| {
                        machines
                            .into_iter()
                            .find(|machine| machine.id == machine_id)
                            .map(|machine| machine.environment_id)
                    })
            });
        let Some(environment_id) = environment_id else {
            self.error = Some("The machine's environment identity is not known yet".into());
            cx.notify();
            return;
        };
        let remote = self.remote.clone();
        let store = self.store.clone();
        let machine = machine_id.clone();
        let work = cx.background_executor().spawn(async move {
            let project = remote.request(&machine, "projects.open", json!({"cwd": path}))?;
            let remembered = locked(&store)?.remember_remote_project(&environment_id, &project)?;
            Ok::<_, String>(remembered.key)
        });
        cx.spawn(async move |this, cx| {
            let result = work.await;
            let _ = this.update_in(cx, |this, window, cx| match result {
                Ok(key) => this.select_project(key, window, cx),
                Err(error) => {
                    this.error = Some(error);
                    cx.notify();
                }
            });
        })
        .detach();
    }
}

struct Poll {
    readiness: String,
    source_id: Option<String>,
    owner_epoch: String,
    capabilities: Vec<String>,
    environment_id: String,
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
        environment_id: describe["environmentId"].as_str().unwrap_or_default().to_string(),
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
