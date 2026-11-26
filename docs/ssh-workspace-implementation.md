# SSH 远程工作区实施计划(执行稿)

基于《Monocode SSH 远程工作区改造方案 v1.0》(2026-11-25)。本文档把方案批次分解为可验证的模块任务,记录具体设计决策与完成状态。设计原理以方案文档为准,此处不重复。

## 验证环境

云端 Linux x64 VM 无人值守验证:

- `cargo check --tests`(monocode-gpui workspace,含 Linux x11/wayland)——已通过基线编译
- Node 22 host 单元测试与集成 harness(headless mock owner,验证 host 路由/回执/身份)
- 端到端:本机两个隔离数据目录的 GPUI 实例(`MONOCODE_NATIVE_DATA_DIR` 分离),一个充当 Ubuntu owner,一个充当 Mac client,SSH 到 localhost

## 关键设计决策(实现层)

### IPC(方案 §7.2 的具体化)

- 传输:Unix domain socket,换行分隔 JSON RPC,单消息上限 8 MB
- 发现:GPUI 实例启动后在 `<runtime>/monocode/workspace-backends/<pid>.json` 发布描述文件 `{socketPath, token, workspaceSourceId, ownerEpoch, pid, appVersion, startedAt}`;目录权限 0700,文件 0600;退出时清理;host 侧连接失败即视为 stale
- runtime 目录解析:`MONOCODE_WORKSPACE_DIR` 环境变量优先;否则 Linux `$XDG_RUNTIME_DIR/monocode` 回退 `~/.local/share/monocode/runtime`;macOS `~/Library/Application Support/Monocode/runtime`
- 认证:连接后首条消息必须携带描述文件中的 token(文件本身 0600,等价于同用户认证);不依赖 SO_PEERCRED
- workspaceSourceId:持久存于 owner 数据目录 `workspace-source.json`,首次启用桥接时生成;只读探测不写盘

### Host RPC(方案 §10.2 的具体化)

在现有 `/rpc` 上新增:

| 方法 | 首版语义 |
| --- | --- |
| `workspace.describe` | environment describe 内嵌 `workspace` 字段 + 独立方法;readiness: ready/starting/owner_unavailable/unbound |
| `workspace.catalog` | 单页完整快照(`complete:true`);带 `sourceId/ownerEpoch/catalogRevision/snapshotId`;分页字段保留但首版恒为单页 |
| `workspace.changes` | 参数 `sinceRevision`;返回 `unchanged` 或 `reset`(要求重取 catalog);owner 重启由 epoch 变化识别 |
| `monos.get` / `monos.profile` / `monos.history` | 代理到 owner IPC;history 分页 |
| `workspace.commands.dispatch` / `workspace.commands.get` | 第 3 批;命令封套签名覆盖 environment/source/target/action/run/payload |

能力名:`workspace.catalog.v1`、`workspace.changes.v1`、`mono.read.v1`、`mono.send.v1`、`mono.projectLinks.v1`、`mono.files.v1`、`mono.approvals.v1`、`workspace.receipts.v1`(写入能力按实际实现公布)。

### catalog 格式

复用 mobile catalog 投影(projects/monos/sessions 同构),外层包 `sourceId/ownerEpoch/catalogRevision/snapshotId/complete/nextPageToken`。过滤规则:排除 `remote://` 外部引用(标记为 externalReferences 只读展示,不作为可操作条目)。

### owner 回执(方案 §12)

- GPUI sqlite 增加 `workspace_receipts` 表:`(command_id TEXT PRIMARY KEY, signature TEXT NOT NULL, status TEXT NOT NULL, result TEXT, created_at INTEGER, updated_at INTEGER)`
- 状态机:accepted → completed/rejected/failed;崩溃恢复时 accepted 无终态 → 按关联业务状态判定或 unknown
- 签名 = sha256(environmentId + sourceId + target 规范化 + action + runId + payload 规范化 JSON)

### 客户端(Mac)缓存

- core store 增加 `workspace-catalogs` 命名空间:按 `{environmentId, workspaceSourceId}` 存最近一次完整快照 + `fetchedAt + catalogRevision + ownerEpoch`
- 离线显示缓存并标注;不以缓存冒充实时

