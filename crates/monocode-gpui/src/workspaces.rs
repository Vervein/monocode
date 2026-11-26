//! A connected machine's workspace, as the phone models it: the Monos the
//! owner's catalog projects, opened into a transcript and composer that go
//! through workspace commands. Nothing here schedules or stores anything
//! locally — the replica is a read cache, the owner's receipt is the truth.
use crate::{
    input::{self, Input},
    theme::*,
};
use gpui::{
    AnyElement, App, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, Role,
    SharedString, Subscription, Window, div, hsla, prelude::*, px,
};
use monocode_core::remote::{Connections, Machine};
use serde_json::{Value, json};
use std::{collections::HashMap, sync::Arc, time::Duration};

/// Catalog and history refresh share one interval: the owner answers both
/// from its own projection, so polling is the protocol's current push.
const POLL: Duration = Duration::from_secs(3);

pub(crate) enum Event {
    // The shared app-view contract includes Close even though the Machines
    // surface currently closes through its tab rather than an inner control.
    #[allow(dead_code)]
    Close,
}

struct Replica {
    readiness: String,
    detail: String,
    offline: bool,
    catalog: Option<Value>,
    /// The machine's environment id, pinned at describe time: a reinstalled
    /// host answering for the same endpoint must never present its workspace
    /// as the one this machine was bound to.
    environment_id: String,
    syncing: bool,
}

struct MonoDetail {
    instance: u64,
    id: String,
    machine: String,
    name: String,
    color: String,
    profile: Option<Value>,
    /// Loaded conversation turns in chronological order. The tail is refreshed
    /// in place; pages loaded manually stay prefixed to it.
    turns: Vec<Value>,
    /// Whether the owner says there are turns before the oldest loaded one.
    older: bool,
    /// A manual older-page request is in flight.
    loading_older: bool,
    /// Once the user pages, tail refreshes must not reset the boundary answer:
    /// the newest page always has older rows relative to itself, even after the
    /// user already paged back to the beginning.
    paged: bool,
    /// The owner conversation these cached turns belong to. Resetting a Mono
    /// keeps its Mono id but starts a new conversation, so row-local turn ids
    /// are only meaningful under this identity.
    conversation: Option<i64>,
    busy: bool,
    can_cancel: bool,
    input: Entity<Input>,
    sending: bool,
    /// An approval, question or cancel command is in flight.
    acting: bool,
    /// The live question these form values belong to; a new request resets them.
    question_request: Option<u64>,
    question_answers: HashMap<String, Vec<String>>,
    question_custom: HashMap<String, Entity<Input>>,
    /// The command the current draft was last dispatched as. Retrying the
    /// same text reuses the id, so a send whose receipt was lost in transit
    /// is answered from the owner's memory instead of queued twice.
    pending_command: Option<(String, String)>,
    notice: String,
    /// Incremented on every send so a late history answer never overwrites
    /// a newer read.
    revision: u64,
}

pub(crate) struct Workspaces {
    remote: Arc<Connections>,
    focus: FocusHandle,
    machines: Vec<Machine>,
    selected: Option<String>,
    replicas: HashMap<String, Replica>,
    detail: Option<MonoDetail>,
    next_detail_instance: u64,
    /// Inactive remote details keyed by machine and owner Mono identity. Tab
    /// activation swaps these in and out; only an explicit close drops one.
    details: HashMap<(String, String), MonoDetail>,
    _events: Vec<Subscription>,
}

impl EventEmitter<Event> for Workspaces {}
impl Focusable for Workspaces {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Workspaces {
    pub(crate) fn new(remote: Arc<Connections>, cx: &mut Context<Self>) -> Self {
        let machines = remote.list().unwrap_or_default();
        let mut this = Self {
            remote,
            focus: cx.focus_handle(),
            machines,
            selected: None,
            replicas: HashMap::new(),
            detail: None,
            next_detail_instance: 0,
            details: HashMap::new(),
            _events: Vec::new(),
        };
        // Single-machine users should never have to make the only possible
        // choice; multi-machine users pick explicitly.
        if this.machines.len() == 1 {
            let id = this.machines[0].id.clone();
            this.select(id, cx);
        }
        this
    }

    /// Re-reads the connection list: machines added or removed in Settings
    /// after this view first opened must show up without an app restart.
    pub(crate) fn reload_machines(&mut self, cx: &mut Context<Self>) {
        self.machines = self.remote.list().unwrap_or_default();
        self.replicas
            .retain(|id, _| self.machines.iter().any(|machine| &machine.id == id));
        self.details.retain(|(machine_id, _), _| {
            self.machines.iter().any(|machine| &machine.id == machine_id)
        });
        if let Some(selected) = self.selected.clone()
            && !self.machines.iter().any(|machine| machine.id == selected)
        {
            self.selected = None;
            self.detail = None;
        }
        if self.selected.is_none() && self.machines.len() == 1 {
            let id = self.machines[0].id.clone();
            self.select(id, cx);
        }
        cx.notify();
    }

    fn detail_mut(&mut self, machine: &str, id: &str, instance: u64) -> Option<&mut MonoDetail> {
        let active = self.detail.as_mut().filter(|detail| {
            detail.machine == machine && detail.id == id && detail.instance == instance
        });
        if active.is_some() {
            return active;
        }
        self.details
            .get_mut(&(machine.to_string(), id.to_string()))
            .filter(|detail| detail.instance == instance)
    }

    fn stash_detail(&mut self) {
        if let (Some(machine_id), Some(detail)) = (self.selected.clone(), self.detail.take()) {
            self.details.insert((machine_id, detail.id.clone()), detail);
        }
    }

    fn select(&mut self, machine_id: String, cx: &mut Context<Self>) {
        if self.selected.as_deref() != Some(machine_id.as_str()) {
            self.stash_detail();
        }
        self.selected = Some(machine_id.clone());
        self.start_sync(machine_id, cx);
        cx.notify();
    }

    /// The currently open remote detail as its stable tab identity. `None`
    /// means the Machines view is showing the workspace catalog instead.
    pub(crate) fn open_detail_key(&self) -> Option<(String, String)> {
        Some((self.selected.clone()?, self.detail.as_ref()?.id.clone()))
    }

    /// Returns the Machines surface to its catalog without touching the remote
    /// workspace. Closing a detail is view state only, never a cancel command.
    pub(crate) fn close_detail(&mut self, cx: &mut Context<Self>) {
        if let (Some(machine_id), Some(detail)) = (self.selected.clone(), self.detail.take()) {
            self.details.remove(&(machine_id, detail.id));
            cx.notify();
        }
    }

    /// Drops one detail whether it is currently active or parked behind the
    /// catalog/another tab. Explicit tab close is different from activation.
    pub(crate) fn close_detail_key(&mut self, machine_id: &str, id: &str, cx: &mut Context<Self>) {
        self.details.remove(&(machine_id.to_string(), id.to_string()));
        if self.open_detail_key() == Some((machine_id.to_string(), id.to_string())) {
            self.detail = None;
        }
        cx.notify();
    }

    /// Shows the catalog without discarding inactive detail state. This is
    /// tab activation, not closing the remote object.
    pub(crate) fn show_catalog(&mut self, cx: &mut Context<Self>) {
        self.stash_detail();
        cx.notify();
    }

    /// Opens a catalog entry chosen from the shell's host-scoped navigation.
    /// The entry already came from the owner's catalog, so the detail can
    /// render immediately while this view's own replica catches up.
    pub(crate) fn open_machine_mono_entry(
        &mut self,
        machine_id: String,
        entry: Value,
        cx: &mut Context<Self>,
    ) {
        if self.selected.as_deref() != Some(machine_id.as_str()) {
            self.select(machine_id, cx);
        }
        self.open_mono(&entry, cx);
    }

