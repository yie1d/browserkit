# browserkit v2 重设计方案

> 面向 AI agent 的持久浏览器运行时。`bk` 是默认 CLI client，daemon/runtime 才是核心边界。
>
> 当前状态：workspace runtime 已完成破坏性迁移。当前 CLI contract 以 `bk --help`、`bk session --help`、`bk debug --help` 和 `docs/bk-browser/references/commands.md` 为准；本文后半部分的实施计划保留为历史设计记录。

## 1. 设计哲学

### 1.1 Agent-first runtime 意味着什么

browserkit v2 的唯一用户是 LLM agent（Claude、GPT、Gemini 等）。这不是"对人类也友好但顺便支持 agent"，而是每一个设计决策都以 agent 的认知模型为出发点：

- **无歧义**：命令命名不能有认知陷阱。goto vs open 这种让 agent 困惑的二义性必须消除。
- **最少步骤**：agent 的每一次工具调用都消耗 token 和延迟。能一步完成的事绝不拆成两步。
- **自描述返回**：每个操作的返回值必须让 agent 知道"发生了什么"，不需要额外调用来确认效果。
- **无隐式状态要求**：agent 不需要记住"当前 session 是哪个"、"刚才连了哪个浏览器"。
- **机器优先格式**：永远 JSON，不需要解析人类友好的表格或缩进文本。

### 1.2 两个核心原语：observe 和 act

浏览器自动化的本质就是两件事：

1. **Observe（观察）** -- `snapshot` 命令：获取页面当前状态，包含可交互元素列表和页面文本内容
2. **Act（行动）** -- `act` 命令：对页面执行一个交互操作

这是 agent 操作浏览器的完整认知模型。不需要理解 BrowserContext、session、target、CDP 这些底层概念。

### 1.3 透明化原则

以下概念在 v2 中对 agent 完全透明（不暴露或最小暴露）：

| 概念 | v1 行为 | v2 行为 |
|------|---------|---------|
| 连接 Chrome | 手动 `browser discover` / `browser connect` | 显式 `bk connect`，幂等 |
| Workspace | 手动创建、切换、指定 `--ws` | 删除，由 session 替代 |
| Session | 不存在 | 自动管理（default）或按名称隔离（`--session`） |
| Tab target | 需要理解 session 和 target 的关系 | 用 `--target` 指定 tab（可选） |
| Daemon | 需要知道 daemon 存在 | 完全透明，按需自启 |

**Session 透明化原则**：不带 `--session` 时，agent 完全不需要理解 session 概念——操作在 default session 中进行，共享用户 Chrome 的登录态。只有多 agent 并行需要隔离时，才显式传 `--session <name>`。

### 1.4 与竞品的差异定位

| 维度 | Playwright MCP | browser-use | browserkit v2 |
|------|---------------|-------------|---------------|
| 接入方式 | MCP Server（需 MCP 客户端） | Python SDK（需 LangChain） | 本地 runtime + `bk` CLI/TCP client |
| 浏览器所有权 | 自己启动的浏览器 | 自己启动的浏览器 | 接管用户已打开的 Chrome |
| 页面表示 | Accessibility Tree（ARIA 树） | DOM + Screenshot | Interactive Elements + Page Text |
| 元素寻址 | `ref=eN`（AX 树节点） | CSS selector / 坐标 | `ref=N`（backendNodeId，纯整数，DOM 稳定） |
| 状态持久性 | 无（每次连接重建） | 无 | daemon 持久化（跨会话保持） |
| 多 tab | 支持 | 有限 | 原生支持，`--target` 切换 |
| 多 agent 隔离 | 无原生支持 | 无 | BrowserContext 级隔离（`--session`） |
| 零配置 | 需 MCP 配置 | 需 Python 环境 | 单二进制，零配置 |
| token 效率 | 中（ARIA 树较冗长） | 低（DOM 很大） | 高（只返回可交互元素 + 精简文本） |

**browserkit v2 的独特价值**：

1. **接管真实浏览器**：操作用户已登录的 Chrome，不用处理登录/cookie 导入
2. **持久浏览器运行时**：daemon 维持浏览器连接、tab、session 和持久化状态，跨 agent 会话保持
3. **薄 CLI 入口**：`bk` 是默认 client，不需要 MCP 协议栈，任何能调 Bash 的 agent 直接用，但核心边界仍是 daemon/runtime
4. **极致 token 效率**：只返回 agent 需要的信息，不返回完整 DOM 或 ARIA 树
5. **多 agent 安全并行**：BrowserContext 级隔离，无竞态

### 1.5 分层边界

- **cdpkit-rs**：protocol layer，只负责 typed CDP commands / events / sessions / sender。
- **browserkit**：runtime layer，负责 daemon 生命周期、连接真实 Chrome、session/tab 管理、持久化、snapshot 和 act。
- **Agent**：decision layer，根据观察结果决定下一步动作。

需要新增底层 CDP 能力时，应先在 cdpkit-rs 实现，再由 browserkit 组合成 agent-friendly runtime 能力。

## 2. Session 设计

### 2.1 核心概念

Session 是 v2 中 agent 与浏览器交互的隔离单元。它取代了 v1 中的 workspace 概念，但更简单、更自动化。

**两种 session 模式：**

| 模式 | 触发方式 | 底层实现 | 适用场景 |
|------|----------|----------|----------|
| Default session | 不带 `--session` 参数 | 共享用户 Chrome 的默认 BrowserContext | 单 agent 操作用户已登录的网站 |
| Isolated session | 带 `--session <name>` | CDP `Target.createBrowserContext` 创建独立 BrowserContext | 多 agent 并行，需要 cookie/storage 隔离 |

> ⚠️ **多 Agent 安全警告**
>
> default session 不是多 agent 安全的。如果两个 agent 同时使用 default session（均不带 `--session` 参数），它们共享同一个 active tab 状态，可能互相干扰。
>
> **规则：多 agent 并行场景必须使用 `--session` 参数，每个 agent 使用唯一的 session 名称。**
>
> default session 的设计用途：单个 agent 操作用户已登录的浏览器。

### 2.2 设计理由

**为什么 isolated session 用 BrowserContext 隔离：**

- 多 agent 并行时，BrowserContext 天然隔离 cookie/storage/cache
- BrowserContext 在 Chrome 层隔离其中创建的 tab；browserkit 对 `bk open` 和 click 产生的新 target 做 session-native 生命周期追踪
- Default session 只用于单 agent 操作用户已登录网站的场景，不需要隔离

**为什么不沿用 workspace：**

- Workspace 对 agent 是不必要的认知负担（16 位 hex ID、手动创建/切换/删除）
- Session 的语义更贴近 agent 的工作模型：开始一个任务 -> 操作 -> 结束任务
- Session 可以自动创建和自动回收，agent 不需要管理生命周期

### 2.3 Tab 归属规则

**Isolated session（`--session <name>`）：**

- `bk open` 创建的 tab 会登记到 session，并参与 active tab 管理
- Chrome 内核保证新 tab 在创建它的 BrowserContext 中打开；click 触发的 target 也会由 watcher 登记到同一 session
- `act click` 在 `target=_blank` 等场景返回 `new_tab` target ID，并把新 target 设为 active

**Default session（不带 `--session`）：**

- Agent 用 `bk open` 创建的 tab 归本 session
- 未 attach 的用户 tab 不进入 session，也不会出现在 `bk tabs`；`bk attach` 可显式接入，随后 `bk close` 只 detach 而不关闭用户 target
- 设计理由：防止 agent 误操作用户正在浏览的页面

### 2.4 Session Active Tab

每个 session 维护一个当前活跃 tab（session-level active tab），用于命令不带 `--target` 时的默认操作对象。

**Active tab 切换规则：**

| 事件 | Active tab 变化 |
|------|-----------------|
| `bk open` 创建新 tab | 新 tab 成为 active tab |
| 当前 active tab 被关闭 | 回退到上一个 tab |
| `--target` 显式指定操作其他 tab | active tab **不变** |

click 触发可跟踪的新 target 时，session-native target watcher 会登记该 tab、将其设为 active，并让 `act click` 返回 `new_tab` target ID。

**与 Chrome 焦点 tab 的区别**：session active tab 是 bk 内部的逻辑概念，不等于 Chrome UI 中用户看到的前台 tab。这允许 agent 在后台操作 tab 而不干扰用户。

### 2.5 Focus 规则

**所有操作一律不自动 focus，永远后台静默操作。** 这保证 agent 在任何情况下都不会抢占用户正在看的页面。

如果 agent 需要将某个 tab 带到前台（例如需要用户看到操作结果），显式加 `--focus` flag：

```bash
bk open <url> --focus
bk snapshot --target X --focus
```

`--focus` 是全局可选参数（默认 false），可用于任何接受 `--target` 的命令。

### 2.6 用户自己的 Tab

Agent 对用户 tab 的默认可见性和操作权限：

- **不可见**：`bk tabs` 不返回用户自己的 tab
- **不可操作**：agent 无法对用户 tab 执行 snapshot/act/navigate
- **继承登录态**：agent 用 `bk open` 开新 tab 时，自动继承用户 cookie（因为 default session 共享默认 BrowserContext）
- **显式接管**：`bk attach <pattern>` 或 `bk --target <targetId> attach` 可以把用户现有 tab 登记到 default session；隔离 session 使用 `bk open`

**为什么这样设计：**

- 防止 agent 误操作用户正在看的页面（关闭用户 tab、在用户正看的表单里填内容等）
- Agent 需要访问用户已登录的网站时，用 `bk open` 开新 tab 即可继承 cookie

显式 attach 的 tab 归属为 `Attached`。后续 `bk close` 或 session cleanup 只 detach browserkit 的 CDP session，不关闭用户 Chrome target。

### 2.7 Session 生命周期

| 事件 | 行为 |
|------|------|
| `bk connect --session x` | 自动创建 isolated session（创建 BrowserContext） |
| `bk connect`（不带 `--session`） | 自动使用 default session（共享默认 BrowserContext） |
| 72h 无操作 | 自动 close（关闭所有 tab + 销毁 BrowserContext） |
| `bk session close --session <name>` | 手动 close isolated session |
| `bk session close`（无参数） | 关闭 default session 的所有 agent tab |
| Chrome 退出 | 所有 session 自动失效 |