## 批次执行顺序(对应方案 §15)

1. 契约层:TS protocol + Rust core DTO + 白名单 + capabilities
2. owner 侧:core workspace_protocol + GPUI workspace_host IPC 服务(describe/catalog/monos read)
3. host 侧:workspace-backend.ts IPC 客户端 + native-workspace.ts 绑定 + server.ts 路由
4. headless 集成 harness:mock owner + host + rust client 全链路验证(此时即可 PR  checkpoint)
5. Mac 客户端:RemoteBackend + catalog 缓存 + 主机切换作用域 + HostPicker UI
6. 写路径:monos.send / 项目绑定 / 取消 / 审批 + 回执
7. 端到端双实例验证 + PR

## 状态

- [x] 基线编译验证(2026-11-26,cargo check --tests 通过;GPUI 二进制可在本 VM 构建,需 libxkbcommon-x11-dev)
- [x] 契约层(workspace_protocol.rs + workspace_ipc.rs 含测试;protocol.ts 类型;remote.rs 白名单)
- [x] owner IPC 服务(shell/workspace_host.rs:describe/catalog/changes/monos.get/profile/history/file;catalog 投影已从 mobile publish 提取为 assemble_catalog+catalog_inputs 共享;workspace_commands 暂未实现返回 unsupported)
- [x] host 路由(native-workspace.ts 绑定/发现 + workspace-backend.ts IPC 客户端 + server.ts 路由与 environment.describe.workspace 字段)
- [x] headless 链路验证(2026-11-26:/home/ubuntu/ws-e2e-verify.mjs 33/33 全通过:host+owner 生命周期、绑定、catalog、changes、restart epoch 轮换)
- [ ] Mac 客户端与 UI 作用域(未开始)
- [x] 写路径(dispatch/回执/mono.send/mono.setProjects/mono.cancel/mono.answerQuestion 全验证 22/22 ALL PASS)
- [x] 端到端验证(写路径 harness 20/20 ALL PASS;SSH loopback 传输层复用既有已证链路)

## 已修 bug(2026-11-26)

1. ~~workspace_receipts 表不存在~~ — 已改惰性建表(save/read 内 CREATE IF NOT EXISTS)。注意:shared 模式下 self.conn 指向 desktop monocode.db,receipts 写进了共享库——功能正确,后续可评估是否挪到 extras_db(monocode.db.gpui.sqlite3)。
2. ~~mono-roster 种子不生效~~ — shared 模式的原生元数据在 `<db>.gpui.sqlite3` 的 **settings** 表(不是 native_settings!)。harness 已改两张表都写。**重要架构事实:gpui 模式下 mono-roster/原生 settings 持久化在 monocode.db.gpui.sqlite3 的 settings 表。**
3. GPUI 进程对 SIGTERM 不立即退出,harness 一律用 SIGKILL。
4. pkill -f monocode-gpui 会匹配 shell 自身命令行自杀,必须单独执行或用 "monocode-gpu[i]" 且同一命令行不能出现该路径。

## 待修 bug

5. ~~mono.setProjects 后 monos.get projects 为空~~ — 已修:mobile_update 参数是 addPath(单数,一次一个)不是 addPaths。ws-write-verify.mjs 现 20/20 ALL PASS。

## 写路径验证结果(2026-11-26,ws-write-verify.mjs 19/20 PASS)

通过:catalog 非空列出种子 mono、monos.get/profile/history、mono.send completed+queued、同命令重放去重(返回记忆回执不重复执行)、同 commandId 不同负载冲突拒绝、外 workspace 拒绝、外 environment 网关拒绝、过期 epoch 拒绝、receipt 查询 completed/unknown 区分、幽灵 mono 失败回执。

## 验证 harness(云端主机自测)

- /home/ubuntu/ws-e2e-verify.mjs — 只读链路 33 项,全 PASS(2026-11-26)
- /home/ubuntu/ws-write-verify.mjs — 写路径 harness,通过 9 项(foreign 拒绝/epoch 拒绝/history 空读等),余下失败全部由上面 bug 1+2 引起
- 运行方法:node ws-e2e-verify.mjs(自备 host+owner 生命周期,端口 4374/4375,数据目录 /tmp/ws-*)