    /// The per-machine sync loop: describe, refill the catalog when the
    /// owner is ready, repeat. Blocking host RPCs live on the background
    /// executor; only their results touch the view.
    fn start_sync(&mut self, machine_id: String, cx: &mut Context<Self>) {
        let replica = self
            .replicas
            .entry(machine_id.clone())
            .or_insert_with(|| Replica {
                readiness: "starting".into(),
                detail: String::new(),
                offline: false,
                catalog: None,
                environment_id: String::new(),
                syncing: false,
            });
        if replica.syncing {
            return;
        }
        replica.syncing = true;
        let remote = self.remote.clone();
        cx.spawn(async move |this, cx| {
            loop {
                let machine = machine_id.clone();
                let remote = remote.clone();
                let outcome = cx
                    .background_executor()
                    .spawn(async move { poll_workspace(&remote, &machine) })
                    .await;
                let keep = this
                    .update(cx, |this, cx| {
                        if let Some(replica) = this.replicas.get_mut(&machine_id) {
                            match outcome {
                                Ok((readiness, detail, catalog, environment_id)) => {
                                    replica.offline = false;
                                    replica.readiness = readiness;
                                    replica.detail = detail;
                                    replica.environment_id = environment_id;
                                    if let Some(catalog) = catalog {
                                        replica.catalog = Some(catalog);
                                    }
                                }
                                Err(error) => {
                                    replica.offline = true;
                                    replica.detail = error;
                                }
                            }
                            cx.notify();
                        }
                    })
                    .is_ok();
                if !keep {
                    break;
                }
                cx.background_executor().timer(POLL).await;
            }
        })
        .detach();
    }

