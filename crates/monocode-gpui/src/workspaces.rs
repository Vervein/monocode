//! A connected machine's workspace, as the phone models it: the Monos the
//! owner's catalog projects, opened into a transcript and composer that go
//! through workspace commands. Nothing here schedules or stores anything
//! locally — the replica is a read cache, the owner's receipt is the truth.
use crate::{
    input::{self, Input},
    theme::*,
};
use gpui::{
    AnyElement, App, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight,
    MouseButton, Role, SharedString, Subscription, Window, div, hsla, prelude::*, px,
};
use monocode_core::remote::{Connections, Machine};
use serde_json::{Value, json};
use std::{collections::HashMap, sync::Arc, time::Duration};

/// Catalog and history refresh share one interval: the owner answers both
/// from its own projection, so polling is the protocol's current push.
const POLL: Duration = Duration::from_secs(3);

pub(crate) enum Event {
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
    id: String,
    name: String,
    color: String,
    profile: Option<Value>,
    messages: Vec<Value>,
    busy: bool,
    input: Entity<Input>,
    sending: bool,
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
    error: String,
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
            error: String::new(),
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

    fn select(&mut self, machine_id: String, cx: &mut Context<Self>) {
        self.selected = Some(machine_id.clone());
        self.detail = None;
        self.start_sync(machine_id, cx);
        cx.notify();
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
        let Some(machine_id) = self.selected.clone() else {
            return;
        };
        let id = entry["id"].as_str().unwrap_or_default().to_string();
        if id.is_empty() {
            return;
        }
        let input = cx.new(|cx| {
            input::Input::field("Message the Mono", "Ask it to do something…", content(0.35), cx)
        });
        self._events.push(
            cx.subscribe(&input, |this, _, _: &input::Submitted, cx| this.send(cx)),
        );
        self.detail = Some(MonoDetail {
            name: entry["name"].as_str().unwrap_or("Mono").to_string(),
            color: entry["color"].as_str().unwrap_or("#888888").to_string(),
            id: id.clone(),
            profile: None,
            messages: Vec::new(),
            busy: entry["busy"].as_bool().unwrap_or(false),
            input,
            sending: false,
            notice: String::new(),
            revision: 0,
        });
        self.refresh_detail(cx);
        // While the detail is open, refresh its transcript on the same
        // cadence as the catalog; closing it stops the loop.
        let remote = self.remote.clone();
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(POLL).await;
                let alive = this
                    .update(cx, |this, cx| {
                        if this.detail.as_ref().is_some_and(|detail| detail.id == id) {
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
                if detail.revision != revision {
                    return;
                }
                match history {
                    Ok(page) => {
                        detail.messages = page["messages"].as_array().cloned().unwrap_or_default();
                        detail.busy = page["busy"].as_bool().unwrap_or(false);
                        detail.notice = String::new();
                    }
                    Err(error) => detail.notice = error,
                }
                detail.profile = profile;
                cx.notify();
            });
        })
        .detach();
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
        let command = json!({
            "commandId": monocode_core::mono::new_id(),
            "environmentId": environment_id,
            "workspaceSourceId": source_id,
            "target": {"kind": "mono", "id": mono_id},
            "action": "mono.send",
            "payload": {"text": text},
        });
        let remote = self.remote.clone();
        let machine = machine_id.clone();
        let work = cx
            .background_executor()
            .spawn(async move { remote.request(&machine, "workspace.commands.dispatch", json!({"command": command})) });
        cx.spawn(async move |this, cx| {
            let outcome = work.await;
            let _ = this.update(cx, |this, cx| {
                let Some(detail) = this.detail.as_mut() else {
                    return;
                };
                detail.sending = false;
                match outcome {
                    Ok(receipt) => {
                        let status = receipt["status"].as_str().unwrap_or("unknown");
                        if status == "completed" {
                            detail.input.update(cx, |input, cx| {
                                input.set_text(String::new(), cx)
                            });
                            detail.revision += 1;
                            detail.notice = String::new();
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
            .when(active, |d| d.bg(content(0.1)).font_weight(FontWeight::SEMIBOLD))
            .when(!active, |d| d.text_color(content(0.65)).hover(|s| s.bg(content(0.06))))
            .child(machine.name.clone())
            .on_click(cx.listener(move |this, _, _, cx| this.select(id.clone(), cx)))
    }

    fn mono_card(&self, entry: &Value, cx: &mut Context<Self>) -> impl IntoElement {
        let name = entry["name"].as_str().unwrap_or("Mono").to_string();
        let status = entry["status"].as_str().unwrap_or("idle").to_string();
        let preview = entry["lastMessage"].as_str().unwrap_or_default().to_string();
        let color = entry["color"].as_str().unwrap_or("#888888").to_string();
        let entry = entry.clone();
        div()
            .id(SharedString::from(format!("ws-mono-{}", entry["id"].as_str().unwrap_or_default())))
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

    fn detail_view(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let detail = self.detail.as_ref()?;
        let messages = detail
            .messages
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
        let empty = messages.is_empty();
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
                                .role(Role::Button)
                                .aria_label("Back")
                                .cursor_pointer()
                                .child(icon("chevron-left", 14.))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.detail = None;
                                    cx.notify();
                                })),
                        )
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
                        ),
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
                        .child(
                            div()
                                .text_size(px(11.))
                                .text_color(content(0.4))
                                .child(if detail.sending {
                                    "Sending…"
                                } else {
                                    "Enter sends to the Mono on this machine"
                                }),
                        ),
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
                                    if replica.offline { " · offline, last known state" } else { "" }
                                )),
                        )
                        .child(div().id("ws-catalog").flex_1().min_h_0().overflow_y_scroll().p_3().child(list))
                        .into_any_element()
                }
                (Some(replica), _) => div()
                    .p_3()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .text_size(px(13.))
                            .text_color(content(0.6))
                            .child(match replica.readiness.as_str() {
                                "unbound" => "This machine has no workspace enabled yet.".to_string(),
                                "owner_unavailable" => {
                                    "The workspace owner is not running on this machine.".to_string()
                                }
                                other => format!("Workspace is {other}…"),
                            }),
                    )
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
    let readiness = workspace["readiness"].as_str().unwrap_or("unbound").to_string();
    let detail = workspace["detail"].as_str().unwrap_or_default().to_string();
    let environment_id = describe["environmentId"].as_str().unwrap_or_default().to_string();
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

fn hsla_parse(hex_color: &str) -> gpui::Hsla {
    let value = u32::from_str_radix(hex_color.trim_start_matches('#'), 16).unwrap_or(0x888888);
    let r = ((value >> 16) & 0xff) as f32 / 255.;
    let g = ((value >> 8) & 0xff) as f32 / 255.;
    let b = (value & 0xff) as f32 / 255.;
    gpui::Rgba { r, g, b, a: 1.0 }.into()
}
