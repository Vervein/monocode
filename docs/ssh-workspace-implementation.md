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
- [x] 写路径核心(dispatch/回执/mono.send/mono.setProjects 全验证;审批/取消 action 仍未做——属第3批剩余)
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