    fn open_mono(&mut self, entry: &Value, cx: &mut Context<Self>) {
        let Some(_machine_id) = self.selected.clone() else {
            return;
        };
        let id = entry["id"].as_str().unwrap_or_default().to_string();
        if id.is_empty() {
            return;
        }
        self.stash_detail();
        let machine_id = self.selected.clone().unwrap();
        if let Some(saved) = self.details.remove(&(machine_id.clone(), id.clone())) {
            self.detail = Some(saved);
        } else {
            let input = cx.new(|cx| {
                input::Input::field(
                    "Message the Mono",
                    "Ask it to do something…",
                    content(0.35),
                    cx,
                )
            });
            self._events.push(
                cx.subscribe(&input, |this, _, _: &input::Submitted, cx| this.send(cx)),
            );
            self.next_detail_instance += 1;
            self.detail = Some(MonoDetail {
                instance: self.next_detail_instance,
                name: entry["name"].as_str().unwrap_or("Mono").to_string(),
                color: entry["color"].as_str().unwrap_or("#888888").to_string(),
                id: id.clone(),
                machine: machine_id.clone(),
                profile: None,
                turns: Vec::new(),
                older: false,
                loading_older: false,
                paged: false,
                conversation: None,
                busy: entry["busy"].as_bool().unwrap_or(false),
                can_cancel: false,
                input,
                sending: false,
                acting: false,
                question_request: None,
                question_answers: HashMap::new(),
                question_custom: HashMap::new(),
                pending_command: None,
                notice: String::new(),
                revision: 0,
            });
        }
        self.refresh_detail(cx);
        // While the detail is open, refresh its transcript on the same
        // cadence as the catalog; closing it stops the loop.
        let remote = self.remote.clone();
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(POLL).await;
                let alive = this
                    .update(cx, |this, cx| {
                        if this.detail.as_ref().is_some_and(|detail| {
                            detail.id == id && detail.machine == machine_id
                        }) {
                            this.refresh_detail(cx);
                            true
                        } else {
                            false
                        }
                    })
                    .unwrap_or(false);
                if !alive {
                    break;
                }
            }
            drop(remote);
        })
        .detach();
        cx.notify();
    }

    /// Refills the open detail from the owner: profile for the header state,
    /// history for the transcript. Both are read-only projections; nothing
    /// here can create or schedule anything on the owner.
    fn refresh_detail(&mut self, cx: &mut Context<Self>) {
        let (Some(machine_id), Some(detail)) = (self.selected.clone(), self.detail.as_ref()) else {
            return;
        };
        let mono_id = detail.id.clone();
        let request_mono_id = mono_id.clone();
        let revision = detail.revision;
        let remote = self.remote.clone();
        let work = cx.background_executor().spawn(async move {
            let history = remote
                .request(&machine_id, "monos.history", json!({"monoId": mono_id}))
                .map_err(|error| error.to_string());
            let profile = remote
                .request(&machine_id, "monos.profile", json!({"monoId": mono_id}))
                .ok();
            (history, profile)
        });
        cx.spawn(async move |this, cx| {
            let (history, profile) = work.await;
            let _ = this.update(cx, |this, cx| {
                let Some(detail) = this.detail.as_mut() else {
                    return;
                };
                if detail.id != request_mono_id || detail.revision != revision {
                    return;
                }
                match history {
                    Ok(page) => {
                        let conversation = page["conversationId"].as_i64();
                        if conversation != detail.conversation {
                            if detail.conversation.is_some() {
                                detail.turns.clear();
                                detail.paged = false;
                            }
                            detail.conversation = conversation;
                        }
                        let incoming = page["turns"].as_array().cloned().unwrap_or_default();
                        merge_latest_turns(&mut detail.turns, incoming);
                        if !detail.paged {
                            detail.older = page["older"].as_bool().unwrap_or(false);
                        }
                        detail.notice = String::new();
                    }
                    Err(error) => detail.notice = error,
                }
                let question_request = profile
                    .as_ref()
                    .and_then(|profile| profile["interaction"]["question"]["requestId"].as_u64());
                if detail.question_request != question_request {
                    detail.question_request = question_request;
                    detail.question_answers.clear();
                    detail.question_custom.clear();
                }
                if let Some(profile) = profile.as_ref() {
                    detail.busy = profile["busy"].as_bool().unwrap_or(false);
                    detail.can_cancel = profile["canCancel"].as_bool().unwrap_or(false);
                }
                detail.profile = profile;
                cx.notify();
            });
        })
        .detach();
    }

    /// Loads the page immediately before the oldest loaded turn. The owner may
    /// answer from the rows currently in memory or cross into the store's older
    /// pages; either way the response is inserted before `before`, not used to
    /// replace the live tail.
    fn load_older(&mut self, cx: &mut Context<Self>) {
        let (Some(machine_id), Some(detail)) = (self.selected.clone(), self.detail.as_mut()) else {
            return;
        };
        if detail.loading_older || !detail.older {
            return;
        }
        let Some(before) = detail
            .turns
            .first()
            .and_then(|turn| turn["id"].as_str())
            .map(str::to_owned)
        else {
            detail.older = false;
            return;
        };
        detail.loading_older = true;
        detail.notice = String::new();
        let mono_id = detail.id.clone();
        let request_mono_id = mono_id.clone();
        let request_machine_id = machine_id.clone();
        let request_instance = detail.instance;
        let request_before = before.clone();
        let remote = self.remote.clone();
        let work = cx.background_executor().spawn(async move {
            remote
                .request(
                    &machine_id,
                    "monos.history",
                    json!({"monoId": mono_id, "before": before, "limit": 30}),
                )
                .map_err(|error| error.to_string())
        });
        cx.spawn(async move |this, cx| {
            let outcome = work.await;
            let _ = this.update(cx, |this, cx| {
                let Some(detail) = this.detail_mut(&request_machine_id, &request_mono_id, request_instance) else {
                    return;
                };
                detail.loading_older = false;
                match outcome {
                    Ok(page) => {
                        if page["conversationId"].as_i64() != detail.conversation {
                            detail.notice = "The conversation changed while older messages were loading".into();
                        } else {
                            let incoming = page["turns"].as_array().cloned().unwrap_or_default();
                            insert_older_turns(&mut detail.turns, &request_before, incoming);
                            detail.older = page["older"].as_bool().unwrap_or(false);
                            detail.paged = true;
                            detail.notice = String::new();
                        }
                    }
                    Err(error) => detail.notice = error,
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// The composer: a workspace command, never a local object. The receipt
    /// answer is the whole truth about acceptance — queued means the owner's
    /// Mono has the message, not that the task finished.
    fn send(&mut self, cx: &mut Context<Self>) {
        let (Some(machine_id), Some(_)) = (self.selected.clone(), self.detail.as_ref()) else {
            return;
        };
        let text = {
            let detail = self.detail.as_ref().unwrap();
            // A dispatch is already in flight: pressing Enter again must not
            // queue the same message under a second command.
            if detail.sending {
                return;
            }
            detail.input.read(cx).content.trim().to_string()
        };
        if text.is_empty() {
            return;
        }
        let (mono_id, environment_id) = {
            let detail = self.detail.as_mut().unwrap();
            detail.sending = true;
            detail.notice = String::new();
            (
                detail.id.clone(),
                self.replicas
                    .get(&machine_id)
                    .map(|replica| replica.environment_id.clone())
                    .unwrap_or_default(),
            )
        };
        let source_id = self
            .replicas
            .get(&machine_id)
            .and_then(|replica| replica.catalog.as_ref())
            .and_then(|catalog| catalog["sourceId"].as_str().map(str::to_owned))
            .unwrap_or_default();
        // The command id is the idempotency key for this draft: retrying
        // unchanged text after a lost receipt reuses it and the owner answers
        // from memory; edited or cleared text is a new command.
        let command_id = {
            let detail = self.detail.as_mut().unwrap();
            match &detail.pending_command {
                Some((id, draft)) if *draft == text => id.clone(),
                _ => {
                    let id = monocode_core::mono::new_id();
                    detail.pending_command = Some((id.clone(), text.clone()));
                    id
                }
            }
        };
        let command = json!({
            "commandId": command_id,
            "environmentId": environment_id,
            "workspaceSourceId": source_id,
            "target": {"kind": "mono", "id": mono_id},
            "action": "mono.send",
            "payload": {"text": text},
        });
        let request_mono_id = mono_id.clone();
        let request_machine_id = machine_id.clone();
        let request_instance = self.detail.as_ref().map(|detail| detail.instance).unwrap_or(0);
        let remote = self.remote.clone();
        let machine = machine_id.clone();
        let work = cx.background_executor().spawn(async move {
            remote.request(
                &machine,
                "workspace.commands.dispatch",
                json!({"command": command}),
            )
        });
        cx.spawn(async move |this, cx| {
            let outcome = work.await;
            let _ = this.update(cx, |this, cx| {
                let Some(detail) = this.detail_mut(&request_machine_id, &request_mono_id, request_instance) else {
                    return;
                };
                detail.sending = false;
                match outcome {
                    Ok(receipt) => {
                        let status = receipt["status"].as_str().unwrap_or("unknown");
                        if status == "completed" {
                            detail
                                .input
                                .update(cx, |input, cx| input.set_text(String::new(), cx));
                            detail.pending_command = None;
                            detail.revision += 1;
                            detail.notice = String::new();
                            this.refresh_detail(cx);
                        } else {
                            // failed/rejected/unknown are recorded outcomes:
                            // resubmitting is a new command, not a retry.
                            detail.pending_command = None;
                            let reason = receipt["result"]["error"]
                                .as_str()
                                .or_else(|| receipt["result"]["reason"].as_str())
                                .unwrap_or(status);
                            detail.notice = format!("Command {status}: {reason}");
                        }
                    }
                    // Transport failure: nothing was recorded either way, so
                    // the draft keeps its command id and Enter retries it.
                    Err(error) => detail.notice = error,
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Dispatches one interaction command through the workspace receipt path.
    /// Approval and question commands derive their idempotency key from the
    /// exact answer; cancel gets a fresh command because each click is a new
    /// interruption request.
    fn dispatch_action(&mut self, action: &str, payload: Value, cx: &mut Context<Self>) {
        let (Some(machine_id), Some(detail)) = (self.selected.clone(), self.detail.as_mut()) else {
            return;
        };
        if detail.acting {
            return;
        }
        detail.acting = true;
        detail.notice = String::new();
        let mono_id = detail.id.clone();
        let replica = self.replicas.get(&machine_id);
        let environment_id = replica
            .map(|replica| replica.environment_id.clone())
            .unwrap_or_default();
        let source_id = replica
            .and_then(|replica| replica.catalog.as_ref())
            .and_then(|catalog| catalog["sourceId"].as_str().map(str::to_owned))
            .unwrap_or_default();
        let business = json!({
            "action": action,
            "monoId": mono_id,
            "payload": payload,
        });
        let command_id = if action == "mono.cancel" {
            monocode_core::mono::new_id()
        } else {
            format!(
                "workspace-{}",
                monocode_core::workspace_protocol::canonical_hash(&business)
            )
        };
        let command = json!({
            "commandId": command_id,
            "environmentId": environment_id,
            "workspaceSourceId": source_id,
            "target": {"kind": "mono", "id": mono_id},
            "action": action,
            "payload": payload,
        });
        let request_mono_id = mono_id.clone();
        let request_machine_id = machine_id.clone();
        let request_instance = detail.instance;
        let remote = self.remote.clone();
        let machine = machine_id.clone();
        let work = cx.background_executor().spawn(async move {
            remote.request(
                &machine,
                "workspace.commands.dispatch",
                json!({"command": command}),
            )
        });
        cx.spawn(async move |this, cx| {
            let outcome = work.await;
            let _ = this.update(cx, |this, cx| {
                let Some(detail) = this.detail_mut(&request_machine_id, &request_mono_id, request_instance) else {
                    return;
                };
                detail.acting = false;
                match outcome {
                    Ok(receipt) => {
                        let status = receipt["status"].as_str().unwrap_or("unknown");
                        if status == "completed" {
                            detail.notice = String::new();
                            detail.revision += 1;
                            this.refresh_detail(cx);
                        } else {
                            let reason = receipt["result"]["error"]
                                .as_str()
                                .or_else(|| receipt["result"]["reason"].as_str())
                                .unwrap_or(status);
                            detail.notice = format!("Command {status}: {reason}");
                        }
                    }
                    Err(error) => detail.notice = error,
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn cancel_mono(&mut self, cx: &mut Context<Self>) {
        self.dispatch_action("mono.cancel", json!({}), cx);
    }

    fn decide_approval(&mut self, request_id: String, decision: &str, cx: &mut Context<Self>) {
        self.dispatch_action(
            "mono.decideApproval",
            json!({"requestId": request_id, "decision": decision}),
            cx,
        );
    }

    fn answer_question(&mut self, skip: bool, keep_open: bool, cx: &mut Context<Self>) {
        let Some(detail) = self.detail.as_ref() else {
            return;
        };
        let Some(request_id) = detail.question_request else {
            return;
        };
        let reply = if keep_open {
            Value::Null
        } else if skip {
            json!({"kind": "skipped"})
        } else {
            let answers = detail
                .question_answers
                .iter()
                .map(|(id, chosen)| (id.clone(), json!(chosen)))
                .collect::<serde_json::Map<_, _>>();
            let custom = detail
                .question_custom
                .iter()
                .filter_map(|(id, input)| {
                    let text = input.read(cx).content.trim().to_string();
                    (!text.is_empty()).then_some((id.clone(), json!(text)))
                })
                .collect::<serde_json::Map<_, _>>();
            json!({"kind": "answered", "answers": answers, "custom": custom})
        };
        let mut payload = json!({"requestId": request_id});
        if !keep_open {
            payload["reply"] = reply;
        }
        self.dispatch_action("mono.answerQuestion", payload, cx);
    }

    fn machine_row(&self, machine: &Machine, cx: &mut Context<Self>) -> impl IntoElement {
        let active = self.selected.as_deref() == Some(&machine.id);
        let id = machine.id.clone();
        div()
            .id(SharedString::from(format!("machine-{}", machine.id)))
            .role(Role::Button)
            .aria_label(machine.name.clone())
            .aria_selected(active)
            .cursor_pointer()
            .h(px(28.))
            .px_2()
            .rounded_md()
            .flex()
            .items_center()
            .gap_2()
            .text_size(px(13.))
            .when(active, |d| {
                d.bg(content(0.1)).font_weight(FontWeight::SEMIBOLD)
            })
            .when(!active, |d| {
                d.text_color(content(0.65)).hover(|s| s.bg(content(0.06)))
            })
            .child(machine.name.clone())
            .on_click(cx.listener(move |this, _, _, cx| this.select(id.clone(), cx)))
    }

    fn mono_card(&self, entry: &Value, cx: &mut Context<Self>) -> impl IntoElement {
        let name = entry["name"].as_str().unwrap_or("Mono").to_string();
        let status = entry["status"].as_str().unwrap_or("idle").to_string();
        let preview = entry["lastMessage"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let color = entry["color"].as_str().unwrap_or("#888888").to_string();
        let entry = entry.clone();
        div()
            .id(SharedString::from(format!(
                "ws-mono-{}",
                entry["id"].as_str().unwrap_or_default()
            )))
            .debug_selector(|| format!("ws-mono-{}", entry["id"].as_str().unwrap_or_default()))
            .role(Role::Button)
            .aria_label(name.clone())
            .cursor_pointer()
            .p_3()
            .rounded_lg()
            .bg(content(0.04))
            .hover(|s| s.bg(content(0.08)))
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().size_2().rounded_full().bg(hsla_parse(&color)))
                    .child(div().font_weight(FontWeight::SEMIBOLD).child(name))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(11.))
                            .text_color(content(0.45))
                            .child(status),
                    ),
            )
            .when(!preview.is_empty(), |d| {
                d.child(
                    div()
                        .text_size(px(12.))
                        .text_color(content(0.55))
                        .line_clamp(2)
                        .child(preview),
                )
            })
            .on_click(cx.listener(move |this, _, _, cx| this.open_mono(&entry, cx)))
    }

    fn project_row(&self, entry: &Value) -> impl IntoElement {
        let path = entry["path"].as_str().unwrap_or_default().to_string();
        let name = entry["name"]
            .as_str()
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| path.rsplit('/').next().unwrap_or(&path).to_string());
        let branch = entry["branch"].as_str().unwrap_or_default().to_string();
        div()
            .px_3()
            .py_2()
            .rounded_md()
            .bg(content(0.03))
            .flex()
            .items_center()
            .gap_2()
            .child(icon("folder", 13.))
            .child(div().text_size(px(13.)).child(name))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(px(11.))
                    .text_color(content(0.4))
                    .child(path),
            )
            .when(!branch.is_empty(), |d| {
                d.child(
                    div()
                        .text_size(px(11.))
                        .text_color(content(0.45))
                        .child(branch),
                )
            })
    }

    fn ensure_question_inputs(&mut self, cx: &mut Context<Self>) {
        let needed: Vec<String> = self
            .detail
            .as_ref()
            .and_then(|detail| detail.profile.as_ref())
            .and_then(|profile| profile["interaction"]["question"]["questions"].as_array())
            .into_iter()
            .flatten()
            .filter(|question| question["allowCustom"].as_bool().unwrap_or(false))
            .filter_map(|question| question["id"].as_str().map(str::to_owned))
            .collect();
        let Some(detail) = self.detail.as_mut() else {
            return;
        };
        for id in needed {
            if let std::collections::hash_map::Entry::Vacant(entry) =
                detail.question_custom.entry(id)
            {
                let input = cx.new(|cx| {
                    input::Input::field("Custom answer", "Type an answer…", content(0.35), cx)
                });
                entry.insert(input);
            }
        }
    }

    /// Live approval and question cards projected by the owner. Every action
    /// here dispatches a workspace command; the cards are never local state
    /// pretending to resolve the remote provider.
    fn interaction_view(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let Some(detail) = self.detail.as_ref() else {
            return Vec::new();
        };
        let interaction = detail
            .profile
            .as_ref()
            .map(|profile| profile["interaction"].clone())
            .unwrap_or_else(|| json!({"approvals": [], "question": null}));
        let mut out: Vec<AnyElement> = Vec::new();
        for approval in interaction["approvals"].as_array().into_iter().flatten() {
            let request_id = approval["requestId"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            if request_id.is_empty() {
                continue;
            }
            let title = approval["title"]
                .as_str()
                .unwrap_or("Approval required")
                .to_string();
            let detail_text = approval["detail"].as_str().unwrap_or_default().to_string();
            let can_remember = approval["canRemember"].as_bool().unwrap_or(false);
            let button =
                |id: String, label: &'static str, decision: &'static str, primary: bool| {
                    div()
                        .id(SharedString::from(format!("ws-approval-{id}-{decision}")))
                        .debug_selector(|| format!("ws-approval-{id}-{decision}"))
                        .role(Role::Button)
                        .aria_label(label)
                        .cursor_pointer()
                        .h(px(26.))
                        .px_2()
                        .rounded_md()
                        .flex()
                        .items_center()
                        .text_size(px(12.))
                        .when(primary, |d| d.bg(content(0.9)).text_color(shell_fill()))
                        .when(!primary, |d| {
                            d.bg(content(0.06)).hover(|s| s.bg(content(0.1)))
                        })
                        .child(label)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.decide_approval(id.clone(), decision, cx)
                        }))
                        .into_any_element()
                };
            out.push(
                div()
                    .p_2()
                    .rounded_md()
                    .bg(hsla(0.11, 0.75, 0.45, 0.12))
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(icon("square-lock", 14.).text_color(hsla(0.11, 0.8, 0.6, 1.)))
                            .child(div().flex_1().text_size(px(13.)).child(title)),
                    )
                    .when(!detail_text.is_empty(), |d| {
                        d.child(
                            div()
                                .text_size(px(11.))
                                .line_height(px(16.))
                                .text_color(content(0.55))
                                .child(detail_text),
                        )
                    })
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap_2()
                            .child(button(request_id.clone(), "Deny", "deny", false))
                            .when(can_remember, |d| {
                                d.child(button(
                                    request_id.clone(),
                                    "Allow for session",
                                    "session",
                                    false,
                                ))
                            })
                            .child(button(request_id.clone(), "Allow once", "allow", true)),
                    )
                    .into_any_element(),
            );
        }

        let question = interaction["question"].clone();
        if question.is_object() {
            let mut sections: Vec<AnyElement> = Vec::new();
            for item in question["questions"].as_array().into_iter().flatten() {
                let item_id = item["id"].as_str().unwrap_or_default().to_string();
                if item_id.is_empty() {
                    continue;
                }
                let multi = item["multiSelect"].as_bool().unwrap_or(false);
                let selected = detail
                    .question_answers
                    .get(&item_id)
                    .cloned()
                    .unwrap_or_default();
                let options = item["options"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|option| {
                        let id = option["id"].as_str()?.to_string();
                        let label = option["label"].as_str().unwrap_or(&id).to_string();
                        let chosen = selected.contains(&id);
                        let item_for_click = item_id.clone();
                        let option_for_click = id.clone();
                        Some(
                            div()
                                .id(SharedString::from(format!(
                                    "ws-question-option-{}-{}",
                                    item_id, id
                                )))
                                .debug_selector(|| format!("ws-question-option-{}-{}", item_id, id))
                                .role(Role::Button)
                                .aria_label(label.clone())
                                .aria_selected(chosen)
                                .cursor_pointer()
                                .px_2()
                                .py_1()
                                .rounded_md()
                                .text_size(px(12.))
                                .when(chosen, |d| d.bg(content(0.14)))
                                .when(!chosen, |d| {
                                    d.bg(content(0.04)).hover(|s| s.bg(content(0.08)))
                                })
                                .child(label)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if let Some(detail) = this.detail.as_mut() {
                                        let answers = detail
                                            .question_answers
                                            .entry(item_for_click.clone())
                                            .or_default();
                                        if multi {
                                            if let Some(index) = answers
                                                .iter()
                                                .position(|id| *id == option_for_click)
                                            {
                                                answers.remove(index);
                                            } else {
                                                answers.push(option_for_click.clone());
                                            }
                                        } else {
                                            *answers = vec![option_for_click.clone()];
                                        }
                                    }
                                    cx.notify();
                                }))
                                .into_any_element(),
                        )
                    })
                    .collect::<Vec<_>>();
                sections.push(
                    div()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child(
                            div()
                                .text_size(px(13.))
                                .child(item["prompt"].as_str().unwrap_or("Question").to_string()),
                        )
                        .child(div().flex().flex_wrap().gap_2().children(options))
                        .when_some(detail.question_custom.get(&item_id), |d, input| {
                            d.child(
                                div()
                                    .border_1()
                                    .border_color(content(0.12))
                                    .rounded_md()
                                    .px_2()
                                    .child(input.clone()),
                            )
                        })
                        .into_any_element(),
                );
            }
            let has_timed_default = question["autoResolveAt"].as_u64().is_some();
            out.push(
                div()
                    .p_2()
                    .rounded_md()
                    .bg(content(0.05))
                    .flex()
                    .flex_col()
                    .gap_3()
                    .when_some(question["title"].as_str().map(str::to_owned), |d, title| {
                        d.child(div().font_weight(FontWeight::SEMIBOLD).child(title))
                    })
                    .children(sections)
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap_2()
                            .when(has_timed_default, |d| {
                                d.child(
                                    div()
                                        .id("ws-question-keep-open")
                                        .debug_selector(|| "ws-question-keep-open".into())
                                        .role(Role::Button)
                                        .aria_label("Keep question open")
                                        .cursor_pointer()
                                        .h(px(26.))
                                        .px_2()
                                        .rounded_md()
                                        .flex()
                                        .items_center()
                                        .text_size(px(12.))
                                        .bg(content(0.06))
                                        .child("Keep open")
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.answer_question(false, true, cx)
                                        })),
                                )
                            })
                            .child(
                                div()
                                    .id("ws-question-skip")
                                    .debug_selector(|| "ws-question-skip".into())
                                    .role(Role::Button)
                                    .aria_label("Skip question")
                                    .cursor_pointer()
                                    .h(px(26.))
                                    .px_2()
                                    .rounded_md()
                                    .flex()
                                    .items_center()
                                    .text_size(px(12.))
                                    .bg(content(0.06))
                                    .child("Skip")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.answer_question(true, false, cx)
                                    })),
                            )
                            .child(
                                div()
                                    .id("ws-question-answer")
                                    .debug_selector(|| "ws-question-answer".into())
                                    .role(Role::Button)
                                    .aria_label("Answer question")
                                    .cursor_pointer()
                                    .h(px(26.))
                                    .px_2()
                                    .rounded_md()
                                    .flex()
                                    .items_center()
                                    .text_size(px(12.))
                                    .bg(content(0.9))
                                    .text_color(shell_fill())
                                    .child("Answer")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.answer_question(false, false, cx)
                                    })),
                            ),
                    )
                    .into_any_element(),
            );
        }
        out
    }

    fn detail_view(&mut self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        self.ensure_question_inputs(cx);
        let detail = self.detail.as_ref()?;
        let transcript = transcript_messages(&detail.turns);
        let empty = transcript.is_empty();
        let messages = transcript
            .iter()
            .map(|message| {
                let user = message["role"].as_str() == Some("user");
                div()
                    .p_2()
                    .rounded_md()
                    .when(user, |d| d.bg(content(0.08)))
                    .when(!user, |d| d.bg(content(0.03)))
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(content(0.45))
                            .child(if user { "You" } else { "Mono" }),
                    )
                    .child(
                        div()
                            .text_size(px(13.))
                            .child(message["text"].as_str().unwrap_or_default().to_string()),
                    )
                    .into_any_element()
            })
            .collect::<Vec<_>>();
        Some(
            div()
                .flex()
                .flex_col()
                .size_full()
                .child(
                    div()
                        .h(px(40.))
                        .flex_shrink_0()
                        .px_3()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .id("ws-back")
                                .debug_selector(|| "ws-back".into())
                                .role(Role::Button)
                                .aria_label("Back")
                                .cursor_pointer()
                                .child(icon("chevron-left", 14.))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.detail = None;
                                    cx.notify();
                                })),
                        )
                        .child(div().size_2().rounded_full().bg(hsla_parse(&detail.color)))
                        .child(
                            div()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(detail.name.clone()),
                        )
                        .child(
                            div()
                                .text_size(px(11.))
                                .text_color(content(0.45))
                                .child(if detail.busy { "Working" } else { "Idle" }),
                        )
                        .when(detail.can_cancel, |d| {
                            d.child(
                                div()
                                    .id("ws-cancel-mono")
                                    .debug_selector(|| "ws-cancel-mono".into())
                                    .role(Role::Button)
                                    .aria_label("Cancel remote Mono")
                                    .cursor_pointer()
                                    .ml_auto()
                                    .h(px(26.))
                                    .px_2()
                                    .rounded_md()
                                    .flex()
                                    .items_center()
                                    .text_size(px(12.))
                                    .bg(content(0.06))
                                    .hover(|s| s.bg(content(0.1)))
                                    .child(if detail.acting {
                                        "Working…"
                                    } else {
                                        "Cancel"
                                    })
                                    .on_click(cx.listener(|this, _, _, cx| this.cancel_mono(cx))),
                            )
                        }),
                )
                .child(
                    div()
                        .id("ws-transcript")
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .px_3()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .when(detail.older || detail.loading_older, |d| {
                            d.child(
                                div()
                                    .id("ws-load-older")
                                    .debug_selector(|| "ws-load-older".into())
                                    .role(Role::Button)
                                    .aria_label("Load older messages")
                                    .cursor_pointer()
                                    .self_center()
                                    .px_3()
                                    .py_1()
                                    .rounded_md()
                                    .bg(content(0.05))
                                    .hover(|s| s.bg(content(0.09)))
                                    .text_size(px(12.))
                                    .text_color(content(0.6))
                                    .child(if detail.loading_older {
                                        "Loading older messages…"
                                    } else {
                                        "Load older messages"
                                    })
                                    .on_click(cx.listener(|this, _, _, cx| this.load_older(cx))),
                            )
                        })
                        .when(empty, |d| {
                            d.child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(content(0.45))
                                    .child("No history yet — send the first message below."),
                            )
                        })
                        .children(messages),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .p_3()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .children(self.interaction_view(cx))
                        .when(!detail.notice.is_empty(), |d| {
                            d.child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(hsla(0.0, 0.7, 0.55, 1.0))
                                    .child(detail.notice.clone()),
                            )
                        })
                        .child(
                            div()
                                .border_1()
                                .border_color(content(0.12))
                                .rounded_md()
                                .px_2()
                                .child(detail.input.clone()),
                        )
                        .child(div().text_size(px(11.)).text_color(content(0.4)).child(
                            if detail.sending {
                                "Sending…"
                            } else {
                                "Enter sends to the Mono on this machine"
                            },
                        )),
                ),
        )
    }
}