**Headless 独立 Chrome 进程**：是否 headless 由 `~/.bk/config.toml` 中的 daemon 配置控制；这不改变 session 边界。

### 2.8 Session 持久化

- Isolated session 的 BrowserContext ID 和关联 tab 持久化到 `~/.bk/state.json`
- Daemon 重启时恢复 session 状态（重连 Chrome、重新 attach tab）
- 72h 超时计时从最后一次操作开始，daemon 重启不重置计时器

### 2.9 在 isolated session 中注入登录态

isolated session 创建时是空白 BrowserContext，没有 cookie。如果 agent 已有目标网站的登录 cookie（例如从环境变量、密钥管理系统获取），可以通过 `bk session cookies set` 注入：

```bash
bk connect --session a
bk session cookies set --file cookies.json --session a
bk open https://app.example.com --session a
# 页面打开后已是登录状态
```

这解决了 isolated session 最常见的问题：需要登录但又不想自动化登录流程（如复杂的 2FA）。

### 2.10 Session 资源限制

每个 isolated session 在用户 Chrome 内创建独立 BrowserContext，消耗 Chrome 的内存资源（每个 tab 约 80-150MB renderer 进程内存）。不加限制时，100 个 session × 4 个 tab = 约 32GB Chrome 内存占用。

**默认资源配额**（可通过 `~/.bk/config.toml` 修改）：

```toml
[limits]
max_sessions = 10           # 最多同时存在的 session 数（default session 不计入）
max_tabs_per_session = 5    # 每个 session 最多 tab 数
session_timeout_hours = 72  # 无操作自动断开（不销毁 tab）
```

**超出配额的行为**：
- 超出 `max_sessions`：新建 session 返回 `SESSION_LIMIT_EXCEEDED` 错误，建议关闭旧 session
- 超出 `max_tabs_per_session`：新建 tab 返回 `TAB_LIMIT_EXCEEDED` 错误

## 3. 命令结构重设计

### 3.1 新命令总览

v2 将 35+ 命令精简为核心命令集：

**主要原语（agent 日常使用 90% 的场景）：**

| 命令 | 用途 | 频率 |
|------|------|------|
| `setup` | 交互式引导用户完成 Chrome 远程调试配置，检测完成后自动验证连接 | 一次性 |
| `connect` | 建立浏览器连接（幂等，推荐但非必须的前置步骤） | 低 |
| `snapshot` | 获取页面状态（元素 + 文本 + 视口信息） | 高 |
| `act` | 执行交互操作（click/type/press/scroll/select/...） | 高 |
| `navigate` | 导航到 URL | 中 |
| `wait` | 等待条件满足 | 中 |
| `evaluate` | 执行 JavaScript，可由 CLI 追加字符串结果到本地文件 | 低 |
| `network watch` | 有界观察 XHR/fetch 响应元数据（不读取 body） | 低 |
| `download` | 点击 ref 并跟踪下载终态 | 低 |
| `screenshot` | 截图（返回 base64 或保存文件） | 低 |

**辅助命令（多 tab / session 管理）：**

| 命令 | 用途 |
|------|------|
| `tabs` | 列出当前 session 的 tab |
| `open` | 新开 tab 并导航到 URL |
| `close` | 关闭指定 tab |
| `session close` | 关闭 session（释放所有 tab + BrowserContext） |
| `session list` | 列出所有活跃 session（调试用） |
| `session cookies set` | 批量注入 cookie（支持 httpOnly） |
| `session cookies get` | 获取当前 session 的 cookie |
| `session cookies clear` | 清除当前 session 的 cookie |
| `status` | 查看连接状态（调试用） |

**内部命令（保留但不推广，agent 一般不用）：**

| 命令 | 用途 |
|------|------|
| `browser connect` | 手动连接浏览器 |
| `browser disconnect` | 断开浏览器 |
| `browser list` | 列出已连接浏览器 |
| `daemon start` | 启动 daemon |
| `daemon stop` | 停止 daemon |
| `daemon status` | daemon 状态 |

### 3.2 命令对比：旧 -> 新

| 旧命令 | 新命令 | 变化说明 |
|--------|--------|----------|
| `goto <url>` | `navigate <url>` | 重命名，消除 goto/open 歧义 |
| `info` | `snapshot` | 重命名，强调"观察"语义 |
| `click --index N` | `act click --ref 42` | 合并进 act，默认用 ref |
| `type --index N "text"` | `act type --ref 42 --text "text"` | 合并进 act，默认 clear |
| `fill --set ...` | `act fill --set ...` | 合并进 act |
| `select --index N "val"` | `act select --ref 42 --value "val"` | 合并进 act |
| `scroll down` | `act scroll --direction down` | 合并进 act |
| `hover --index N` | `act hover --ref 42` | 合并进 act |
| `keys Enter` | `act press --keys Enter` | 合并进 act，重命名为 press |
| `options --index N` | `act options --ref 42` | 合并进 act |
| `drag ...` | `act drag --from-ref 42 --to-ref 55` | 合并进 act；源和目标也可分别使用 selector |
| `upload ...` | `act upload --ref 42 C:\absolute\file.txt` | 合并进 act；文件路径改为 positional absolute paths |
| `focus --index N` | `act focus --ref 42` | 合并进 act |
| `eval "expr"` | `evaluate "expr"` | 重命名，不缩写 |
| `shot` | `screenshot` | 重命名，不缩写 |
| `open <url>` | `open <url>` | 保留，语义变为"新 tab + 导航"（不自动 focus） |
| `back` | `navigate --back` | 合并进 navigate |
| `forward` | `navigate --forward` | 合并进 navigate |
| `reload` | `navigate --reload` | 合并进 navigate |
| `wait ...` | `wait ...` | 保留，参数不变 |
| `tab list` | `tabs` | 提升为顶级命令 |
| `tab close <tid>` | `close --target <tid>` | 提升为顶级命令 |
| `find "selector"` | `find "selector"` | 保留为 session-native CSS 查询 |
| `search "text"` | `search "text"` | 保留为 session-native 文本/regex 搜索 |
| `html` | `html` | 保留为 session-native HTML 获取 |
| `url` | 删除 | snapshot 返回中包含 |
| `title` | 删除 | snapshot 返回中包含 |
| `console` | `console` | 保留为 session-native console buffer |
| `pdf` | `pdf` | 保留为 session-native PDF 导出 |
| `fetch` | 删除 | navigate + evaluate 替代 |
| `ws *` | 删除 | session 替代，workspace 不暴露给 agent |
| `storage *` | `session storage *` / `session cookies *` | 存储能力移动到 session 边界 |
| `dialog *` | `dialog *` | 保留为 session-native dialog 管理 |
| `debug monitor/har/events` | 删除 | 非真实 streaming 能力 |
| `debug block/unblock/cdp` | 保留 | developer escape hatch |
| `new / ls / rm` | 删除 | workspace 别名，不再需要 |

### 3.3 各命令完整参数定义

#### `setup` -- 交互式引导 Chrome 远程调试配置

```
bk setup
```

**功能**：交互式引导用户完成 Chrome 远程调试配置，检测完成后自动验证连接。

**行为流程**：

```
$ bk setup

Checking Chrome... ✓ Chrome 136 found at C:\Program Files\Google\Chrome\...
Checking remote debugging... ✗ Not enabled

Remote debugging lets bk connect to your Chrome browser.
You only need to do this once — the setting persists across restarts.

Steps:
  1. Open Chrome (if not already open)
  2. In the address bar, type: chrome://inspect/#remote-debugging
  3. Check the box: "Discover network targets" (or "Enable remote debugging")
  4. Come back here and press Enter

Waiting... [Press Enter when done]

Checking connection... ✓ Connected to Chrome 136!
Remote debugging is now enabled.

You're all set. Run 'bk connect' to start using bk.
```

**Edge cases**：
- Chrome 未安装 → 提示安装，检测 Edge → 如果有 Edge 给出 Edge 版本的引导
- Chrome 版本过低 → 提示升级
- 用户完成步骤后连接失败 → 重试检测，给出具体错误信息
- 已经配置好 → 直接返回成功状态，不重复引导

**返回值**（JSON）：

```json
{
  "ok": true,
  "data": {
    "status": "ready",
    "browser": "Chrome 136",
    "message": "Remote debugging enabled. Run 'bk connect' to start."
  }
}
```

#### `connect` -- 建立浏览器连接

```
bk connect [--session <name>] [--timeout <ms>]
```

| 参数 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `--session` | string | default session | 指定 session |
| `--timeout` | u64 | 30000 | 超时毫秒数 |

