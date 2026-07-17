# browserkit

面向 AI agent 的持久浏览器运行时,**构建在 cdpkit 之上**。`bk` 是默认 CLI client,后台 daemon/runtime 才是核心边界。

## 架构

```
bk CLI / TCP client  --(newline-JSON over TCP)-->  browserkit daemon/runtime  --(cdpkit typed CDP)-->  Chrome
```

daemon 常驻后台,维持持久 CDP 连接,状态持久化到 `~/.bk/`,重启时恢复(重连浏览器、为每个 tab 重新 attach session)。

## 源码布局（`src/`）

- `main.rs` CLI 入口(clap)、`client.rs` TCP 客户端 + daemon 自启、`config.rs`(`~/.bk/config.toml`)、`error.rs`(统一 `BkError`)。
- `browser/` — Chrome 发现 `finder.rs`、进程管理 `launcher.rs`、CDP 连接 `mod.rs`。
- `daemon/` — 生命周期 `mod.rs`、状态 `state.rs`(`Arc<DaemonState>` + DashMap)、TCP server `server.rs`、防抖持久化 `persist.rs`、协议 `protocol.rs`、`handler/`(每命令组一文件:session/open/attach/tabs/snapshot/act/navigate/wait/evaluate/inspect/storage/dialog/browser/network/debug/daemon/common)。
- `page/` — navigation / interaction / capture / state。

## 关键设计（改之前先尊重）

- **Session** = 唯一浏览器活动与持久化边界。默认 session 复用用户 Chrome 登录态;命名 session 用 BrowserContext 隔离 cookie/storage/tab。
- **Tab ownership** = `Owned` tab 由 browserkit 创建,close 会关闭 Chrome target;`Attached` tab 来自用户现有浏览器,close 只 detach,不能关闭用户 target。
- **持久化**:`~/.bk/state.json` 为 schema v3 session-only 状态文件。v2 state 会先备份为 `state.v2.backup*.json` 再迁移到 v3;运行时不再写 workspace 字段。损坏或未来版本会禁用写入并在 `daemon.status.persistence` 暴露原因。`daemon.port` 记录 daemon 端口。写入原子(tmp+rename)且防抖(500ms)。别在请求处理里同步阻塞写盘。
- **破坏性迁移**:`ws`/`tab`/`fetch` CLI、`BK_WS`/`--ws`、`ws.*`/`tab.*`/`nav.*`/`page.*`/旧 `storage.*`/`v2.*` route 已移除;文档和 skill 只能推荐当前 canonical commands。
- **并发**:DashMap / parking_lot;注意别持锁跨 await。
- daemon 按需自启;端口存 `~/.bk/daemon.port`。
- v2 输出格式固定 JSON;旧 `--format`/text/tsv 口径只属于历史 v1。

## 对底层 cdpkit 的依赖（重要）

- `Cargo.toml` 当前通过 `../cdpkit-rs/cdpkit` path 依赖联合开发中的 cdpkit 0.4.0;发布 browserkit 前必须改为对应的 crates.io 版本并重新验证锁文件。底层 API 变更会立即传导到本地构建。
- 0.4.0 使用 unified Sender trait API:浏览器级命令 `cmd.send(&cdp)`;页面级命令 `let session = cdp.session(session_id); cmd.send(&session)`;`tokio::spawn` 场景用 `cdp.owned_session(id)` 得到 `OwnedSession`(Send+'static)。
- 事件订阅:`SomeEvent::subscribe(&session)`(或 `&cdp`),返回 `EventStream<T>`;订阅要在触发动作之前。
- 自定义 Method 结构体(不在 protocol.rs 生成的)用 `sender.send_cmd(my_method).await?` 发送,需 `use cdpkit::Sender;`。
- 需要 cdpkit 尚未提供的能力时:**不在本仓库实现底层**。记录需要什么 CDP API,交给用户去 cdpkit-rs 那套 agent 实现;待底层就绪(发新版,或临时 path 依赖 `../cdpkit-rs/cdpkit`)再接入。
- **归属判断 + 不做强行兼容**:若 bug 根因或某能力其实属于 cdpkit(库层),**明确指出"该在 cdpkit-rs 改"并告诉用户**,不要在 browserkit 里做 workaround / 绕过 / 上层硬凑来掩盖——两个项目都是我们维护的,去对的那一层修,而不是堆补丁。
- 调 cdpkit 守约定:浏览器命令 `&cdp`、页面命令 `&session`;订阅前先 enable、先 subscribe 再触发;attach 用 `with_flatten(true)`。

## 构建与验证

```
cargo build        # 产出 bk
cargo test
```

涉及 CLI 行为的改动,尽量实际跑相关 `bk` 命令验证。新增网络监听/端口暴露时,主动说明是否引入未鉴权入口。

## Agent 团队

本项目一套项目内 agent 协作,均**只服务于 browserkit**:

- **`browserkit`**(主,纯调度)— 判断需求归属并派发,自己不写代码。
- **`bk-feature`** — 实现功能/改 bug(写代码)。
- **`bk-debug`** — 复现并定位 bug 根因(只读+可运行,不做最终修复)。
- **`bk-test`** — 写测试、跑 build/test、实跑 bk 命令验证。
- **`bk-review`** — 只读审查 diff(正确性、并发、持久化、CLI 体验、安全)。

典型链路:新功能 feature → test → review;bug 则 debug 定位 → feature 修 → test 回归。纯审查/纯测试需求可直接派对应 agent。

## 记忆系统

持久记忆在 **`.Codex/memory/`**,索引为 `.Codex/memory/MEMORY.md`。本项目所有 agent(及主对话)遵守:

- **开工前**:读 `.Codex/memory/MEMORY.md`,按需读相关条目。
- **收尾时**:把**非显而易见**的持久信息写成条目并在 MEMORY.md 加一行索引。值得记的:用户偏好与反馈、跨会话仍有效的决策、踩过的坑/约束、外部资源指针。
- **不要记**:能从代码/git 看出来的、仅本次任务的临时状态。
- 记忆可能过时:据此行动前先核对当前代码,冲突时以现状为准并更新记忆。

条目文件用 frontmatter(`name` / `description` / `metadata.type`,type ∈ user/feedback/project/reference),feedback/project 类型正文写明 **Why** 与 **How to apply**。