impl gpui::Render for Workspaces {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let machine_rows = self
            .machines
            .clone()
            .iter()
            .map(|machine| self.machine_row(machine, cx).into_any_element())
            .collect::<Vec<_>>();
        let body: AnyElement = if self.machines.is_empty() {
            div()
                .text_size(px(13.))
                .text_color(content(0.5))
                .child("No machines connected yet. Add one in Settings → Connections, then pick it here.")
                .into_any_element()
        } else if self.detail.is_some() {
            match self.detail_view(cx) {
                Some(view) => view.into_any_element(),
                None => div().into_any_element(),
            }
        } else if let Some(selected) = self.selected.clone() {
            let replica = self.replicas.get(&selected);
            let catalog = replica.and_then(|replica| replica.catalog.clone());
            match (replica, catalog) {
                (Some(replica), Some(catalog)) if replica.readiness == "ready" => {
                    let monos = catalog["monos"].as_array().cloned().unwrap_or_default();
                    let projects = catalog["projects"].as_array().cloned().unwrap_or_default();
                    let mut list = div().flex().flex_col().gap_2();
                    if monos.is_empty() {
                        list = list.child(
                            div()
                                .text_size(px(13.))
                                .text_color(content(0.5))
                                .child("This workspace has no Monos yet."),
                        );
                    }
                    for entry in &monos {
                        list = list.child(self.mono_card(entry, cx));
                    }
                    if !projects.is_empty() {
                        list = list.child(
                            div()
                                .pt_2()
                                .text_size(px(11.))
                                .text_color(content(0.45))
                                .child("Projects"),
                        );
                        for entry in &projects {
                            list = list.child(self.project_row(entry));
                        }
                    }
                    div()
                        .flex()
                        .flex_col()
                        .size_full()
                        .child(
                            div()
                                .px_3()
                                .py_2()
                                .flex_shrink_0()
                                .text_size(px(11.))
                                .text_color(content(0.45))
                                .child(format!(
                                    "{} Monos · {} projects{}",
                                    monos.len(),
                                    projects.len(),
                                    if replica.offline {
                                        " · offline, last known state"
                                    } else {
                                        ""
                                    }
                                )),
                        )
                        .child(
                            div()
                                .id("ws-catalog")
                                .debug_selector(|| "ws-catalog".into())
                                .flex_1()
                                .min_h_0()
                                .overflow_y_scroll()
                                .p_3()
                                .child(list),
                        )
                        .into_any_element()
                }
                (Some(replica), _) => div()
                    .p_3()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(div().text_size(px(13.)).text_color(content(0.6)).child(
                        match replica.readiness.as_str() {
                            "unbound" => "This machine has no workspace enabled yet.".to_string(),
                            "owner_unavailable" => {
                                "The workspace owner is not running on this machine.".to_string()
                            }
                            other => format!("Workspace is {other}…"),
                        },
                    ))
                    .when(!replica.detail.is_empty(), |d| {
                        d.child(
                            div()
                                .text_size(px(11.))
                                .text_color(content(0.4))
                                .child(replica.detail.clone()),
                        )
                    })
                    .into_any_element(),
                _ => div()
                    .p_3()
                    .text_size(px(13.))
                    .text_color(content(0.5))
                    .child("Connecting…")
                    .into_any_element(),
            }
        } else {
            div()
                .p_3()
                .text_size(px(13.))
                .text_color(content(0.5))
                .child("Pick a machine to see its Monos.")
                .into_any_element()
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .h(px(40.))
                    .flex_shrink_0()
                    .px_3()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(
                        div()
                            .text_size(px(13.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(content(0.7))
                            .mr_2()
                            .child("Machines"),
                    )
                    .children(machine_rows),
            )
            .child(div().flex_1().min_h_0().child(body))
    }
}

/// One round against the host: the workspace section of environment.describe,
/// then a full catalog refill when the owner is ready.
fn poll_workspace(
    remote: &Arc<Connections>,
    machine_id: &str,
) -> Result<(String, String, Option<Value>, String), String> {
    let describe = remote.request(machine_id, "environment.describe", json!({}))?;
    let workspace = &describe["workspace"];
    let readiness = workspace["readiness"]
        .as_str()
        .unwrap_or("unbound")
        .to_string();
    let detail = workspace["detail"].as_str().unwrap_or_default().to_string();
    let environment_id = describe["environmentId"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    if readiness != "ready" {
        return Ok((readiness, detail, None, environment_id));
    }
    let catalog = remote.request(machine_id, "workspace.catalog", json!({}))?;
    let catalog = if catalog["complete"].as_bool() == Some(true) {
        Some(catalog)
    } else {
        None
    };
    Ok((readiness, detail, catalog, environment_id))
}

/// Tail refreshes are authoritative for turns they include, but must not drop
/// older pages the user explicitly loaded. Existing IDs update in place; new
/// tail turns append in order.
fn merge_latest_turns(loaded: &mut Vec<Value>, incoming: Vec<Value>) {
    for turn in incoming {
        let Some(id) = turn["id"].as_str() else {
            continue;
        };
        if let Some(index) = loaded
            .iter()
            .position(|existing| existing["id"].as_str() == Some(id))
        {
            loaded[index] = turn;
        } else {
            loaded.push(turn);
        }
    }
}

/// Older pages are contiguous immediately before the turn the client asked
/// from. Insert only missing turns there; an overlap refreshes in place so a
/// slow tail refresh and a paging answer cannot duplicate one another.
fn insert_older_turns(loaded: &mut Vec<Value>, before: &str, incoming: Vec<Value>) {
    let mut insert_at = loaded
        .iter()
        .position(|turn| turn["id"].as_str() == Some(before))
        .unwrap_or(0);
    for turn in incoming {
        let Some(id) = turn["id"].as_str() else {
            continue;
        };
        if let Some(index) = loaded
            .iter()
            .position(|existing| existing["id"].as_str() == Some(id))
        {
            loaded[index] = turn;
            continue;
        }
        loaded.insert(insert_at, turn);
        insert_at += 1;
    }
}

/// The workspace detail still renders a deliberately compact transcript. It
/// reads the same turn projection the phone uses, though, so paging and later
/// interactive cards share one contract instead of the lossy `messages` tail.
fn transcript_messages(turns: &[Value]) -> Vec<Value> {
    let mut out = Vec::new();
    for turn in turns {
        let turn_id = turn["id"].as_str().unwrap_or_default();
        let Some(items) = turn["items"].as_array() else {
            continue;
        };
        for item in items {
            let kind = item["type"].as_str().unwrap_or_default();
            let (role, text) = match kind {
                "user" => (
                    "user",
                    item["text"].as_str().unwrap_or_default().to_string(),
                ),
                "text" | "notice" => (
                    "assistant",
                    item["text"].as_str().unwrap_or_default().to_string(),
                ),
                "choices" => {
                    let options = item["options"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(" · ");
                    let text = match item["chosen"].as_str() {
                        Some(chosen) => format!("Chose: {chosen}"),
                        None if !options.is_empty() => format!("Choices: {options}"),
                        None => String::new(),
                    };
                    ("assistant", text)
                }
                _ => continue,
            };
            if text.trim().is_empty() {
                continue;
            }
            let item_id = item["id"].as_str().unwrap_or_default();
            out.push(json!({
                "id": format!("{turn_id}:{item_id}"),
                "role": role,
                "text": text,
            }));
        }
    }
    out
}

fn hsla_parse(hex_color: &str) -> gpui::Hsla {
    let value = u32::from_str_radix(hex_color.trim_start_matches('#'), 16).unwrap_or(0x888888);
    let r = ((value >> 16) & 0xff) as f32 / 255.;
    let g = ((value >> 8) & 0xff) as f32 / 255.;
    let b = (value & 0xff) as f32 / 255.;
    gpui::Rgba { r, g, b, a: 1.0 }.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(id: &str, text: &str) -> Value {
        json!({
            "id": id,
            "items": [{"type": "text", "id": format!("{id}:text"), "text": text}],
        })
    }

    fn ids(turns: &[Value]) -> Vec<&str> {
        turns
            .iter()
            .map(|turn| turn["id"].as_str().unwrap())
            .collect()
    }

    #[test]
    fn latest_refresh_updates_in_place_and_preserves_paged_history() {
        let mut loaded = vec![turn("p9t0", "old"), turn("t0", "tail"), turn("t2", "next")];
        merge_latest_turns(
            &mut loaded,
            vec![
                turn("t0", "tail updated"),
                turn("t2", "next updated"),
                turn("t4", "new"),
            ],
        );
        assert_eq!(ids(&loaded), ["p9t0", "t0", "t2", "t4"]);
        assert_eq!(loaded[1]["items"][0]["text"], "tail updated");
    }

    #[test]
    fn older_pages_insert_before_their_anchor_without_duplicates() {
        let mut loaded = vec![turn("t0", "tail"), turn("t2", "next")];
        insert_older_turns(
            &mut loaded,
            "t0",
            vec![turn("p9t0", "older"), turn("p9t2", "page")],
        );
        assert_eq!(ids(&loaded), ["p9t0", "p9t2", "t0", "t2"]);
        insert_older_turns(
            &mut loaded,
            "t0",
            vec![turn("p9t0", "older updated"), turn("p9t2", "page")],
        );
        assert_eq!(ids(&loaded), ["p9t0", "p9t2", "t0", "t2"]);
        assert_eq!(loaded[0]["items"][0]["text"], "older updated");
    }

    #[test]
    fn compact_transcript_reads_user_text_notice_and_choice_items() {
        let turns = vec![json!({
            "id": "t0",
            "items": [
                {"type": "user", "id": "r0", "text": "please"},
                {"type": "text", "id": "r1", "text": "done"},
                {"type": "notice", "id": "input:a1", "text": "Waiting for approval"},
                {"type": "choices", "id": "c1", "options": ["yes", "no"]},
                {"type": "document", "id": "d1", "title": "hidden in compact mode"},
            ],
        })];
        let messages = transcript_messages(&turns);
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[2]["text"], "Waiting for approval");
        assert_eq!(messages[3]["text"], "Choices: yes · no");
    }
    use gpui::{TestAppContext, VisualTestContext};
    use monocode_core::remote::Machine;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread::JoinHandle;

    #[derive(Default)]
    struct HostState {
        approval: bool,
        question: bool,
        busy: bool,
        requests: Vec<Value>,
    }

    struct InteractionHost {
        client: Arc<Connections>,
        machine: Machine,
        url: String,
        state: Arc<Mutex<HostState>>,
        accepting: Arc<AtomicBool>,
        stop: Arc<AtomicBool>,
        server: Option<JoinHandle<()>>,
        directory: std::path::PathBuf,
    }

    impl InteractionHost {
        fn new() -> Self {
            let environment = format!("interaction-{}", monocode_core::mono::new_id());
            let directory = std::env::temp_dir().join(&environment);
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let state = Arc::new(Mutex::new(HostState {
                approval: true,
                question: true,
                busy: true,
                ..Default::default()
            }));
            let stop = Arc::new(AtomicBool::new(false));
            let accepting = Arc::new(AtomicBool::new(true));
            let (served, done, env, accept_requests) = (
                state.clone(),
                stop.clone(),
                environment.clone(),
                accepting.clone(),
            );
            let server = std::thread::spawn(move || {
                while !done.load(Ordering::Relaxed) {
                    let (mut stream, _) = match listener.accept() {
                        Ok(value) => value,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(std::time::Duration::from_millis(1));
                            continue;
                        }
                        Err(error) => panic!("host accept: {error}"),
                    };
                    if !accept_requests.load(Ordering::Relaxed) {
                        drop(stream);
                        continue;
                    }
                    stream.set_nonblocking(false).unwrap();
                    stream
                        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                        .unwrap();
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut length = 0;
                    loop {
                        let mut line = String::new();
                        reader.read_line(&mut line).unwrap();
                        if line == "\r\n" {
                            break;
                        }
                        if let Some((key, value)) = line.split_once(':')
                            && key.eq_ignore_ascii_case("content-length")
                        {
                            length = value.trim().parse().unwrap();
                        }
                    }
                    let mut body = vec![0; length];
                    reader.read_exact(&mut body).unwrap();
                    let request: Value = serde_json::from_slice(&body).unwrap();
                    let mut state = served.lock().unwrap();
                    state.requests.push(request.clone());
                    let body = match request["method"].as_str().unwrap_or_default() {
                        "environment.describe" => json!({"result":{
                            "protocolVersion":1,
                            "environmentId":env,
                            "name":"Fake",
                            "providers":["codex"],
                            "capabilities":["sessions.sync"],
                            "workspace":{"readiness":"ready","detail":""}
                        }}),
                        "workspace.catalog" => json!({"result":{
                            "complete":true,
                            "sourceId":"source",
                            "catalogRevision":1,
                            "monos":[{
                                "id":"mono-1",
                                "name":"Remote Mono",
                                "color":"#3366ff",
                                "status":"running",
                                "busy":state.busy,
                                "lastMessage":"working"
                            }],
                            "projects":[]
                        }}),
                        "monos.history" => json!({"result":{
                            "turns":[],
                            "older":false,
                            "busy":state.busy,
                            "conversationId":1
                        }}),
                        "monos.profile" => {
                            let approvals = if state.approval {
                                json!([{
                                    "requestId":"approval-1",
                                    "title":"Run tests?",
                                    "detail":"The Mono wants to run cargo test.",
                                    "canRemember":true
                                }])
                            } else {
                                json!([])
                            };
                            let question = if state.question {
                                json!({
                                    "requestId":42,
                                    "title":"Choose a direction",
                                    "autoResolveAt":null,
                                    "questions":[{
                                        "id":"choice",
                                        "prompt":"Which answer?",
                                        "multiSelect":false,
                                        "allowCustom":false,
                                        "options":[{"id":"yes","label":"Yes"}]
                                    }]
                                })
                            } else {
                                Value::Null
                            };
                            json!({"result":{
                                "status":"working",
                                "busy":state.busy,
                                "canCancel":state.busy,
                                "interaction":{"waiting":true,"approvals":approvals,"question":question}
                            }})
                        }
                        "workspace.commands.dispatch" => {
                            let command = request["params"]["command"].clone();
                            let action = command["action"].as_str().unwrap_or_default();
                            let result = match action {
                                "mono.decideApproval" => {
                                    state.approval = false;
                                    json!({"monoId":"mono-1","decided":true})
                                }
                                "mono.answerQuestion" => {
                                    if command["payload"].get("reply").is_some() {
                                        state.question = false;
                                    }
                                    json!({"monoId":"mono-1","answered":true})
                                }
                                "mono.cancel" => {
                                    state.busy = false;
                                    json!({"monoId":"mono-1","cancelled":true})
                                }
                                other => json!({"error":format!("not served: {other}")}),
                            };
                            json!({"result":{
                                "commandId":command["commandId"],
                                "status":"completed",
                                "result":result,
                                "ownerEpoch":"owner"
                            }})
                        }
                        other => json!({"error":format!("not served: {other}")}),
                    };
                    drop(state);
                    let body = body.to_string();
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    )
                    .unwrap();
                    stream.flush().unwrap();
                }
            });
            let client = Arc::new(Connections::new(&directory));
            let machine = client
                .connect(String::new(), url.clone(), "a".repeat(43))
                .unwrap();
            Self {
                client,
                machine,
                url,
                state,
                accepting,
                stop,
                server: Some(server),
                directory,
            }
        }

        fn set_accepting(&self, accepting: bool) {
            self.accepting.store(accepting, Ordering::Relaxed);
        }

        fn commands(&self, action: &str) -> Vec<Value> {
            self.state
                .lock()
                .unwrap()
                .requests
                .iter()
                .filter(|request| request["method"] == "workspace.commands.dispatch")
                .map(|request| request["params"]["command"].clone())
                .filter(|command| command["action"] == action)
                .collect()
        }
    }