幂等：已连接时直接返回状态（status: "already_connected"），不重连。详见 [6. 连接设计](#6-连接设计)。

#### `snapshot` -- 获取页面状态

```
bk snapshot [--session <name>] [--target <targetId>] [--full]
            [--max-tokens <16..100000>] [--timeout <ms>]
```

| 参数 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `--session` | string | default session | 指定 session |
| `--target` | string | session active tab | 指定操作的 tab（targetId） |
| `--full` | flag | false | 完整模式（更长文本截断阈值、所有属性） |
| `--no-page-text` | flag | false | 不返回 page_text 字段，只返回 elements（节省约 500 token）。适用于纯交互任务（填表、点击）不需要阅读页面内容的场景 |
| `--wait` | string | dom-stable | 等待策略：`dom-stable`（默认）\| `networkidle` \| `none` |
| `--max-tokens` | usize | - | 对 elements + page_text 应用确定性内容预算 |
| `--timeout` | u64 | 30000 | 超时毫秒数 |

自动行为：
- 若 session 不存在且自动 connect 失败，返回对应连接错误（如 `BROWSER_NOT_RUNNING`）
- 等待策略：
  - 默认（`dom-stable`）：等待 `DOMContentLoaded` + 200ms DOM 稳定检测（连续 200ms 内无 DOM 变化）
  - `--wait networkidle`：等待网络空闲（500ms 无新请求），适用于已知会触发 AJAX 的页面
  - `--wait none`：跳过等待，立即采集当前 DOM 状态

#### `act` -- 执行交互操作

```
bk act [OPTIONS] [KIND] [FILES]...
```

**公共参数：**

| 参数 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `--session` | string | default session | 指定 session |
| `--target` | string | session active tab | 指定操作的 tab |
| `--timeout` | u64 | 30000 | 超时毫秒数 |
| `--no-state-diff` | flag | false | 不附带操作后的状态变化（节省 token） |
| `--focus` | flag | false | 将目标 tab 带到前台 |

**kind = click**

```
bk act click --ref <N> [--session <name>] [--target <targetId>]
bk act click --x <f64> --y <f64>
```

| 参数 | 类型 | 说明 |
|------|------|------|
| `--ref` | i64 | 元素引用（来自 snapshot 的 ref 字段） |
| `--x, --y` | f64 | 坐标点击（备用方案，snapshot 中无 ref 时使用） |

当前行为：click 返回 action result 和 `state_diff`；如果点击打开了新 target，session-native target lifecycle tracking 会登记该 target 并在响应中报告 `new_tab`。

**kind = type**

```
bk act type --ref <N> --text <TEXT> [--append] [--session <name>]
```

| 参数 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `--ref` | i64 | 必填 | 目标输入元素 |
| `--text` | string | 必填 | 要输入的文本 |
| `--append` | flag | false | 追加模式（默认是替换已有内容） |

设计决策：v2 中 type 默认行为是**替换**（clear + type），与 v1 相反。理由：agent 绝大多数场景是把这个字段设为某个值而不是在已有内容后追加。需要追加时显式用 `--append`。
**kind = press**

```
bk act press --keys <KEYS...> [--session <name>]
```

| 参数 | 类型 | 说明 |
|------|------|------|
| `--keys` | string[] | 按键名列表，支持组合键如 `Control+a`、`Enter` |

支持的键名：Enter, Tab, Escape, Backspace, Delete, ArrowUp/Down/Left/Right, Home, End, PageUp, PageDown, Space, F1-F12, 单字符(a-z, 0-9)。修饰键：Control, Shift, Alt, Meta。

**kind = scroll**

```
bk act scroll --direction <DIR> [--amount <PX>] [--session <name>]
bk act scroll --ref <N>
bk act scroll --selector <CSS>
```

| 参数 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `--direction` | string | "down" | up/down/left/right/top/bottom |
| `--amount` | f64 | 500 | 滚动像素数 |
| `--ref` | i64 | - | 滚动到指定元素使其可见 |
| `--selector` | string | - | 滚动到 CSS selector 匹配的元素 |

**kind = select**

```
bk act select --ref <N> --value <VALUE> [--session <name>]
```

| 参数 | 类型 | 说明 |
|------|------|------|
| `--ref` | i64 | 目标 select 元素 |
| `--value` | string | 选项的 value 属性或显示文本 |

**kind = fill**

```
bk act fill --set ref:42="value1" --set ref:55="value2" [--session <name>]
```

| 参数 | 类型 | 说明 |
|------|------|------|
| `--set` | string[] | `ref:<N>=<value>` 格式，可重复 |

支持 input/textarea/select/checkbox/radio/contenteditable。每个字段独立报告成功/失败。

**kind = options**

```
bk act options --ref <N> [--session <name>]
```

| 参数 | 类型 | 说明 |
|------|------|------|
| `--ref` | i64 | 目标 select 元素；返回其 option 列表 |

**kind = hover**

```
bk act hover --ref <N> [--session <name>]
```

**kind = focus**

```
bk act focus --ref <N> [--session <name>]
```

**kind = drag**

```
bk act drag --from-ref <N> --to-ref <N> [--session <name>]
bk act drag --from-selector <CSS> --to-selector <CSS>
bk act drag --from-ref <N> --to-selector <CSS>
bk act drag --from-selector <CSS> --to-ref <N>
```

| 参数 | 类型 | 说明 |
|------|------|------|
| `--from-ref` | i64 | 拖拽源元素 ref；与 `--from-selector` 二选一 |
| `--from-selector` | string | 拖拽源元素 CSS selector；与 `--from-ref` 二选一 |
| `--to-ref` | i64 | 拖放目标元素 ref；与 `--to-selector` 二选一 |
| `--to-selector` | string | 拖放目标元素 CSS selector；与 `--to-ref` 二选一 |

**kind = upload**

```
bk act upload --ref <N> <file_path...> [--session <name>]
bk act upload --selector <CSS> <file_path...> [--session <name>]
```

| 参数 | 类型 | 说明 |
|------|------|------|
| `--ref` | i64 | file input 元素的 ref；与 `--selector` 二选一 |
| `--selector` | string | file input 元素的 CSS selector；与 `--ref` 二选一 |
| `file_path` | string[] | positional absolute paths，至少一个 |

`dialog` 不是 `bk act` kind。独立的 `bk dialog` 是当前 session-native dialog 管理命令，不应写成 act 的 dialog 子命令。

#### `navigate` -- 导航

```
bk navigate <url> [--session <name>] [--target <targetId>] [--timeout <ms>]
bk navigate --back [--session <name>] [--target <targetId>]
bk navigate --forward [--session <name>] [--target <targetId>]
bk navigate --reload [--session <name>] [--target <targetId>]
```

| 参数 | 类型 | 说明 |
|------|------|------|
| `url` | string | 目标 URL |
| `--back` | flag | 后退一页 |
| `--forward` | flag | 前进一页 |
| `--reload` | flag | 刷新当前页 |
| `--session` | string | 指定 session |
| `--target` | string | 指定 tab |
| `--timeout` | u64 | 超时（默认 30000ms） |

行为明确：
- 在当前 session active tab 里导航（不开新 tab）
- Agent 应该只在自己创建的 tab 上用 navigate
- 不应该用 navigate 操作用户正在看的页面（default session 中用户 tab 对 agent 不可见，已由设计保证）
- 未连接时自动尝试 connect，若失败返回对应连接错误
- 导航完成的判定：
  - 传统导航（整页刷新）：等待 `load` 事件
  - SPA 路由跳转（pushState/replaceState）：等待 URL 变化 + 200ms DOM 稳定检测
  - navigate 内部会根据导航类型自动选择等待策略，agent 无需区分

#### `wait` -- 等待条件

```
bk wait [--selector <css>] [--text <string>] [--text-gone <string>]
         [--url <pattern>] [--idle] [--fn <js_expr>] [--time <ms>]
         [--session <name>] [--target <targetId>] [--timeout <ms>]
```

| 参数 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `--selector` | string | - | 等待 CSS 选择器匹配的元素可见 |
| `--text` | string | - | 等待文本出现在页面中 |
| `--text-gone` | string | - | 等待文本从页面消失 |
| `--url` | string | - | 等待 URL 匹配（子串或 glob） |
| `--idle` | flag | false | 等待网络空闲（500ms 无请求） |
| `--fn` | string | - | 等待 JS 表达式返回 truthy |
| `--time` | u64 | - | 固定等待毫秒数 |
| `--timeout` | u64 | 30000 | 整体超时 |
| `--session` | string | default session | 指定 session |
| `--target` | string | session active tab | 指定 tab |

无参数时默认等待 networkidle。

#### `evaluate` -- 执行 JavaScript

```
bk evaluate "<expression>" [--append-to <file>] [--session <name>] [--target <targetId>] [--timeout <ms>]
bk evaluate --file <path> [--append-to <file>] [--session <name>] [--target <targetId>]
```

| 参数 | 类型 | 说明 |
|------|------|------|
| `expression` | string | JS 表达式（默认 async，可 await） |
| `--file` | string | 从文件读取 JS 代码 |
| `--append-to` | string | CLI 将字符串结果的原始 UTF-8 字节追加到本地文件，不自动补换行 |
| `--session` | string | 指定 session |
| `--target` | string | 指定 tab |
| `--timeout` | u64 | 超时（默认 30000ms） |

daemon 始终返回结构化 `data.result`。`--append-to` 不进入 daemon request；CLI 只接受 string result，成功时只输出 file/bytes summary，非字符串、目录、符号链接或 I/O 失败返回结构化错误。

#### `network watch` -- 有界观察 XHR/fetch 响应

```
bk network watch --pattern <url-substring> [--count <1..100>]
                 [--session <name>] [--target <targetId>] [--timeout <ms>]
```

命令先以容量 256、overflow=`close_stream` 的显式 bounded policy 订阅 `responseReceived` / `loadingFinished` / `loadingFailed`，再 enable Network 域，只收集 URL 包含 pattern 的 XHR/fetch。响应严格为 metadata-only：保留 status、headers、MIME type、encoded size 与失败信息，但 `body=null`、`body_omitted=true`、`body_omission_reason="metadata_only"`，禁止调用 `Network.getResponseBody`。乱序先到的 finished/failed 使用容量 256 的有界暂存；事件流 overflow/close/error 和 terminal 暂存丢弃通过 `stop_reason`、`event_streams`、`terminal_buffer` 明确报告。达到 count 或 timeout 后返回一个 JSON response；它不是 streaming route，也不会恢复已删除的 debug monitor/har/events。

#### `download` -- 下载生命周期

```
bk download --ref <N> --output-dir <existing-dir>
            [--session <name>] [--target <targetId>] [--timeout <ms>]
```

CLI 将目录 canonicalize 为绝对路径。daemon 在订阅 `Browser.downloadWillBegin` / `Browser.downloadProgress` 后调用现有 act click，按主 frame 和 GUID 关联事件，返回 completed 或结构化 canceled/timeout 错误。最终路径必须仍位于输出目录内；超时会尝试 cancel，所有已配置出口都会恢复 Browser download behavior。同一 daemon 内下载生命周期串行执行，避免共享 Browser behavior 竞态。

#### `screenshot` -- 截图

```
bk screenshot [--output <path>] [--full-page] [--session <name>] [--target <targetId>]
```

| 参数 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `--output` | string | stdout(base64) | 保存到文件路径 |
| `--full-page` | flag | false | 截取完整可滚动页面 |
| `--session` | string | default session | 指定 session |
| `--target` | string | session active tab | 指定 tab |

#### `tabs` -- 列出 tab

```
bk tabs [--session <name>] [--all]
```

| 参数 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `--session` | string | default session | 指定 session |
| `--all` | flag | false | 显示本 session 所有 tab（含非 active 的） |

行为明确：
- 只返回本 session 的 tab
- 不返回用户自己的 tab
- 返回每个 tab 的 targetId、url、title、是否为 active tab

返回示例：

```json
{
  "ok": true,
  "data": {
    "session": "default",
    "active_target": "E3B2A1F09C7D4A68",
    "tabs": [
      {
        "target": "E3B2A1F09C7D4A68",
        "url": "https://taobao.com/search?q=laptop",
        "title": "laptop - 淘宝搜索",
        "active": true
      },
      {
        "target": "F4C3B2A10D8E5B79",
        "url": "https://item.taobao.com/item.htm?id=12345",
        "title": "MacBook Pro 14 - 淘宝",
        "active": false
      }
    ]
  }
}
```

#### `open` -- 新开 tab

```
bk open <url> [--session <name>] [--timeout <ms>]
```

| 参数 | 类型 | 说明 |
|------|------|------|
| `url` | string | 要打开的 URL |
| `--session` | string | 指定 session（不存在则自动创建） |
| `--timeout` | u64 | 导航超时（默认 30000ms） |

行为明确：
- 在对应 BrowserContext 中开新 tab（isolated session 在独立 BrowserContext 中，default session 在默认 BrowserContext 中）
- 不自动 focus（后台静默操作，需要时显式加 `--focus`）
- 设为 session active tab
- 返回新 tab 的 target + 自动 snapshot

#### `close` -- 关闭 tab

```
bk close [--session <name>] [--target <targetId>]
```

| 参数 | 类型 | 说明 |
|------|------|------|
| `--session` | string | 指定 session |
| `--target` | string | 要关闭的 tab（不传则关闭当前 active tab） |

#### `session close` -- 关闭 session

```
bk session close [--session <name>]
```

| 参数 | 类型 | 说明 |
|------|------|------|
| `--session` | string | 要关闭的 session 名称。不传则关闭 default session 的所有 agent tab |

行为：关闭 session 的所有 tab。Isolated session 还会销毁对应的 BrowserContext。

#### `session list` -- 列出 session

```
bk session list
```

无参数。返回所有活跃 session 的名称、类型（default/isolated）、tab 数量、最后活跃时间。调试用途。

#### `session cookies` -- Cookie 管理

```
bk session cookies set --file <cookies.json> [--session <name>]
bk session cookies get [--session <name>]
bk session cookies clear [--session <name>]
```

| 参数 | 类型 | 说明 |
|------|------|------|
| `--file` | string | cookies.json 文件路径（set 子命令必填） |
| `--session` | string | 指定 session |

行为说明：
- `cookies set --file` 通过 CDP `Network.setCookie` 批量注入 cookie（支持 httpOnly cookie，JS 无法注入但 CDP 可以）
- cookies.json 格式与 Chrome DevTools 导出格式兼容（name/value/domain/path/secure/httpOnly/expires 字段）
- `cookies get` 返回当前 session（BrowserContext）的所有 cookie
- `cookies clear` 清除当前 session 的所有 cookie
- 典型用途：在 isolated session 中注入已有账号的登录 cookie，跳过登录流程

#### `status` -- 连接状态

```
bk status
```

返回 daemon 状态、已连接浏览器、session 列表、tab 列表。仅调试/诊断用。

## 4. snapshot 返回格式设计

这是整个重设计中最重要的决策。snapshot 的输出直接决定了 agent 的"视觉"质量和 token 消耗。

### 4.1 设计原则

1. **token 效率优先**：agent 每次 snapshot 消耗的 token 决定了对话的有效长度
2. **可操作性**：返回的每个元素必须附带 ref，agent 可以直接在下一步 act 中使用
3. **上下文充分**：agent 必须能从 snapshot 中判断页面是什么、可以做什么
4. **防 prompt injection**：页面内容中的文本不能干扰 agent 的推理

### 4.2 完整输出格式

#### 示例 1：登录页面

```json
{
  "ok": true,
  "data": {
    "url": "https://app.example.com/login",
    "title": "Sign In - Example App",
    "target": "E3B2A1F09C7D4A68",
    "viewport": {"width": 1280, "height": 720},
    "scroll": {"x": 0, "y": 0, "height": 900, "percent": 0},
    "elements": [
      {"ref": 42, "tag": "input", "type": "email", "placeholder": "Email address", "id": "email"},
      {"ref": 55, "tag": "input", "type": "password", "placeholder": "Password", "id": "password"},
      {"ref": 61, "tag": "a", "text": "Forgot password?", "href": "/forgot"},
      {"ref": 67, "tag": "button", "text": "Sign In", "type": "submit"},
      {"ref": 73, "tag": "a", "text": "Create account", "href": "/signup"}
    ],
    "total_elements": 5,
    "elements_shown": 5,
    "page_text": "[PAGE_CONTENT_START]Sign In
Welcome back. Please sign in to continue.

Email address
Password
Forgot password?
Sign In
Don't have an account? Create account[PAGE_CONTENT_END]",
    "truncated": false
  }
}
```

#### 示例 2：商品列表页面

```json
{
  "ok": true,
  "data": {
    "url": "https://shop.example.com/products?page=2",
    "title": "Products - Page 2",
    "target": "E3B2A1F09C7D4A68",
    "viewport": {"width": 1280, "height": 720},
    "scroll": {"x": 0, "y": 450, "height": 3200, "percent": 18},
    "elements": [
      {"ref": 101, "tag": "a", "text": "Home", "href": "/"},
      {"ref": 102, "tag": "input", "type": "search", "placeholder": "Search products...", "id": "search"},
      {"ref": 103, "tag": "button", "text": "Search", "aria_label": "Search products"},
      {"ref": 110, "tag": "select", "id": "sort", "options": ["Price: Low to High", "Price: High to Low", "Newest", "Rating"]},
      {"ref": 120, "tag": "a", "text": "Wireless Headphones XM5", "href": "/products/xm5"},
      {"ref": 121, "tag": "button", "text": "Add to Cart", "aria_label": "Add Wireless Headphones XM5 to cart"},
      {"ref": 130, "tag": "a", "text": "USB-C Hub 7-in-1", "href": "/products/usbc-hub"},
      {"ref": 131, "tag": "button", "text": "Add to Cart", "aria_label": "Add USB-C Hub 7-in-1 to cart"},
      {"ref": 200, "tag": "a", "text": "1", "href": "?page=1", "aria_label": "Page 1"},
      {"ref": 201, "tag": "a", "text": "3", "href": "?page=3", "aria_label": "Page 3"},
      {"ref": 202, "tag": "a", "text": "Next", "href": "?page=3", "aria_label": "Next page"}
    ],
    "total_elements": 128,
    "elements_shown": 11,
    "page_text": "[PAGE_CONTENT_START]Products - Page 2

Showing 11-20 of 156 products

Wireless Headphones XM5
99.99 | 4.8 stars (2,341 reviews)
Add to Cart

USB-C Hub 7-in-1
9.99 | 4.5 stars (891 reviews)
Add to Cart

... (8 more products)

Page: 1 [2] 3 4 ... 16 Next[PAGE_CONTENT_END]",
    "truncated": true
  }
}
```

### 4.3 字段含义与设计理由

| 字段 | 类型 | 说明 | 设计理由 |
|------|------|------|----------|
| `url` | string | 当前页面 URL | agent 需要知道我在哪，省去额外 url 命令 |
| `title` | string | 页面标题 | 辅助 agent 理解页面语义 |
| `target` | string | CDP targetId | agent 用于多 tab 时传给 --target |
| `viewport` | object | 视口宽高 | agent 理解页面布局规模 |
| `scroll` | object | 滚动位置 + 总高度 + 百分比 | agent 判断是否需要滚动、还有多少内容 |
| `elements` | array | 可交互元素列表 | 核心数据：agent 的操作手柄 |
| `total_elements` | number | 页面上可交互元素总数（包括未返回的） | agent 判断是否需要 scroll 查看更多元素 |
| `elements_shown` | number | 本次返回的元素数量（compact 模式最多 50，full 模式无限制） | 与 total_elements 对比判断是否有遗漏 |
| `page_text` | string | 页面可见文本（截断） | agent 理解页面内容，不仅仅是按钮 |
| `truncated` | bool | page_text 是否被截断 | agent 知道是否需要 scroll 看更多内容 |

**元素字段明细：**

| 字段 | 存在条件 | 说明 |
|------|----------|------|
| `ref` | 始终 | backendNodeId，传给 act --ref 使用 |
| `tag` | 始终 | HTML 标签名（a, button, input, select 等） |
| `text` | 有文本时 | 元素文本内容（截断到 100 字符） |
| `type` | input/select/textarea | 输入类型（email, password, checkbox 等） |
| `id` | 有 id 时 | DOM id 属性 |
| `href` | a 标签 | 链接地址 |
| `placeholder` | 有 placeholder 时 | 输入框占位文本 |
| `aria_label` | 有 aria-label 时 | 无障碍标签（帮助 agent 理解元素用途） |
| `value` | 表单元素 | 当前值（输入框当前文本、select 当前选中） |
| `checked` | checkbox/radio | 是否选中 |
| `options` | select 元素 | 可选项列表（值数组） |

### 4.4 compact 和 full 两种模式

**compact 模式**（默认）：
- page_text 截断到 2000 字符
- 元素只返回前 50 个（覆盖首屏）
- 元素文本截断到 100 字符
- 不返回坐标（x/y/width/height）

**full 模式**（`--full` 标志）：
- page_text 截断到 8000 字符
- 返回所有可交互元素（无上限）
- 元素文本截断到 200 字符
- 返回元素坐标信息
- 返回 `ancestors` 字段（DOM 祖先路径，帮助定位）

设计理由：compact 模式适用于 90% 场景，最小化 token 消耗。full 模式用于复杂页面或 agent 需要更多上下文时。

### 4.5 ref 的生成规则和稳定性

ref 是 CDP 的 backendNodeId，具有以下特性：

1. **唯一性**：同一页面中每个 DOM 节点有唯一的 backendNodeId
2. **稳定性**：只要节点还在 DOM 中，ref 就不会变。DOM 重排、属性修改、样式变化都不会影响。
3. **失效条件**：节点被从 DOM 中移除时 ref 失效
4. **跨 snapshot 稳定**：两次 snapshot 之间如果 DOM 没有增删节点，相同元素的 ref 保持一致

Agent 使用 ref 的最佳实践：
- snapshot 后立即使用 ref 操作（最安全）
- 如果 act 返回 `REF_NOT_FOUND` 错误，说明 DOM 发生了变化，需要重新 snapshot
- 不需要记忆或缓存 ref，每次 snapshot 都会返回最新的

### 4.6 page_text 的截断策略

1. 提取 `document.body.innerText`（只含可见文本，不含隐藏元素或 script）
2. 移除连续空行（压缩为单个换行）
3. compact 模式截断到 2000 字符，full 模式截断到 8000 字符
4. 截断位置尝试在句子或段落边界
5. 截断后设置 `truncated: true`

显式 `--max-tokens` 在上述 mode limit 之后应用。预算估算固定为 `ceil(serialized UTF-8 JSON bytes / 4)`，scope 仅为 `elements + page_text`，不是任一模型的精确 tokenizer。响应保留 legacy `truncated`，并新增 `token_budget` 与 `truncation.elements/page_text`，分别报告 requested/estimated、total/shown bytes 或 counts，以及 `source_limit`、`mode_limit`、`token_budget`、`excluded` 原因。不传预算时 compact/full 内容选择保持兼容。

### 4.7 untrusted content wrapping（防 prompt injection）

页面内容是不可信的。恶意页面可能在文本中注入 prompt injection 攻击。

以下所有来自页面的字段均视为不可信内容，在 snapshot 返回的 JSON 结构中通过字段位置隔离（JSON 结构本身是信任的，只有特定字段的值是不可信的）：
- `data.title`
- `data.page_text`
- `elements[].text`
- `elements[].placeholder`
- `elements[].aria_label`
- `elements[].options[]`

防护措施：
- `page_text` 被包裹在 `[PAGE_CONTENT_START]` ... `[PAGE_CONTENT_END]` 标记中（因为它可能包含多行长文本，更容易被注入）
- 其他不可信字段因为较短（100 字符截断）且结构化，风险较低，通过 JSON 字段位置隔离
- agent 的 system prompt 应明确指出上述字段来自网页内容，不应被当作指令执行

这种 wrapping 方式与 Playwright MCP 的做法一致，已被验证有效。

### 4.8 iframe 处理

> **iframe 处理（v2 默认行为）**
>
> snapshot 默认**不穿透** iframe。iframe 元素本身会出现在 elements 列表中（tag: "iframe"，带 src 属性），但 iframe 内部的元素不会被采集。
>
> 理由：iframe 内容属于不同的 browsing context，直接穿透采集会大幅增加 snapshot 延迟和 token 消耗，且很多 iframe 跨域（如 OAuth 弹窗、第三方支付）无法访问。
>
> 如果 agent 需要操作 iframe 内的元素：
> - 对于同源 iframe，使用 `bk evaluate` + `document.querySelector('iframe').contentDocument.querySelector(...)` 操作
> - 对于跨域 iframe（如 OAuth），无法直接操作，需要用户手动完成
>
> Phase 2 计划：通过 `snapshot --frame <iframeRef>` 支持采集指定 iframe 内的元素。

## 5. act 返回格式设计

### 5.1 设计原则

act 的返回必须告诉 agent：
1. 操作是否成功
2. 操作产生了什么效果（state diff）
3. 如果失败，为什么失败 + 怎么恢复

### 5.2 成功时的完整返回结构

```json
{
  "ok": true,
  "data": {
    "action": "click",
    "ref": 67,
    "result": "completed",
    "state_diff": {
      "url_changed": {"from": "https://app.example.com/login", "to": "https://app.example.com/dashboard"},
      "title_changed": {"from": "Sign In", "to": "Dashboard"},
      "elements_added": 12,
      "elements_removed": 5
    },
    "target": "E3B2A1F09C7D4A68"
  }
}
```

state_diff 字段说明：

| 字段 | 类型 | 说明 |
|------|------|------|
| `url_changed` | object/null | URL 是否变化（含 from/to） |
| `title_changed` | object/null | 标题是否变化 |
| `elements_added` | number | 新增的可交互元素数量 |
| `elements_removed` | number | 消失的可交互元素数量 |

state_diff 的目的是让 agent 快速判断操作有没有产生预期效果，而不需要再调一次 snapshot。如果 diff 显示页面发生了显著变化（URL 变了、大量元素增删），agent 通常需要再 snapshot 一次获取完整新状态。

> **state_diff 的捕获时机**：act 操作完成（鼠标事件已派发/输入已完成）后，等待最多 500ms 的 DOM 稳定窗口，然后对比操作前后的 URL、title、可交互元素数量。不等待 networkidle，因此对于触发异步加载的 click 操作，state_diff 中的 elements_added/removed 可能不完整。如需确认异步操作结果，在 act 后调用 `wait --idle` 再 snapshot。

### 5.3 click 触发新 tab

当前 act 响应会在页面通过 `target=_blank` 等方式打开新 target 时报告 `new_tab`。browserkit 的 session-native target lifecycle tracking 会把新 target 登记到当前 session；agent 需要操作新页时应使用响应里的 target 或后续 `bk tabs` 结果。

### 5.4 失败时的完整返回结构

```json
{
  "ok": false,
  "error": {
    "code": "REF_NOT_FOUND",
    "message": "element ref 67 not found in current DOM",
    "suggestion": "call snapshot to refresh refs -- page may have changed since last snapshot",
    "recoverable": true
  }
}
```

错误字段说明：

| 字段 | 类型 | 说明 |
|------|------|------|
| `code` | string | 机器可读的错误码（agent 可 switch on） |
| `message` | string | 人类可读的错误描述 |
| `suggestion` | string | 建议的恢复步骤 |
| `recoverable` | bool | 是否可恢复（true = agent 可以尝试修正；false = 需人工介入） |

### 5.5 dialog 边界

v2 act 当前没有 dialog kind，也没有 `blocked_by_dialog` success result contract。独立的 `bk dialog` 命令属于 session-native command surface，不是 `bk act` 的兼容别名。

## 6. 连接设计

### 6.1 核心理念

连接是**推荐但非强制**的前置步骤。`bk connect` 用于显式建立连接并检查状态，但其他命令（`snapshot` / `act` / `navigate` / `open` 等）在未连接时会自动尝试一次 connect：

- 检测到 `NOT_CONNECTED` 状态时，自动尝试 `connect` 一次然后重试原命令
- 如果自动 connect 成功，对 agent 透明（直接返回正常结果）
- 如果自动 connect 失败，返回具体的连接错误（`BROWSER_NOT_RUNNING`、`REMOTE_DEBUG_NOT_ENABLED` 等）

因此：
- `bk connect` 变为推荐但非必须的步骤（用于提前检查连接状态、确认浏览器版本等）
- agent 也可以直接 `bk open <url>`，未连接时自动触发 connect

这符合"能一步完成绝不拆两步"的设计哲学。connect 作为幂等检查命令保留，但不作为强制前置步骤。

### 6.2 `bk connect` 命令

```
bk connect [--session <name>] [--timeout <ms>]
```

| 参数 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `--session` | string | default session | 指定 session |
| `--timeout` | u64 | 30000 | 超时毫秒数 |

**行为：**

- 检查是否已有可用连接（daemon 已连接 Chrome + session 已就绪）
- 已连接 → 直接返回连接状态，不做任何操作，status: "already_connected"
- 未连接 → 按以下检测流程发现并连接浏览器，创建 session，status: "connected"
- 连接失败 → 返回对应错误码（见下方流程）

**返回示例：**

```json
// 已连接
{
  "ok": true,
  "data": {
    "status": "already_connected",
    "browser": "Chrome 136",
    "session": "default",
    "tabs": 3
  }
}

// 新连接成功
{
  "ok": true,
  "data": {
    "status": "connected",
    "browser": "Chrome 136",
    "session": "default",
    "tabs": 5
  }
}
```

#### Chrome 连接检测流程（按优先级顺序）

**第1步：检测 Chrome 是否在运行**
- 在运行 → 检查 DevToolsActivePort 文件是否存在
  - 文件存在 → 尝试 WebSocket 连接
    - 连接成功 → 返回 `status: "connected"`
    - 连接失败 → 返回 `CONNECTION_REFUSED`，提示用户确认 Chrome 是否弹出了授权对话框，点击"允许"后重试 `bk connect`
  - 文件不存在 → 返回 `REMOTE_DEBUG_NOT_ENABLED`，提示：Chrome 正在运行但未开启远程调试，请打开 `chrome://inspect/#remote-debugging` 勾选开启（该设置重启后保留，只需设置一次）

**第2步：Chrome 未运行，检测 Edge 是否在运行**
- Edge 在运行 → 同第1步逻辑，检查 Edge 的 DevToolsActivePort 文件 → 尝试连接
  - 连接成功 → 返回 `status: "connected"`
  - 连接失败 → 返回 `CONNECTION_REFUSED`，提示同上
  - 文件不存在 → 返回 `REMOTE_DEBUG_NOT_ENABLED`，引导打开 `edge://inspect/#remote-debugging`

**第3步：Chrome 和 Edge 都未运行，检测是否已安装**
- 检测 Chrome 是否已安装：
  - 已安装 → 检测版本是否 ≥ 112
    - 版本满足 → 返回 `BROWSER_NOT_RUNNING`，提示用户手动打开 Chrome，打开后重试 `bk connect`
    - 版本不满足 → 返回 `BROWSER_VERSION_TOO_OLD`，提示用户升级 Chrome
  - Chrome 未安装 → 检测 Edge 是否已安装：
    - Edge 已安装 → 检测版本是否 ≥ 112
      - 版本满足 → 返回 `BROWSER_NOT_RUNNING`，提示用户手动打开 Edge，打开后重试 `bk connect`
      - 版本不满足 → 返回 `BROWSER_VERSION_TOO_OLD`，提示用户升级 Edge
    - Edge 也未安装 → 返回 `BROWSER_NOT_INSTALLED`，提示用户安装 Chrome

**注意：不提供自动启动浏览器功能，全部引导用户手动操作。**

### 6.3 幂等性

`bk connect` 是幂等的：已连接时直接返回状态，不重连。多次调用完全安全，agent 可以在任务开头无条件调用。

### 6.4 daemon 持久化连接

一旦连接成功，daemon 跨会话保持连接（Chrome 不关就一直有效）。重连时机：Chrome 重启后连接断开，agent 再次调 `bk connect` 即可重连。

### 6.5 自动 discover 浏览器的流程

浏览器开启远程调试后会在用户数据目录写 `DevToolsActivePort` 文件。bk 利用这个文件自动发现正在运行的浏览器：

1. 扫描已知的浏览器用户数据目录：
   - Chrome:
     - Windows: `%LOCALAPPDATA%/Google/Chrome/User Data/`
     - macOS: `~/Library/Application Support/Google/Chrome/`
     - Linux: `~/.config/google-chrome/`
   - Edge:
     - Windows: `%LOCALAPPDATA%/Microsoft/Edge/User Data/`
     - macOS: `~/Library/Application Support/Microsoft Edge/`
     - Linux: `~/.config/microsoft-edge/`
2. 读取 `DevToolsActivePort` 文件（第一行是端口，第二行是 WebSocket path）
3. 构造 WebSocket URL: `ws://127.0.0.1:<port><path>`
4. 建立 CDP 连接
5. 注册为 unmanaged browser（bk 不负责其生命周期）

### 6.6 失败时的错误码

连接失败时根据检测流程的具体阶段返回对应错误码，详见 [7.2 完整 error code 列表](#72-完整-error-code-列表) 中的 `REMOTE_DEBUG_NOT_ENABLED`、`CONNECTION_REFUSED`、`BROWSER_NOT_RUNNING`、`BROWSER_VERSION_TOO_OLD`、`BROWSER_NOT_INSTALLED`。

### 6.7 完全禁止 auto-launch headless Chrome

v2 设计决策：**永远不会自动启动无头浏览器**。

理由：
1. agent 操作的是用户已打开的真实浏览器（已登录、有 cookie、有状态）
2. 自动启动无头浏览器对 agent 来说是一个空壳，没有任何有用的状态
3. 如果 agent 需要一个全新的浏览器环境，应该明确调用 `browser connect` 或由用户手动准备

Phase 2 支持 `--headless` flag 显式启动无头 Chrome，但绝不自动触发。

### 6.8 Chrome 崩溃检测

daemon 需要主动检测 Chrome 进程健康状态，而不是等 agent 的下一条命令失败后才发现。

**实现方式**：daemon 在建立 CDP WebSocket 连接后，监听 WebSocket 的 close/error 事件。Chrome 崩溃时 WebSocket 会关闭，daemon 通过这个事件立即感知。

**崩溃后的处理**：
1. 将该浏览器下所有 session 标记为 `disconnected` 状态
2. 清理对应的内存状态（browsers DashMap 中删除该 browser）
3. 所有 session 的后续命令立即返回 `CHROME_DISCONNECTED` 错误，不再挂起等待

**恢复流程**：Chrome 重启后，agent 调用 `bk connect` 重新连接（connect 是幂等的，可以安全重试）。

### 6.9 安全默认值

**`disable_security` 默认为 `false`**。启用 `disable_security: true` 会向 Chrome 传入 `--disable-web-security` 和 `--ignore-certificate-errors` 标志，允许跨域请求和读取本地文件，仅在受信任的测试环境中使用。

### 6.10 daemon 访问控制

daemon 启动时生成随机 token 写入 `~/.bk/daemon.token`（0600 权限，仅 owner 可读）。所有 TCP 请求必须在 header 中携带此 token。CLI 客户端自动读取并附带，对用户透明。此机制防止同机器其他进程未授权访问 daemon。

## 7. 错误处理规范

### 7.1 统一错误返回格式

所有命令失败时返回统一结构：

```json
{
  "ok": false,
  "error": {
    "code": "ERROR_CODE",
    "message": "human-readable description",
    "suggestion": "what the agent should do next",
    "recoverable": true
  }
}
```

### 7.2 完整 error code 列表

| Code | 含义 | recoverable | 典型 suggestion |
|------|------|-------------|-----------------|
| `NOT_CONNECTED` | 未建立浏览器连接 | true | run 'bk connect' first to establish a browser connection |
| `REF_NOT_FOUND` | 元素 ref 在当前 DOM 中不存在 | true | call snapshot to refresh refs -- page may have changed |
| `REMOTE_DEBUG_NOT_ENABLED` | 浏览器在运行但未开启远程调试 | true | open chrome://inspect/#remote-debugging and enable remote debugging, then retry bk connect |
| `CONNECTION_REFUSED` | 调试端口存在但连接失败（含授权弹窗场景） | true | check if Chrome showed an authorization dialog and click Allow, then retry |
| `BROWSER_NOT_RUNNING` | 浏览器已安装但未运行 | true | manually open Chrome/Edge, then retry bk connect |
| `BROWSER_VERSION_TOO_OLD` | 浏览器版本过低（< 112） | false | upgrade Chrome/Edge to version 112 or later |
| `BROWSER_NOT_INSTALLED` | Chrome 和 Edge 均未安装 | false | install Google Chrome from https://www.google.com/chrome |
| `CHROME_DISCONNECTED` | Chrome 连接已断开 | true | Chrome may have closed; run bk status to check |
| `SESSION_NOT_FOUND` | 指定的 session 不存在 | true | session may have expired or been closed; create a new one |
| `SESSION_NO_TAB` | session 中没有可操作的 tab | true | use bk open to create a tab first |
| `DIALOG_BLOCKING` | JavaScript 对话框阻塞了操作 | true | v2 act has no dialog kind; resolve the dialog outside the v2 act surface, then retry |
| `NAVIGATE_FAILED` | 页面导航失败（网络错误、无效 URL） | true | check URL is valid and accessible |
| `TIMEOUT` | 操作超时 | true | increase --timeout or check if page is responsive |
| `ELEMENT_NOT_VISIBLE` | 元素存在但不可见/不可交互 | true | element may be hidden or overlapped; try scrolling or waiting |
| `ELEMENT_NOT_INTERACTABLE` | 元素被禁用或只读 | true | element is disabled; check page state |
| `TARGET_NOT_FOUND` | 指定的 targetId 不存在 | true | tab may have been closed; run bk tabs to see available tabs |
| `TARGET_CRASHED` | 目标 tab 崩溃 | false | tab has crashed and cannot recover |
| `JS_ERROR` | JavaScript 执行产生异常 | true | check expression syntax |
| `INVALID_ARGUMENT` | 参数格式错误 | true | check command syntax |
| `DAEMON_ERROR` | daemon 内部错误 | false | restart daemon: bk daemon stop and bk daemon start |
| `FILE_NOT_FOUND` | upload 的文件路径不存在 | true | check file path exists and is absolute |
| `SELECTOR_NOT_FOUND` | CSS 选择器未匹配到元素 | true | selector matched no elements; check page state |
| `SESSION_LIMIT_EXCEEDED` | 已达最大 session 数量 | true | close unused sessions with 'bk session close --session <name>' |
| `TAB_LIMIT_EXCEEDED` | 已达该 session 的最大 tab 数 | true | close unused tabs with 'bk close --target <tid>' |

### 7.3 错误码的使用建议

agent 应该对以下常见错误码有固定的恢复策略：

1. **`NOT_CONNECTED`**：agent 尚未建立连接，应先调 `bk connect` 再重试。
2. **`REF_NOT_FOUND`**：最常见的错误。agent 应该自动重新 snapshot，然后重试操作。
3. **`REMOTE_DEBUG_NOT_ENABLED`**：需要人工操作后可恢复。agent 应该告知用户"请打开 chrome://inspect/#remote-debugging 开启远程调试，完成后重试 bk connect"。
4. **`CONNECTION_REFUSED`**：可恢复，agent 应提示用户检查浏览器是否弹出了授权对话框，点击允许后重试。
5. **`BROWSER_NOT_RUNNING`**：需要人工操作后可恢复。agent 应该告知用户"请手动打开浏览器，然后重试 bk connect"。
6. **`BROWSER_VERSION_TOO_OLD`**：无法恢复，agent 应该告知用户升级浏览器到 112 或更高版本。
7. **`BROWSER_NOT_INSTALLED`**：无法恢复，agent 应该告知用户安装 Chrome。
8. **`SESSION_NO_TAB`**：agent 需要先 `bk open <url>` 创建一个 tab。
9. **`TIMEOUT`**：可能是页面加载慢，agent 可以增加 timeout 重试，或先 wait --idle 再操作。
10. **`DIALOG_BLOCKING`**：v2 act 不提供 dialog kind；需要在 v2 act 之外处理对话框后再重试。
11. **`NAVIGATE_FAILED`**：检查 URL 是否正确，可能是网络问题。

> **关于 `recoverable` 语义**：`recoverable: true` 并不意味着 agent 可以自动恢复，而是表示"通过某种操作（可能需要人工参与）后可以恢复"。例如 `BROWSER_NOT_RUNNING` 标记为 recoverable，因为用户打开浏览器后 agent 重试即可成功——虽然 agent 自己无法打开浏览器，但整个流程是可恢复的。`recoverable: false` 表示无论如何都无法恢复（如 `TARGET_CRASHED`、`BROWSER_VERSION_TOO_OLD`）。

## 8. 全局参数设计

### 8.1 保留的全局参数

| 参数 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `--session <name>` | string | (default session) | 指定操作的 session |
| `--target <targetId>` | string | session active tab | 指定操作的 tab |
| `--timeout <ms>` | u64 | 30000 | 操作超时毫秒数 |
| `--no-state-diff` | flag | false | act 后不附带状态变化（节省 token） |
| `--focus` | flag | false | 显式 focus 操作的 tab |

### 8.2 删除的全局参数

| 删除的参数 | 理由 |
|-----------|------|
| `--ws` / `-w` | workspace 概念删除，由 session 替代 |
| `--format` | 永远输出 JSON，不需要格式选择 |
| `BK_WS` 环境变量 | workspace 不暴露 |

### 8.3 环境变量

| 环境变量 | 说明 |
|---------|------|
| `BK_SESSION` | 当前 session 名称，等同于每条命令带 `--session`。多 agent 场景下，在 agent 进程启动时 export 一次即可。`--session` 参数优先级高于环境变量。 |

### 8.4 输出格式

v2 永远输出 JSON。不再支持 text/tsv 格式。

理由：
- agent 解析 JSON 是零成本的
- text 格式对 agent 来说反而需要额外解析
- 统一格式减少 agent 的认知负担
- 错误信息也是结构化的（code + message + suggestion），比纯文本更易处理

成功时：

```json
{
  "ok": true,
  "data": { ... }
}
```

失败时：

```json
{
  "ok": false,
  "error": { "code": "...", "message": "...", "suggestion": "...", "recoverable": true/false }
}
```

## 9. Agent 使用工作流

### 9.1 基础工作流

```bash
# 首次使用：运行一次 setup（之后不需要再运行）
bk setup

# 之后每次使用：
# Step 0: 推荐：显式检查连接状态（幂等，可重复调用）
bk connect
# 也可以跳过 connect，直接开始操作——未连接时自动触发连接

# Step 1: 看页面
bk snapshot
# -> 返回 elements(with refs) + page_text + target

# Step 2: 操作
bk act click --ref 67
# -> 返回操作结果 + state_diff

# Step 3: 如果 state_diff 显示重大变化，再看一次
bk snapshot
# -> 获取新页面的完整状态
```

### 9.2 单 agent 操作用户已登录网站（default session）

```bash
# 不需要指定 --session，自动使用 default session
# default session 共享用户 Chrome 的 cookie，自动继承登录态

bk connect                           # 推荐：显式检查连接状态
bk open https://taobao.com          # 新 tab，继承用户登录态（未连接时自动触发 connect）
bk snapshot                          # 操作 active tab，不用带 --session/--target
bk act type --ref 42 --text "搜索词"
bk act click --ref 55
bk snapshot
bk session close                     # 关闭 agent 开的 tab
```

### 9.3 多 agent 并行，cookie 隔离（isolated session）

```bash
# agent-a（独立 BrowserContext，独立 cookie）
bk connect --session a                # 推荐：显式建立连接
bk open https://shop.com --session a  # 创建 BrowserContext-A，新 tab
bk snapshot --session a
bk act fill --set ref:42=user_a@example.com --set ref:55=pass_a --session a
bk act click --ref 67 --session a
bk session close --session a

# agent-b（同时运行，完全隔离）
bk connect --session b                # 推荐：显式建立连接
bk open https://shop.com --session b  # 创建 BrowserContext-B，独立 cookie
bk snapshot --session b
bk act fill --set ref:42=user_b@example.com --set ref:55=pass_b --session b
bk act click --ref 67 --session b
bk session close --session b
```

**关键点**：两个 agent 同时操作同一个网站，但 cookie 完全隔离。agent-a 登录 user_a，agent-b 登录 user_b，互不干扰。

### 9.4 session 内多 tab 操作

```bash
bk connect --session a                              # 推荐：显式建立连接
bk open https://list.example.com --session a   # tab-1，active
bk snapshot --session a                         # 操作 tab-1
bk open https://detail.example.com --session a # 显式创建 tab-2，active 切换到 tab-2
bk snapshot --session a                         # 操作新 active tab（tab-2）
bk snapshot --session a --target TAO111        # 显式操作 tab-1
```

如果 click 打开 `target=_blank` tab，`bk act click` 会在响应中报告 `new_tab`。需要继续操作新页时，使用该 target 或先运行 `bk tabs` 确认。

### 9.5 登录场景完整示例

**Agent 任务**：登录 app.example.com

```bash
# 0. 推荐：显式检查连接状态
bk connect

# 1. 打开登录页（新 tab，继承 cookie）
bk open https://app.example.com/login
```

返回（open 自带 snapshot）：

```json
{
  "ok": true,
  "data": {
    "url": "https://app.example.com/login",
    "title": "Sign In - Example App",
    "target": "E3B2A1F09C7D4A68",
    "viewport": {"width": 1280, "height": 720},
    "scroll": {"x": 0, "y": 0, "height": 800, "percent": 0},
    "elements": [
      {"ref": 42, "tag": "input", "type": "email", "placeholder": "Email address", "id": "email"},
      {"ref": 55, "tag": "input", "type": "password", "placeholder": "Password", "id": "password"},
      {"ref": 61, "tag": "a", "text": "Forgot password?", "href": "/forgot"},
      {"ref": 67, "tag": "button", "text": "Sign In", "type": "submit"},
      {"ref": 73, "tag": "a", "text": "Create account", "href": "/signup"}
    ],
    "total_elements": 5,
    "elements_shown": 5,
    "page_text": "[PAGE_CONTENT_START]Sign In
Welcome back...
Email address
Password
Sign In[PAGE_CONTENT_END]",
    "truncated": false
  }
}
```

```bash
# 2. 填写表单
bk act fill --set ref:42=user@example.com --set ref:55=MyP@ssw0rd
```

返回：

```json
{
  "ok": true,
  "data": {
    "action": "fill",
    "result": "completed",
    "fields": [
      {"ref": 42, "status": "filled", "value": "user@example.com"},
      {"ref": 55, "status": "filled", "value": "MyP@ssw0rd"}
    ],
    "state_diff": {"url_changed": null, "title_changed": null, "elements_added": 0, "elements_removed": 0},
    "target": "E3B2A1F09C7D4A68"
  }
}
```

```bash
# 3. 点击登录按钮
bk act click --ref 67
```

返回：

```json
{
  "ok": true,
  "data": {
    "action": "click",
    "ref": 67,
    "result": "completed",
    "state_diff": {
      "url_changed": {"from": "https://app.example.com/login", "to": "https://app.example.com/dashboard"},
      "title_changed": {"from": "Sign In - Example App", "to": "Dashboard - Example App"},
      "elements_added": 15,
      "elements_removed": 5
    },
    "target": "E3B2A1F09C7D4A68"
  }
}
```

Agent 看到 URL 变为 /dashboard，登录成功。如需了解 dashboard 内容，再调一次 snapshot。

### 9.6 多 agent 使用 BK_SESSION 环境变量

```bash
# 多 agent 场景推荐做法：进程级别设置 BK_SESSION，避免每条命令都带参数
export BK_SESSION=agent-a

bk connect
bk open https://shop.com
bk snapshot
bk act click --ref 42
bk session close
```

每个 agent 进程启动时 export 一次 `BK_SESSION`，后续所有命令自动使用该 session，无需每条命令都带 `--session` 参数。`--session` 参数优先级高于环境变量。

### 9.7 错误恢复场景

**场景**：页面 DOM 动态更新导致 ref 失效

```bash
# 0. 推荐：显式检查连接状态
bk connect

# 1. snapshot 获取元素
bk snapshot
# -> 看到 ref:42 是搜索按钮

# 2. 但在 agent 决策期间页面自动刷新了
bk act click --ref 42
# -> 返回 REF_NOT_FOUND 错误

# 3. Agent 自动恢复：重新 snapshot
bk snapshot
# -> 获取新的元素列表，搜索按钮现在是 ref:89

# 4. 重试操作
bk act click --ref 89
# -> 成功
```

## 10. 与当前版本的对比表

| 维度 | 旧版本 (v1) | 新版本 (v2) |
|------|-------------|-------------|
| 主命令数量 | 35+（含子命令） | 10（6 主要 + 4 辅助） + session 管理 |
| 首次配置 | 需手动理解 chrome://inspect | bk setup 交互式引导 |
| 连接浏览器 | 4 步手动（discover -> ws new -> tab list -> tab switch） | `bk connect`（幂等，推荐但非必须）或其他命令自动触发 |
| Chrome 崩溃感知 | 等下一条命令失败才发现 | WebSocket close 事件立即感知 |
| 多 agent 隔离 | workspace 手动管理，有竞态 | session 自动隔离，BrowserContext 保证 |
| Session 资源控制 | workspace_timeout 30分钟，无数量上限 | max_sessions/max_tabs 配额，72h 超时 |
| 用户 tab 保护 | 无保护，可能误操作 | agent 不可见用户 tab |
| 新 tab 归属 | 推断，有竞态 | `bk open` 和 click 产生的新 target 都由 session-native lifecycle tracking 登记 |
| session 生命周期 | workspace 手动管理 | 自动创建，72h 超时，支持显式 close |
| 操作后获知结果 | 需额外调 `info` | 自动附带 state_diff |
| 输出格式 | text/json/tsv 三选一 | 永远 JSON |
| 元素寻址 | index（不稳定）或 ref（稳定） | 统一 ref（index 不再暴露） |
| 默认 type 行为 | 追加（append） | 替换（clear + type） |
| 错误信息 | 纯文本 string | 结构化（code + message + suggestion + recoverable） |
| workspace 概念 | 暴露给用户，需手动管理 | 删除，由 session 内部替代 |
| dialog 处理 | 需手动 dialog accept/dismiss | 默认 manual；可用 `bk dialog policy accept|dismiss` 配置自动策略 |
| 浏览器启动 | 默认 auto-launch headless | 完全禁止 auto-launch |
| daemon 访问控制 | 无鉴权 | daemon.token 随机 token |
| agent 认知负担 | 高（需理解 ws/tab/browser/daemon 关系） | 低（只需理解 snapshot + act + session） |
| token 效率 | 低（info 输出冗长，需额外调用） | 高（精简返回，自动 state_diff） |
| 多 tab 管理 | 通过 workspace + tab 子命令 | `--target` 参数 + `tabs` 命令 |
| 首次使用门槛 | 需理解架构后才能操作 | `bk open <url>` 即可开始（自动触发连接） |

## 11. 当前兼容与迁移状态

### 11.1 已移除命令及替代方式

v2 action surface 是 breaking change。下表中的旧命令已经移除，不是仍可执行的 alias：

| 已移除命令 | 替代命令 | 状态 |
|------------|----------|------|
| `bk goto <url>` | `bk navigate <url>` | 已移除 |
| `bk info` | `bk snapshot` | 已移除 |
| `bk eval <expr>` | `bk evaluate <expr>` | 已移除 |
| `bk shot` | `bk screenshot` | 已移除 |
| `bk click --ref N` | `bk act click --ref N` | 已移除 |
| `bk type --ref N "text"` | `bk act type --ref N --text "text"` | 已移除（注意默认行为变了） |

**没有兼容 alias 的已移除 surface：**

- `ws *` 系列：workspace 概念移除，无法映射
- `tab *` 系列：使用 `open` / `tabs` / `close --target`
- `storage *` 系列：使用 `session cookies *` 和 `session storage *`
- `debug monitor/har/events`：这些旧 streaming 命令没有真实事件流
- `new / ls / rm`：workspace 别名，随 ws 一起删除

### 11.2 Breaking Changes 列表

| 变化 | 影响 | 迁移方式 |
|------|------|----------|
| 输出永远 JSON | 依赖 text 输出的脚本需改 | 解析 JSON 的 data 字段 |
| type 默认替换 | 原来依赖追加行为的需改 | 加 --append 标志 |
| 无 --ws 参数 | 多 workspace 脚本需改 | 用 --session 替代隔离需求 |
| 无 `BK_WS` 环境变量 | 依赖进程级 workspace 的脚本需改 | 用 `BK_SESSION` |
| 无 workspace daemon route | 调用 `ws.*`/`tab.*`/`nav.*`/`page.*`/旧 `storage.*`/`v2.*` 的客户端需改 | 调用当前 canonical command |
| state v2 自动迁移 | 旧 state 文件首次加载会被转换 | 先备份为 `state.v2.backup*.json`，迁移/丢弃计数见 `bk status` |
| 无 auto-launch | 依赖自动启动浏览器的需改 | 手动启动 Chrome 或用 browser connect |
| 错误格式变化 | 解析错误的脚本需改 | 解析 error.code 字段 |
| snapshot 替代 info | 返回格式有变化 | 适配新的 data 结构 |

### 11.3 当前策略

- 当前 runtime 只使用 session 作为浏览器活动边界。
- click/type 等旧 action 命令已经移除，不提供 executable aliases。
- `ws`/`tab`/`fetch`、`BK_WS`/`--ws`、旧 daemon route 和非真实 streaming debug 命令已经移除。
- `browser`、`daemon` 是 admin surface；`debug block|unblock|cdp` 是 developer surface；`dialog`、`find`、`search`、`html`、`console`、`pdf` 是 session-native command surface。

## 12. 实施计划

> **历史规划记录（非当前 CLI contract）**：本节保留早期阶段目标用于追溯。未完成条目不能作为当前可执行命令或响应行为；当前 action contract 以前文、`bk act --help` 和 `docs/ROADMAP.md` 为准。

### Phase 1：最小可用（MVP）

**目标**：agent 能用 snapshot + act + navigate + session 完成基本浏览器操作

**核心实现：**

| 模块 | 变化 |
|------|------|
| Session 管理 | 新增 Session 抽象层：default session + isolated session（BrowserContext） |
| CLI (`src/main.rs`) | 新增 setup/connect/snapshot/act/navigate/open/close/tabs/session 命令；新增 --session 全局参数；删除 --format/--ws；永远 JSON 输出 |
| 协议 (`src/daemon/protocol.rs`) | Response 结构增加结构化 error（code/message/suggestion/recoverable） |
| 错误 (`src/error.rs`) | 新增 ErrorCode enum，每个 BkError 映射到一个 code；实现 suggestion() 和 recoverable() 方法；新增 SESSION_LIMIT_EXCEEDED、TAB_LIMIT_EXCEEDED |
| Handler 路由 | 新增路由：setup, connect, snapshot, act.*, navigate, session.* |
| `handler/setup.rs` | **新文件**：交互式引导 Chrome 远程调试配置——检测 Chrome/Edge 安装、版本、远程调试状态，等待用户确认后验证连接 |
| `handler/session.rs` | **新文件**：session 创建（createBrowserContext）、close（disposeBrowserContext）、list、tab 归属追踪；创建时检查 max_sessions 限制 |
| `handler/snapshot.rs` | **新文件**：实现 snapshot 命令（组合 page state + page text + viewport） |
| `handler/act.rs` | **新文件**：统一 act dispatcher（路由到 click/type/scroll 等）；每个 act 返回带 state_diff |
| `handler/navigate_v2.rs` | **新文件**：navigate 命令（合并 goto/back/forward/reload）；未连接时返回 NOT_CONNECTED |
| `handler/auto_connect.rs` | **新文件**：`bk connect` 的实现逻辑——发现 + 连接 Chrome + 创建 session |
| `page/state_diff.rs` | **新文件**：实现 state_diff 计算（对比操作前后的 URL/title/元素数量） |
| `src/client.rs` | 简化输出逻辑：去掉 format 参数，永远打印 JSON |
| `src/config.rs` | 新增 `[limits]` 配置段：max_sessions、max_tabs_per_session、session_timeout_hours |
| Chrome 崩溃检测 | 监听 CDP WebSocket close/error 事件；崩溃时清理 browsers DashMap 和相关 session 状态；受影响 session 后续命令返回 CHROME_DISCONNECTED |
| Tab 创建检查 | `bk open` 新 tab 时检查 max_tabs_per_session；click 新 target 的 lifecycle tracking 是历史未完成目标 |
| daemon 鉴权 | 启动时生成随机 token 写入 `~/.bk/daemon.token`（0600 权限）；TCP server 验证请求 token；CLI 自动读取附带 |

**Phase 1 交付标准：**

- `bk setup` 能交互式引导用户开启远程调试并验证连接
- `bk connect` 能自动发现 Chrome、建立 CDP 连接并创建 session
- `bk open <url>` 能在已连接状态下创建 tab 并返回 snapshot
- `bk snapshot` 能返回完整页面状态（elements + page_text + scroll）
- `bk act click/type/scroll` 能操作元素并返回 state_diff
- `bk navigate <url>` 能导航
- `--session` 能创建 isolated session（BrowserContext 隔离）
- 未连接时操作命令自动尝试 connect，失败则返回具体连接错误
- 错误返回有 code + suggestion
- 历史目标曾计划保留旧命令 warning；当前 click/type 等 action aliases 已移除
- 监听 CDP WebSocket close/error 事件，崩溃时立即清理 browsers DashMap 和相关 session 状态，受影响的 session 后续命令返回 CHROME_DISCONNECTED
- 在 config.toml 加入 limits 配置项，session 创建时检查 max_sessions 限制，tab 创建时检查 max_tabs_per_session 限制，返回对应 error code
- daemon 启动时生成随机 token 写入 `~/.bk/daemon.token`，TCP 请求携带 token 鉴权

### Phase 2：完整功能

**目标**：覆盖所有 act kind + dialog 自动处理 + session 超时回收

| 模块 | 变化 |
|------|------|
| `handler/act.rs` | 历史目标：完善 fill/select/options/hover/focus/drag/upload/press；dialog 不属于最终 act kind |
| `handler/dialog.rs` | 历史目标（未形成 v2 act contract）：dialog 自动处理 |
| Session 超时 | 72h 无操作自动 close session + 销毁 BrowserContext |
| Active tab 追踪 | 历史未完成目标：click 新 target 自动登记并切换 active tab |
| `handler/snapshot.rs` | 实现 --full 模式；实现 untrusted content wrapping |
| 用户 tab 过滤 | default session 中 tabs 命令只返回 agent 创建的 tab |
| `--headless` flag | 支持显式启动无头 Chrome 进程 |

**Phase 2 新增能力：**

| 模块 | 变化 |
|------|------|
| 网络响应观察 | 当前合同为 `bk network watch --pattern <url-substring> [--count N]`；只返回 XHR/fetch metadata，不读取 body；事件流与乱序 terminal 暂存容量均为 256，并显式报告 overflow/close/drop |
| 文件下载处理 | 当前合同为 `bk download --ref <N> --output-dir <existing-dir>`；先订阅 Browser events，再 click 并等待 GUID 终态 |
| evaluate 追加写入 | 当前合同为 CLI-local `evaluate --append-to <file>`；daemon 返回 result，CLI 仅追加 string 的原始 UTF-8 bytes |
| snapshot token 控制 | 当前合同为 `snapshot --max-tokens <16..100000>`；使用文档化确定性估算并返回分项截断元数据 |

**Phase 2 交付标准：**

- 所有 act kind 都能正常工作
- 历史未完成目标：dialog 自动处理；最终 v2 act 无 dialog kind 或 blocked_by_dialog result contract
- snapshot --full 模式提供完整信息
- page_text 有 [PAGE_CONTENT_START/END] wrapping
- session 超时自动清理
- 历史未完成目标：click 新 target 接入 session-native lifecycle tracking

### Phase 3：优化与清理

**目标**：性能优化 + 删除废弃代码 + 完善边界情况

| 模块 | 变化 |
|------|------|
| `src/main.rs` | 清理剩余 v1 legacy 命令定义；click/type 等 action aliases 已先行移除 |
| `handler/workspace.rs` | 删除（session 已完全替代） |
| `handler/tab.rs` | 精简：只保留 tabs/open/close 需要的逻辑 |
| `handler/storage.rs` | 删除 |
| `handler/debug.rs` | 删除 |
| 性能优化 | snapshot 响应时间 < 200ms（非首次）；state_diff 计算 < 50ms |
| 持久化精简 | 去掉 workspace 相关持久化，简化为 session + browser 状态 |

**Phase 3 交付标准：**

- 无 deprecated 命令残留
- 代码量显著减少（预计删除 30-40% handler 代码）
- snapshot 响应时间 < 200ms（非首次）
- state_diff 计算开销 < 50ms
- 完整的 error code 覆盖（所有错误路径都有结构化返回）

### 实施顺序建议

Phase 1 内部推荐顺序：

1. 结构化 error 改造（影响所有命令，先做）
2. daemon 鉴权 token（安全基础，尽早加入）
3. Session 抽象层（BrowserContext 管理）+ config.toml limits 配置
4. connect 命令（发现 + 连接 Chrome + 创建 session）
5. setup 命令（交互式引导远程调试配置）
6. Chrome 崩溃检测（WebSocket close/error 监听 + 状态清理）
7. open 命令（创建 tab + session + 返回 snapshot）+ tab 数量检查
8. snapshot 命令（核心观察能力）
9. navigate 命令（基本导航）
10. act click/type/press（最常用的操作）
11. state_diff 计算（提升 act 返回质量）
12. tabs/close 命令（多 tab 支持）
13. session close/list 命令 + session 数量检查
14. migration cleanup（不新增 executable action aliases）

---

*文档版本：2026-07-01 v4（新增 bk setup 命令、Chrome 崩溃检测、Session 资源配额、daemon token 鉴权、disable_security 默认值、prompt injection 防护扩展）*
