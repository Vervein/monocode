//! The workspace owner service: same-user IPC that the Node host gateways
//! remote SSH clients through. Requests reuse the mobile projection and
//! domain handlers so a remote desktop sees exactly what a paired phone sees,
//! without the phone pairing transport.
use super::mobile_host::{
    PageLook, Saved, Target, assemble_catalog, older_page, page_turn, saved_snapshot, string,
};
use super::*;
use crate::mobile::transcript as turns;
use monocode_core::workspace_ipc::{WorkspaceIpc, WorkspaceIpcEvent, WorkspaceIpcRequest};
use monocode_core::workspace_protocol::{CatalogPage, capabilities};
use serde_json::{Value, json};

const READ_TURNS: usize = 30;
const MAX_PAGE: usize = 100;

pub(super) struct Workspace {
    pub(super) ipc: Option<WorkspaceIpc>,
    pub(super) source_id: String,
    pub(super) owner_epoch: String,
    pub(super) catalog_revision: u64,
    pub(super) catalog_hash: String,
}

impl Default for Workspace {
    fn default() -> Self {
        Self {
            ipc: None,
            source_id: String::new(),
            owner_epoch: String::new(),
            catalog_revision: 0,
            catalog_hash: String::new(),
        }
    }
}

impl Shell {
    /// Starts the owner endpoint remote desktops reach through the host. The
    /// source identity is created here on first run: enabling this build is
    /// the explicit act that persists it.
    pub(super) fn init_workspace_host(&mut self, cx: &mut Context<Self>) {
        if cx.is_test() {
            return;
        }
        let Some(data_dir) = locked(&self.store)
            .ok()
            .and_then(|store| store.data_dir().map(|dir| dir.to_path_buf()))
        else {
            return;
        };
        let version = crate::notifications::updater::version();
        let (ipc, events) = match WorkspaceIpc::open(&data_dir, version) {
            Ok(opened) => opened,
            Err(error) => {
                log::warn!("workspace host: {error}");
                return;
            }
        };
        let info = ipc.info();
        self.workspace_host.source_id = info.source_id;
        self.workspace_host.owner_epoch = info.owner_epoch;
        self.workspace_host.ipc = Some(ipc);
        // Close the endpoint with the app so a stale socket never outlives
        // the owner it belongs to.
        cx.on_app_quit(|this, _| {
            this.shutdown_workspace_host();
            async {}
        })
        .detach();
        cx.spawn(async move |this, cx| {
            while let Ok(event) = events.recv().await {
                if this
                    .update(cx, |this, cx| this.handle_workspace_event(event, cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }

    pub(super) fn shutdown_workspace_host(&mut self) {
        self.workspace_host.ipc = None;
    }

    fn handle_workspace_event(&mut self, event: WorkspaceIpcEvent, cx: &mut Context<Self>) {
        let WorkspaceIpcEvent::Request(request) = event;
        match request.method.as_str() {
            "workspace.describe" => request.reply(Ok(self.workspace_describe())),
            "workspace.catalog" => self.workspace_catalog(request, cx),
            "workspace.changes" => self.workspace_changes(request, cx),
            "monos.get" => self.workspace_mono_get(request, cx),
            "monos.profile" => self.workspace_mono_profile(request, cx),
            "monos.history" => self.workspace_mono_history(request, cx),
            "monos.file" => self.workspace_mono_file(request, cx),
            "workspace.commands.dispatch" => self.workspace_dispatch(request, cx),
            "workspace.commands.get" => self.workspace_receipts(request),
            _ => request.reply(Err("Unsupported workspace method".into())),
        }
    }

    fn workspace_capabilities(&self) -> Vec<String> {
        [
            capabilities::CATALOG,
            capabilities::CHANGES,
            capabilities::MONO_READ,
            capabilities::MONO_FILES,
            capabilities::MONO_SEND,
            capabilities::MONO_PROJECT_LINKS,
            capabilities::MONO_APPROVALS,
            capabilities::RECEIPTS,
        ]
        .iter()
        .map(|capability| capability.to_string())
        .collect()
    }

    fn workspace_describe(&self) -> Value {
        json!({
            "workspaceSourceId": self.workspace_host.source_id,
            "ownerMode": "desktop",
            "ownerEpoch": self.workspace_host.owner_epoch,
            "dataSchemaVersion": monocode_core::workspace_protocol::WORKSPACE_SCHEMA_VERSION,
            "readiness": "ready",
            "capabilities": self.workspace_capabilities(),
            "appVersion": crate::notifications::updater::version(),
        })
    }

    /// The full workspace catalog, built on demand through the same
    /// projection the phone poll uses, then partitioned: anything pointing at
    /// another environment leaves the operable lists and is reported as an
    /// external reference instead.
    fn workspace_catalog(&mut self, request: WorkspaceIpcRequest, cx: &mut Context<Self>) {
        let inputs = self.catalog_inputs(cx);
        let store = self.store.clone();
        let work = cx
            .background_executor()
            .spawn(async move { assemble_catalog(&store, inputs).await });
        cx.spawn(async move |this, cx| {
            let outcome = work.await;
            let answer = match this.update(cx, |this, cx| {
                outcome.and_then(|(payload, ids, previews)| {
                    this.mobile.ids = ids;
                    this.mobile.mono_previews = previews;
                    this.workspace_catalog_page(payload, cx)
                })
            }) {
                Ok(answer) => answer,
                Err(_) => Err("Workspace is no longer available".into()),
            };
            request.reply(answer);
        })
        .detach();
    }

    /// Wraps the shared projection in the workspace envelope, moves external
    /// references out of the operable lists, and maintains the revision used
    /// by `workspace.changes`.
    fn workspace_catalog_page(&mut self, mut payload: Value, cx: &mut App) -> Result<Value, String> {
        let _ = cx;
        let mut externals: Vec<Value> = Vec::new();
        let mut operable_projects: Vec<Value> = Vec::new();
        let mut operable_ids: std::collections::HashSet<String> = Default::default();
        if let Some(projects) = payload["projects"].as_array() {
            for project in projects {
                let path = project["path"].as_str().unwrap_or_default();
                if path.starts_with(monocode_core::remote::REMOTE_PATH_PREFIX) {
                    externals.push(json!({
                        "kind": "project",
                        "id": project["id"].clone(),
                        "path": path,
                        "name": project["name"].clone(),
                    }));
                } else {
                    if let Some(id) = project["id"].as_str() {
                        operable_ids.insert(id.to_string());
                    }
                    operable_projects.push(project.clone());
                }
            }
        }
        let mut monos: Vec<Value> = payload["monos"].as_array().cloned().unwrap_or_default();
        for mono in monos.iter_mut() {
            let mono_id = mono["id"].as_str().unwrap_or_default().to_string();
            let linked: Vec<String> = mono["projectIds"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect();
            let (operable, external): (Vec<String>, Vec<String>) =
                linked.into_iter().partition(|id| operable_ids.contains(id));
            mono["projectIds"] = json!(operable);
            for id in external {
                externals.push(json!({
                    "kind": "monoProject",
                    "monoId": mono_id,
                    "projectId": id,
                }));
            }
        }
        payload["projects"] = json!(operable_projects);
        payload["monos"] = json!(monos);
        // Sessions of external projects leave the catalog with their project.
        if let Some(sessions) = payload["sessions"].as_array() {
            let filtered: Vec<Value> = sessions
                .iter()
                .filter(|session| {
                    session["projectId"]
                        .as_str()
                        .is_none_or(|id| operable_ids.contains(id))
                })
                .cloned()
                .collect();
            payload["sessions"] = json!(filtered);
        }

        let hash = monocode_core::workspace_protocol::canonical_hash(&payload);
        if hash != self.workspace_host.catalog_hash {
            self.workspace_host.catalog_hash = hash.clone();
            self.workspace_host.catalog_revision += 1;
        }
        let page = CatalogPage {
            source_id: self.workspace_host.source_id.clone(),
            owner_epoch: self.workspace_host.owner_epoch.clone(),
            catalog_revision: self.workspace_host.catalog_revision,
            snapshot_id: hash,
            complete: true,
            next_page_token: None,
            projects: payload["projects"].as_array().cloned().unwrap_or_default(),
            monos: payload["monos"].as_array().cloned().unwrap_or_default(),
            sessions: payload["sessions"].as_array().cloned().unwrap_or_default(),
            external_references: externals,
        };
        serde_json::to_value(page).map_err(|error| error.to_string())
    }

    /// Coarse invalidation for now: the client refetches the catalog whenever
    /// the revision moved. Rebuilding here keeps the answer honest even when
    /// nobody polled the catalog meanwhile.
    fn workspace_changes(&mut self, request: WorkspaceIpcRequest, cx: &mut Context<Self>) {
        let since_revision = request.params["sinceRevision"].as_u64().unwrap_or(0);
        let since_epoch = request.params["ownerEpoch"].as_str().unwrap_or_default().to_string();
        let inputs = self.catalog_inputs(cx);
        let store = self.store.clone();
        let work = cx
            .background_executor()
            .spawn(async move { assemble_catalog(&store, inputs).await });
        cx.spawn(async move |this, cx| {
            let outcome = work.await;
            let answer = match this.update(cx, |this, cx| {
                outcome.and_then(|(payload, ids, previews)| {
                    this.mobile.ids = ids;
                    this.mobile.mono_previews = previews;
                    let _ = this.workspace_catalog_page(payload, cx);
                    let epoch = this.workspace_host.owner_epoch.clone();
                    let revision = this.workspace_host.catalog_revision;
                    Ok::<_, String>(if since_epoch != epoch || since_revision != revision {
                        json!({"kind": "reset", "catalogRevision": revision, "ownerEpoch": epoch})
                    } else {
                        json!({"kind": "unchanged", "catalogRevision": revision})
                    })
                })
            }) {
                Ok(answer) => answer,
                Err(_) => Err("Workspace is no longer available".into()),
            };
            request.reply(answer);
        })
        .detach();
    }

    fn workspace_mono_get(&mut self, request: WorkspaceIpcRequest, cx: &mut Context<Self>) {
        let id = string(&request.params, "monoId").unwrap_or_default();
        let result = self
            .monos
            .as_ref()
            .and_then(|monos| monos.read(cx).roster.get(&id).cloned())
            .map(|mono| {
                json!({
                    "id": mono.id,
                    "name": mono.label(),
                    "mascot": mono.mascot,
                    "color": format!("#{:06x}", mono.color & 0xff_ffff),
                    "projects": mono.projects,
                    "sessionId": mono.session_id,
                    "heartbeatMinutes": mono.heartbeat_minutes,
                    "subsessions": mono.subsessions,
                })
            })
            .ok_or_else(|| "Mono is no longer available".into());
        request.reply(result);
    }

    fn workspace_mono_profile(&mut self, request: WorkspaceIpcRequest, cx: &mut Context<Self>) {
        let id = string(&request.params, "monoId").unwrap_or_default();
        let Some(monos) = self.monos.clone() else {
            return request.reply(Err("Mono is no longer available".into()));
        };
        let views = self.mono_session_views(cx);
        let result = monos.update(cx, |monos, cx| monos.mobile_profile(&id, &views, cx));
        request.reply(result);
    }

    fn workspace_mono_file(&mut self, request: WorkspaceIpcRequest, cx: &mut Context<Self>) {
        let id = string(&request.params, "monoId").unwrap_or_default();
        let Some(monos) = self.monos.clone() else {
            return request.reply(Err("Mono is no longer available".into()));
        };
        let work = monos.update(cx, |monos, cx| monos.mobile_file(&id, &request.params, cx));
        cx.spawn(async move |_, _| request.reply(work.await))
            .detach();
    }

    /// The write path: validate, deduplicate by persistent receipt, execute
    /// through the same domain calls the local UI uses, then record the
    /// business outcome. A receipt is the owner's memory of acceptance, so a
    /// retried command after a disconnect is answered, never repeated.
    fn workspace_dispatch(&mut self, request: WorkspaceIpcRequest, cx: &mut Context<Self>) {
        let command: monocode_core::workspace_protocol::WorkspaceCommand =
            match serde_json::from_value(request.params["command"].clone()) {
                Ok(command) => command,
                Err(error) => return request.reply(Err(format!("Invalid command envelope: {error}"))),
            };
        let owner_epoch = self.workspace_host.owner_epoch.clone();
        let command_id = command.command_id.clone();
        let receipt_json = |status: &str, result: Option<Value>| {
            json!({
                "commandId": command_id,
                "status": status,
                "result": result,
                "ownerEpoch": owner_epoch,
            })
        };
        if command.command_id.is_empty() {
            return request.reply(Err("Command id is required".into()));
        }
        // Identity mismatches reject without touching state or persistence:
        // a command for another workspace is never ours to remember.
        if command.workspace_source_id != self.workspace_host.source_id {
            return request.reply(Ok(receipt_json(
                "rejected",
                Some(json!({"reason": "Command names a different workspace"})),
            )));
        }
        if let Some(epoch) = &command.expected_owner_epoch
            && epoch != &self.workspace_host.owner_epoch
        {
            return request.reply(Ok(receipt_json(
                "rejected",
                Some(json!({"reason": "Owner restarted since the command was prepared"})),
            )));
        }
        let signature = monocode_core::workspace_protocol::command_signature(&command);
        let existing = locked(&self.store).and_then(|store| store.workspace_receipt(&command.command_id));
        match existing {
            Ok(Some((stored_signature, status, result))) => {
                if stored_signature == signature {
                    // The retry of an accepted command: answer from memory.
                    let result = result.and_then(|text| serde_json::from_str::<Value>(&text).ok());
                    return request.reply(Ok(receipt_json(&status, result)));
                }
                return request.reply(Ok(receipt_json(
                    "rejected",
                    Some(json!({"reason": "Command id was already used for a different command"})),
                )));
            }
            Ok(None) => {}
            Err(error) => return request.reply(Err(format!("Cannot read receipts: {error}"))),
        }
        if let Err(error) = locked(&self.store)
            .and_then(|store| store.save_workspace_receipt(&command.command_id, &signature, "accepted", None))
        {
            return request.reply(Err(format!("Cannot record the command: {error}")));
        }

        let outcome = self.execute_workspace_command(&command, cx);
        if let Ok(result) = &outcome
            && command.action == "mono.send"
        {
            // completed means queued *durably*: the outbox write commits on
            // a background task after the domain call returns, so answering
            // now would let a crash in between lose a message the receipt
            // claims was accepted. Hold the reply until the write lands;
            // a retry in the meantime is answered by the accepted receipt.
            let message_id = result["messageId"].as_str().unwrap_or_default().to_string();
            let store = self.store.clone();
            let signature = signature.clone();
            let command_id = command.command_id.clone();
            let owner_epoch = self.workspace_host.owner_epoch.clone();
            let result = result.clone();
            cx.spawn(async move |this, cx| {
                let mut persisted = false;
                for _ in 0..600 {
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(50))
                        .await;
                    persisted = this
                        .update(cx, |shell, cx| {
                            shell
                                .monos
                                .as_ref()
                                .map(|monos| monos.read(cx).message_persisted(&message_id))
                                .unwrap_or(false)
                        })
                        .unwrap_or(false);
                    if persisted {
                        break;
                    }
                }
                let (status, result) = if persisted {
                    ("completed", Some(result))
                } else {
                    (
                        "unknown",
                        Some(json!({"error": "Message did not reach disk in time; its outcome is unknown"})),
                    )
                };
                let encoded = result.as_ref().map(|value| value.to_string());
                if let Err(error) = locked(&store).and_then(|store| {
                    store.save_workspace_receipt(&command_id, &signature, status, encoded.as_deref())
                }) {
                    return request.reply(Err(format!("Cannot record the outcome: {error}")));
                }
                request.reply(Ok(json!({
                    "commandId": command_id,
                    "status": status,
                    "result": result,
                    "ownerEpoch": owner_epoch,
                })));
            })
            .detach();
            return;
        }
        let (status, result) = match outcome {
            Ok(result) => ("completed", Some(result)),
            Err(error) => ("failed", Some(json!({"error": error}))),
        };
        let encoded = result.as_ref().map(|value| value.to_string());
        if let Err(error) = locked(&self.store).and_then(|store| {
            store.save_workspace_receipt(&command.command_id, &signature, status, encoded.as_deref())
        }) {
            return request.reply(Err(format!("Cannot record the outcome: {error}")));
        }
        request.reply(Ok(receipt_json(status, result)));
    }

    /// Executes one validated command. `completed` means the business effect
    /// the action promises — for mono.send, that the message is queued to the
    /// Mono's own conversation — never that the task has finished.
    fn execute_workspace_command(
        &mut self,
        command: &monocode_core::workspace_protocol::WorkspaceCommand,
        cx: &mut Context<Self>,
    ) -> Result<Value, String> {
        let Some(monos) = self.monos.clone() else {
            return Err("Monos are not available in this window".into());
        };
        match command.action.as_str() {
            "mono.send" => {
                let id = command.target.id.clone();
                let text = command.payload["text"].as_str().unwrap_or_default().to_string();
                if text.is_empty() {
                    return Err("mono.send requires payload.text".into());
                }
                let attachments: Vec<String> = command.payload["attachments"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect();
                let message_id =
                    monos.update(cx, |monos, cx| monos.mobile_send(&id, text, attachments, cx))?;
                let session = monos.read(cx).roster.get(&id).and_then(|mono| mono.session_id);
                Ok(json!({"queued": true, "monoId": id, "sessionId": session, "messageId": message_id}))
            }
            "mono.setProjects" => {
                let id = command.target.id.clone();
                let projects: Vec<String> = command.payload["projects"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect();
                // Project binding reuses the phone's edit path so validation,
                // normalization, chat reconfiguration and persistence stay
                // exactly what a local edit would do.
                let current = monos
                    .read(cx)
                    .roster
                    .get(&id)
                    .map(|mono| mono.projects.clone())
                    .ok_or("Mono is no longer available")?;
                let remove: Vec<&str> = current
                    .iter()
                    .filter(|path| !projects.contains(path))
                    .map(String::as_str)
                    .collect();
                let add: Vec<&str> = projects
                    .iter()
                    .filter(|path| !current.contains(path))
                    .map(String::as_str)
                    .collect();
                // mobile_update takes one addPath at a time and a removePaths
                // list, mirroring the desktop's settings edits.
                monos.update(cx, |monos, cx| {
                    monos.mobile_update(&id, &json!({"removePaths": remove}), cx)?;
                    for path in &add {
                        monos.mobile_update(&id, &json!({"addPath": path}), cx)?;
                    }
                    Ok::<_, String>(())
                })?;
                Ok(json!({"monoId": id, "projects": projects}))
            }
            "mono.cancel" => {
                let id = command.target.id.clone();
                // Cancellation is idempotent: a Mono without a busy turn has
                // nothing to interrupt, which is the same end state as a
                // cancelled one.
                let mut cancelled = false;
                if let Some(chat) = monos.read(cx).open_chat(&id) {
                    chat.update(cx, |chat, cx| {
                        cancelled = chat.responding() || cancelled;
                        chat.stop_if_busy(cx);
                    });
                } else if monos.read(cx).roster.get(&id).is_none() {
                    return Err("Mono is no longer available".into());
                }
                Ok(json!({"monoId": id, "cancelled": cancelled}))
            }
            "mono.answerQuestion" => {
                let id = command.target.id.clone();
                let request_id = command.payload["requestId"]
                    .as_u64()
                    .ok_or("mono.answerQuestion requires payload.requestId")?;
                // An omitted reply holds a timed question open rather than
                // answering it, mirroring the phone's keepQuestionOpen.
                let reply = command.payload.get("reply").cloned();
                let chat = monos
                    .read(cx)
                    .open_chat(&id)
                    .ok_or("This question was already answered")?;
                chat.update(cx, |chat, cx| {
                    chat.mobile_answer_question(request_id, reply.as_ref(), cx)
                })?;
                Ok(json!({"monoId": id, "answered": reply.is_some()}))
            }
            action => Err(format!("Unsupported workspace action: {action}")),
        }
    }

    /// Receipt lookup is the answer to "did my command land after I lost the
    /// response": unknown is an honest answer for a command this owner never
    /// accepted, distinct from every executed outcome.
    fn workspace_receipts(&mut self, request: WorkspaceIpcRequest) {
        let mut ids: Vec<String> = Vec::new();
        if let Some(id) = request.params["commandId"].as_str() {
            ids.push(id.to_string());
        }
        if let Some(list) = request.params["commandIds"].as_array() {
            ids.extend(list.iter().filter_map(Value::as_str).map(str::to_owned));
        }
        if ids.is_empty() {
            return request.reply(Err("commandId or commandIds is required".into()));
        }
        let answer = locked(&self.store).and_then(|store| {
            let mut receipts = Vec::new();
            for id in &ids {
                match store.workspace_receipt(id)? {
                    Some((_, status, result)) => {
                        let result = result.and_then(|text| serde_json::from_str::<Value>(&text).ok());
                        receipts.push(json!({
                            "commandId": id,
                            "status": status,
                            "result": result,
                            "ownerEpoch": self.workspace_host.owner_epoch,
                        }));
                    }
                    None => receipts.push(json!({
                        "commandId": id,
                        "status": "unknown",
                        "result": null,
                    })),
                }
            }
            Ok(json!({"receipts": receipts}))
        });
        request.reply(answer);
    }

    /// A Mono's conversation from the original store only: browsing never
    /// opens its chat or starts anything, per the workspace contract.
    fn workspace_mono_history(&mut self, request: WorkspaceIpcRequest, cx: &mut Context<Self>) {
        let id = string(&request.params, "monoId").unwrap_or_default();
        let target = Target::Mono(id);
        let before = string(&request.params, "before").unwrap_or_default();
        let limit = (request.params["limit"].as_u64().unwrap_or(READ_TURNS as u64) as usize)
            .clamp(1, MAX_PAGE);
        if !before.is_empty()
            && let Some((cursor, at)) = page_turn(&before)
        {
            self.workspace_older_page(request, &target, cursor, Some(at), limit, cx);
            return;
        }
        let load = self.load_saved(&target, cx);
        cx.spawn(async move |_, _| {
            let answer = match load.await {
                Ok(saved) => workspace_history_page(&saved, limit),
                Err(error) => Err(error),
            };
            request.reply(answer);
        })
        .detach();
    }

    fn workspace_older_page(
        &mut self,
        request: WorkspaceIpcRequest,
        target: &Target,
        cursor: i64,
        before: Option<usize>,
        limit: usize,
        cx: &mut Context<Self>,
    ) {
        let (session, name) = match target {
            Target::Session(id) => (Some(*id), None),
            Target::Mono(mono) => {
                let found = self.monos.as_ref().and_then(|m| {
                    let mono = m.read(cx).roster.get(mono)?;
                    Some((mono.session_id, mono.label()))
                });
                (found.as_ref().and_then(|found| found.0), found.map(|found| found.1))
            }
        };
        let Some(session) = session else {
            return request.reply(Ok(json!({"turns": [], "older": false})));
        };
        let summary = self.session_summaries.get(&session).cloned();
        let look = PageLook {
            mono: name,
            launches: self.mobile_launches(target, cx),
            cwd: summary.as_ref().map(|s| s.cwd.clone()).unwrap_or_default(),
            harness: summary.as_ref().map(|s| s.provider.clone()).unwrap_or_default(),
            model: None,
        };
        let store = self.store.clone();
        let work = cx.background_executor().spawn(async move {
            let store = locked(&store)?;
            older_page(&store, session, cursor, before, limit, look)
        });
        cx.spawn(async move |_, _| request.reply(work.await))
            .detach();
    }
}

/// The first page of a Mono's stored conversation, projected exactly as the
/// mobile read projects it; empty for a Mono that has not talked yet.
fn workspace_history_page(saved: &Saved, limit: usize) -> Result<Value, String> {
    if saved.rows.is_empty() && saved.id == 0 {
        return Ok(json!({
            "messages": [],
            "turns": [],
            "busy": false,
            "older": false,
        }));
    }
    let snapshot = saved_snapshot(saved);
    let source = snapshot.source();
    let ids = turns::turn_ids(&source);
    let start = ids.len().saturating_sub(limit);
    Ok(json!({
        "messages": turns::messages(snapshot.rows),
        "turns": turns::build(&source, start..ids.len()),
        "busy": false,
        "older": start > 0 || snapshot.history.is_some(),
    }))
}