    impl Drop for InteractionHost {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(server) = self.server.take() {
                let _ = server.join();
            }
            let _ = std::fs::remove_dir_all(&self.directory);
        }
    }

    fn open_interaction_workspace(
        cx: &mut TestAppContext,
        host: &InteractionHost,
    ) -> gpui::WindowHandle<Workspaces> {
        cx.update(|cx| cx.set_reduce_motion(true));
        let client = host.client.clone();
        cx.update(|cx| {
            cx.open_window(Default::default(), |_, cx| {
                cx.new(|cx| Workspaces::new(client.clone(), cx))
            })
            .unwrap()
        })
    }

    fn read<R>(
        window: &gpui::WindowHandle<Workspaces>,
        cx: &mut VisualTestContext,
        f: impl FnOnce(&mut Workspaces, &mut Context<Workspaces>) -> R,
    ) -> R {
        window.update(cx, |view, _, cx| f(view, cx)).unwrap()
    }

    fn wait_for(cx: &mut VisualTestContext, mut done: impl FnMut(&mut VisualTestContext) -> bool) {
        for _ in 0..3000 {
            cx.run_until_parked();
            if done(cx) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        panic!("condition not reached");
    }

    fn click(visual: &mut VisualTestContext, selector: &'static str) {
        visual.refresh().unwrap();
        let bounds = visual
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("missing interaction element {selector}"));
        visual.simulate_click(bounds.center(), Default::default());
        visual.refresh().unwrap();
    }

    #[gpui::test]
    fn interaction_cards_dispatch_approval_question_and_cancel_commands(cx: &mut TestAppContext) {
        let host = InteractionHost::new();
        assert!(!host.machine.environment_id.is_empty());
        let window = open_interaction_workspace(cx, &host);
        let visual = &mut VisualTestContext::from_window(window.into(), cx);
        wait_for(visual, |cx| {
            read(&window, cx, |view, _| {
                view.replicas
                    .values()
                    .any(|replica| replica.catalog.is_some() && replica.readiness == "ready")
            })
        });
        visual.refresh().unwrap();
        assert!(
            visual.debug_bounds("ws-catalog").is_some(),
            "catalog container did not render"
        );
        click(visual, "ws-mono-mono-1");
        wait_for(visual, |cx| {
            read(&window, cx, |view, _| {
                view.detail
                    .as_ref()
                    .is_some_and(|detail| detail.profile.is_some())
            })
        });

        click(visual, "ws-approval-approval-1-allow");
        wait_for(visual, |cx| {
            read(&window, cx, |view, _| {
                view.detail.as_ref().is_some_and(|detail| {
                    detail.profile.as_ref().is_some_and(|profile| {
                        profile["interaction"]["approvals"]
                            .as_array()
                            .is_some_and(|approvals| approvals.is_empty())
                    })
                })
            })
        });
        let approvals = host.commands("mono.decideApproval");
        assert_eq!(approvals.len(), 1);
        assert_eq!(approvals[0]["payload"]["requestId"], "approval-1");
        assert_eq!(approvals[0]["payload"]["decision"], "allow");

        click(visual, "ws-question-option-choice-yes");
        click(visual, "ws-question-answer");
        wait_for(visual, |cx| {
            read(&window, cx, |view, _| {
                view.detail.as_ref().is_some_and(|detail| {
                    detail
                        .profile
                        .as_ref()
                        .is_some_and(|profile| profile["interaction"]["question"].is_null())
                })
            })
        });
        let answers = host.commands("mono.answerQuestion");
        assert_eq!(answers.len(), 1);
        assert_eq!(answers[0]["payload"]["requestId"], 42);
        assert_eq!(answers[0]["payload"]["reply"]["kind"], "answered");
        assert_eq!(
            answers[0]["payload"]["reply"]["answers"]["choice"],
            json!(["yes"])
        );

        click(visual, "ws-cancel-mono");
        wait_for(visual, |cx| {
            read(&window, cx, |view, _| {
                view.detail.as_ref().is_some_and(|detail| !detail.busy)
            })
        });
        let cancels = host.commands("mono.cancel");
        assert_eq!(cancels.len(), 1);
        assert_eq!(cancels[0]["target"]["id"], "mono-1");
    }

    #[gpui::test]
    fn multiple_machines_keep_catalogs_and_open_details_scoped(cx: &mut TestAppContext) {
        let first = InteractionHost::new();
        let second = InteractionHost::new();
        let second_machine = first
            .client
            .connect(String::new(), second.url.clone(), "a".repeat(43))
            .unwrap();
        assert_ne!(first.machine.id, second_machine.id);
        assert_eq!(
            first
                .client
                .machine_for_environment(&first.machine.environment_id)
                .unwrap()
                .id,
            first.machine.id
        );
        assert_eq!(
            first
                .client
                .machine_for_environment(&second_machine.environment_id)
                .unwrap()
                .id,
            second_machine.id
        );

        let window = open_interaction_workspace(cx, &first);
        let visual = &mut VisualTestContext::from_window(window.into(), cx);
        read(&window, visual, |view, _| {
            assert_eq!(view.machines.len(), 2);
            assert_eq!(view.selected, None);
        });
        let first_id = first.machine.id.clone();
        read(&window, visual, |view, cx| {
            view.select(first_id.clone(), cx)
        });
        wait_for(visual, |cx| {
            read(&window, cx, |view, _| {
                view.replicas
                    .get(&first_id)
                    .is_some_and(|replica| replica.catalog.is_some())
            })
        });
        read(&window, visual, |view, cx| {
            let entry = view.replicas[&first_id].catalog.as_ref().unwrap()["monos"][0].clone();
            view.open_mono(&entry, cx);
            assert_eq!(
                view.detail.as_ref().map(|detail| detail.id.as_str()),
                Some("mono-1")
            );
        });

        let second_id = second_machine.id.clone();
        read(&window, visual, |view, cx| {
            view.select(second_id.clone(), cx)
        });
        read(&window, visual, |view, _| {
            assert_eq!(view.selected.as_deref(), Some(second_id.as_str()));
            assert!(
                view.detail.is_none(),
                "switching machines must not leak the previous machine's detail"
            );
        });
    }

    #[test]
    fn workspace_poll_recovers_after_the_host_becomes_reachable_again() {
        let host = InteractionHost::new();
        let (readiness, _, catalog, environment) =
            poll_workspace(&host.client, &host.machine.id).unwrap();
        assert_eq!(readiness, "ready");
        assert!(catalog.is_some());
        assert_eq!(environment, host.machine.environment_id);

        host.set_accepting(false);
        assert!(
            poll_workspace(&host.client, &host.machine.id).is_err(),
            "dropped host connections must surface as an offline poll"
        );

        host.set_accepting(true);
        let (readiness, _, catalog, environment) =
            poll_workspace(&host.client, &host.machine.id).unwrap();
        assert_eq!(readiness, "ready");
        assert!(catalog.is_some());
        assert_eq!(environment, host.machine.environment_id);
    }
}