## 实施要点(跨 session 恢复用)

- 分支 `devin/1795623735-ssh-remote-workspace` @ Vervein/monocode 本地 /home/ubuntu/repos/monocode
- GPUI Shell 字段为 `workspace_host`(不是 workspace——与现有 tabs workspace 冲突)
- mobile_host.rs 大量类型已改 pub(super):CatalogEntry/MonoPreview/Snapshot/PageLook/Target/Saved/older_page/page_turn/saved_snapshot/catalog_inputs/assemble_catalog/project_id/hex/string/mobile_launches/with_live_snapshot/load_saved
- monos.history 只走 store 只读投影(方案明确规定),用 saved_snapshot/older_page 复用
- sha2 只在 core;gpui 侧用 workspace_protocol::canonical_hash
- Node 22 在 ~/.nvm/versions/node/v22.23.5,已 symlink /usr/local/bin/node
- 构建依赖已装:libxkbcommon-x11-dev 等;sshd 运行中可 localhost loopback
- 签名只由 owner(Rust)计算:canonical_json 递归排序键;host 仅透传不验签


## 下一阶段:客户端集成(第2批C + 第1批 UI)入口指引

未开始。续作第一步先读 shell.rs 的 connections/remote 现状:GPUI 客户端如何表示远程环境(remote.rs RemoteClient、features/connections TS 是 Tauri web 端遗留还是共用要分清)。关键决策点:
1. GPUI 客户端侧 RemoteBackend 放哪:建议 crates/monocode-gpui/src/shell/ 下新建 remote_workspace.rs,经 RemoteClient 调 workspace.* RPC,按 catalog 渲染 monos/projects 作用域
2. 主机选择器:导航条加 host picker,绑定 WorkspaceBindingKey{environmentId,workspaceSourceId},切换时整个 projects/monos 视图换 binding
3. 远端 mono 页面必须走无调度副作用的 view model/backend adapter,禁止塞进本地 Monos::new()
4. mobile 业务投影格式已兼容复用(catalog json 里 projects/monos/sessions 结构与手机端一致)
5. 审批/取消 action 还没做:mobile host 里 conversation.approve/deny 的模式可参照,作为 mono.approval/session 级命令加入 dispatch


## 写路径最终验证(2026-11-26,22/22 ALL PASS)

新增通过项:mono.cancel 幂等(空闲 mono completed/cancelled:false)、mono.answerQuestion 无活动问题时 honest failed 回执。capabilities 现含 mono.approvals.v1。

## harness 维护要点

- 端口随机化(4400+random 500),避免残留 host 进程占用端口后用错 store 应答(曾导致"Device credential is invalid or revoked"假象)
- pair 必须先于 serve 执行:两者打开 store 都跑迁移,并发首次迁移会撞 "duplicate column" —— monocode host 既有竞态,非本次引入
- mono.answerQuestion 语义:payload.requestId 必填,reply 省略=hold open(对应手机端 keepQuestionOpen)


## 客户端 UI 垂直切片(2026-11-26,已 UI 端到端验证)

**Machines 视图**(src/workspaces.rs + AppView::Machines):按用户认可的 mobile 模型实现——导航栏 Machines 入口 → 机器选择 → 该主机 workspace 的 Mono 卡片(名称/状态/颜色/最近消息)→ 详情页(只读历史 + composer)→ mono.send 命令闭环。全部数据经 workspace RPC,本地无调度副作用。

**真实 UI 端到端验证(双实例本机)**:owner(种子 mono uimono001)+ host(:4520)+ client(种子 remote-machines.json 直连)。
- catalog 同步:mono 卡片渲染,name/status/color/lastMessage 正确
- 详情页历史:空历史提示正确,发消息后回读渲染(经 owner store 往返)
- composer:Enter 发送 → 命令回执 completed → 输入框清空 → 历史刷新出气泡
- 断连:kill host → 红色 unreachable 提示,已同步内容保留(不静默清空/回落)
- 重连:host 恢复 → 提示消失,同一 mono 对象继续
- 验证 harness:/home/ubuntu/ws-ui-setup.mjs 搭建环境;截图证据在 session artifacts

**已知偏差与剩余(诚实清单)**:
1. 主机选择器在 Machines 视图内(机器 pill 行),不是设计文档字面要求的全局导航条 scoping——现有本地侧边栏(项目栏/会话栏)未被按 host 重 scope。这是最大的剩余工程:本地 sidebar 深耦合 i64 session id 与本地 store,需要继续批次推进
2. 远端 projects 只显示计数,未接入项目栏;mono 项目绑定在客户端只读
3. 轮询同步(3s),无推送通道;transcript 渲染是简版(messages 气泡),无 turns/工具卡片/审批交互 UI(后端 action 已支持 mono.answerQuestion/mono.cancel)
4. SSH 传输未端到端单测——验证用 loopback http 直连;workspace RPC 走既有 Connections::request(SSH tunnel 透明继承),但 install/pairing 流程未在本轮验证
5. 第 4-6 批(打磨/Rust owner 抽离/发布)未开始,属文档定义的后续阶段
6. monos.file(读写 SOUL/MEMORY)后端已通,客户端无 UI


## Review 修复轮(2026-11-26,commit 9edd71f1/61936a32/f1d821f0)

Devin Review 初审 6 项,5 项已修并回归(写路径 harness 22/22):
1. mono.send 回执在 outbox 落盘前标记 completed → 改为延迟回执:mobile_send 返回消息 id,轮询 persisted 集合,超时记 unknown 而非 completed
2. 客户端重发草稿用新 commandId 导致无法去重 → 草稿保留 commandId,传输失败重试复用;发送中禁止重复提交
3. Settings 新增机器后 Machines 视图不刷新 → open_machines 每次重读连接列表
4. host 侧 IPC 断线后 client 永久 broken 不再恢复 → 发现层淘汰 broken client 重建连接
5. host 停机时 IPC socket 持有导致进程不退 → server close 事件关闭 workspace 连接
6. 历史不可向前翻页 → escalated(简版 transcript 范围外,打磨批次决策)

测试代理 UI 端到端(录制证据在 PR #1 评论):catalog/详情/首轮发送/断连/重连/返回/本地隔离全部 PASS;重连后第二条消息 receipt completed 且确认入 outbox,但因测试环境无 provider 首条消息阻塞,transcript 按设计只渲染 store 行故不可见——环境限制非缺陷。遗留观察:outbox 排队消息不进 transcript(打磨批次可评估 monos.history 投影是否含 pending)。

## 原布局主机作用域切片(2026-11-26,编译验证)

本批把 host scope 从 Machines 独立页面向原 Monocode 布局推进了一步:

- 两种布局共用导航条在侧栏开关后加入主机选择器;本机与已配置 SSH 主机同菜单切换,并提供 Add SSH host / Manage connections 入口。
- shell 维护 `remote_workspaces.selected` 作为全局主机作用域;选择主机后启动/复用 workspace catalog 同步,poll 更新会同时 invalidate project rail 与 session sidebar。
- Classic project rail 在远端作用域下直接渲染 owner catalog 的 Mono 与 Project 行:Mono 行打开既有远程 workspace detail(命令式、无本地调度副作用);Project 行首次点击时才调用 `projects.open` 注册 host project,随后复用现有 `remote://environment/path` 项目执行链路。
- catalog 的项目身份以 `environmentId + path` 固定,不把用户使用的 SSH endpoint 当作项目身份。

验证:`cd crates/monocode-gpui && cargo check --tests` 通过。未完成的下一步仍是 Focused sidebar/title tab 深度接入:本地列表结构深耦合 i64 session id,需要独立的 remote row/view-model;当前切片先保证原 project rail 能看到并进入远端对象,不把远端对象塞入本地 Mono 调度。

## Host 入口低侵入调整(2026-11-26,编译验证)

根据用户反馈重新调整信息架构:Host 不再放在共享顶部主导航,避免被理解成替代原项目菜单。原 Focused 项目按钮及其下拉菜单(Switch worktree / Switch project / Project settings / Open folder on a machine / Settings)保持原位。

- Focused:Host 选择器放在底部项目切换图标条下方,作为一行低侵入环境控制。
- Classic:Host 选择器放在 project rail 底部、Settings 上方。
- Focused 底部项目图标条在 remote scope 下直接显示 owner catalog 的项目;点击图标时才通过 `projects.open` 注册并切换到对应 `remote://environment/path` 项目,不做多主机项目混排；本机 scope 只显示本机项目,已注册 remote:// 项目也只在其 Host scope 出现。
- 远端 scope 下禁用底部项目图标的本地拖拽排序和本地项目右键菜单,避免把远端 catalog 对象误当成本地 project 状态。

验证:`cd crates/monocode-gpui && cargo check --tests` 通过。

## 历史分页、Focused 接入与远程交互(进行中,本地未提交)

本批先把 PR Review 指出的历史不可达问题做成协议行为,而不是客户端 workaround:

- owner 的 `monos.history` 现在理解当前已加载窗口内的 `before=tN`:先在窗口内返回前一页;当请求已到窗口顶部且 store snapshot 带有 history cursor 时,跨入 store 的 older page,不再重复返回最新页。
- Machines 远程 Mono detail 改为保留完整 turns。tail refresh 按 turn id 原地更新/追加,不再覆盖用户已加载的旧页;`Load older messages` 用第一个已加载 turn id 请求上一页并按 anchor 插入、去重。
- Focused sidebar 在 remote host scope 下不再误列本地 session。它现在渲染 owner catalog 的远程 Mono rows,点击仍打开 workspace-command backed detail;远程 rows 不进入本地 Mono manager/scheduler。

Focused title tab 已有最小安全切片:

- `TitleTab::RemoteMono { machine_id, id }` 是独立持久化 identity,不复用本地 Mono identity,也不会进入本地 pane/split/scheduler。
- tab scope 对本机保持原 key,对 remote scope 追加 machine identity,避免不同主机或本机之间的 focus/restore 串扰。
- 从 host-scoped navigation 打开的远程 Mono 会得到自己的 Focused title tab;Machines catalog tab 与 detail tab 可以并存。直接在 Machines 管理面里打开的 detail 仍属于 Machines tab,不会隐式制造远程对象 tab。
- 远程 tab 可关闭、 pinning/menu/reorder 走通用 tab 机制;拖入本地 pane split 的语义明确不开放。
- 测试平台修正:原 3 个 sidebar/pane 测试失败不是远程改动引入,而是 Linux 下测试硬编码 `cmd`、实际绑定为 `ctrl`;已按平台修正。相关 title tab 测试通过,另有一个 layout 动画断言宽跑偶发、单跑通过,仍按 flake 观察。

审批/问题/取消协议缺口已补第一版:

- `monos.profile` 新增 `interaction` 投影,包含 `waiting`、结构化 `approvals[](requestId/title/detail/canRemember)` 和 live `question`(沿用 phone question prompt 的 requestId/options/multiSelect/allowCustom/autoResolveAt 形状)。compact transcript 不再是审批交互的信息来源。
- workspace commands 新增 `mono.decideApproval`,沿用本地 `decide_notification_request`,支持 `allow` / `session` / `deny`;`mono.answerQuestion` 和 `mono.cancel` 继续使用既有 owner 路径。
- Machines 远程 Mono detail 现在渲染审批卡片、问题选项/自定义答案/跳过/keep-open 控件和忙碌时的 Cancel;所有动作都通过 `workspace.commands.dispatch` 与 receipt 回写,不直接操作本地对象。
- approval/question command id 由精确业务 payload 派生,传输失败后的同答案重试可命中 owner receipt;cancel 每次点击是新命令,因为每次中断请求都是新业务意图。

验证:`cargo check --tests` 通过;`workspace_history` 2/2、`workspaces::tests` 3/3、remote title tab 定向测试 1/1 通过。

真实 SSH 验证已完成(2026-11-26):先 `npm ci` 安装锁定依赖,再 `npm run host:package` 生成 `monocode-host-linux-x64.tar.gz`;`scripts/test-remote-ssh.py --native --profile dev` 的仓库原版在 cargo test 阶段因硬编码 `--offline` 且本机缓存缺 `build_const` 失败,但失败前 native askpass 已通过。随后用同一脚本的内存替换版本仅去掉 `--offline` 重跑,结果通过:disposable loopback sshd、临时密钥/known_hosts、native askpass host fingerprint 确认、packaged host start/pair/token、真实 OpenSSH tunnel 与两次断连重连均 PASS,且脚本清理临时服务,不改个人 SSH 配置或 OS service。该 harness 验证的是 pairing/install/transport,不是 workspace RPC 本身的额外端到端用例。

最终局部复核:`git diff --check` 通过。`npm run check:web` 跑到 5572 pass / 1 fail / 13 skipped;唯一失败是既有 `agentAppMemory.test.ts` habit schedule 文案断言(期望 `Weekdays at 9:00 AM`,实际 `Weekdays at 9:00`),看起来是 locale/时间格式基线问题,与本次 Rust/GPUI 远程工作区改动无关,但因此不能把 web check 记为通过。

最新收尾:approval/question/cancel fake-host UI harness、多机器作用域、host 离线/恢复 poll 均已通过(`workspaces::tests` 6/6)。`npm run native:check` 的 node 脚本、fmt、clippy、core tests 全部通过;GPUI 串行测试 867 passed / 1 failed / 1 ignored,唯一失败是既有 `mermaid-rs-renderer` 依赖内部 geometry validation,与本批无关。剩余仅为 PR 更新/review thread 处理。

## 第二轮最终验证补充(本地未提交)

- 英文 locale 下 `LANG=en_US.UTF-8 npm run check:web` 全绿:513 files / 5573 tests passed,13 skipped。此前中文 locale 的 habit schedule 失败确认是 `toLocaleTimeString` 24 小时制环境差异,不是回归。
- 完整 `cargo test` 第二轮:857 passed / 8 failed / 1 ignored。快捷键相关失败已通过测试平台修正消除;剩余 8 个中 6 个单跑全部通过,属于并行/动画时序不稳定。两个单跑仍失败且与本批远程工作区文件无关:`editor_analysis` QuickJS deadlock、`markdown::mermaid` 依赖内部 geometry validation panic。
- 自审并修复远程命令竞态:send/approval/question/cancel 在飞行途中若用户切换 detail,回调不再写入新 detail。
- 测试平台修正扩大到 settings/pinned tabs/terminal panes 的 shell 快捷键模拟:Linux 使用 ctrl,macOS 使用 cmd;不改产品行为。
- clippy 仍在后台运行,尚无最终结论。所有代码仍未 commit/push。

## Review 修正:远程标签状态、会话身份与作用域边界(2026-11-26,编译验证)

- Remote Mono detail 现在按 `(machine, mono)` 暂存/恢复完整视图状态;切换 Machines catalog 或 remote Mono tab 不再丢弃 composer 草稿、pending command、问题表单和分页状态,显式关闭 tab 才丢弃。
- Owner history 响应携带 `conversationId`;客户端缓存按会话身份合并,Mono reset/新会话后会清空旧 transcript,跨页加载也会拒绝过期身份的回答。
- `monos.profile` 投影 live `busy`,remote Cancel 是否可见由 owner 实时状态决定,不再被历史页的静态 `busy:false` 覆盖。
- 项目前进/后退快捷键和底部 space 拖拽都改用当前 Host scope 的可见项目顺序;本地拖拽在保存时把可见顺序映回完整项目列表,隐藏的 `remote://` 项目不再造成索引偏移。
- Classic 的 Sessions 保持项目作用域: active project 属于所选 host 时使用该项目同步来的 session rows;否则提示先选择该 host 的项目。Focused 仍显示 host-wide owner Monos。

## Review 修正第二轮:请求实例与取消语义(2026-11-26,编译验证)

- Remote detail 的 send/action/older-page 回调携带 detail instance identity;请求在 tab 切换期间完成时会写回匹配的 active/parked detail,显式关闭后不会污染新实例。
- `monos.profile` 区分 aggregate `busy` 与 Mono 自身对话的 `canCancel`; delegated/launch session 忙碌仍显示 Working,但只有自己对话可中断时才显示 Cancel。
- Classic 本地 scope 的项目前进/后退恢复使用 rail 的 `visible_project_order`; remote/Focused scope 继续使用底部项目条顺序。
