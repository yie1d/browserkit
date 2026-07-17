# browserkit 项目分析报告

> 生成时间：2026-03-23
>
> 状态：历史 v1 架构快照，已被 session-only runtime 迁移取代。本文保留 workspace/text-json-tsv 等旧口径用于考古，不代表当前 CLI、daemon route、配置或持久化 schema。当前定位以 README、docs/REDESIGN.md、docs/ROADMAP.md 和 `bk --help` 为准：browserkit 是构建在 cdpkit-rs 之上的 persistent browser runtime，`bk` 和 daemon 是入口。
>
> 当前破坏性迁移结论：workspace 命令、变量、字段和 daemon route 已移除；schema v2 state 会先备份再迁移到 schema v3；`bk status` 暴露 migration metadata，清理命令响应暴露 `cleanup_errors`。

## 1. 项目概述

本节是 2026-03-23 的历史快照。快照当时描述的 workspace 状态、text/json/tsv 输出和旧 route 已不再是当前产品 contract。当前实现由 daemon 维护 browser/session/tab 状态和本地 JSON 协议，底层 CDP 能力来自 cdpkit-rs。

| 属性 | 值 |
|------|-----|
| 语言 | Rust (Edition 2021) |
| 版本 | 0.1.0 |
| 许可证 | MIT |
| 二进制名 | `bk` |
| 核心依赖 | cdpkit 0.3.0, tokio 1, clap 4 |
| 源码文件 | 32 个 (.rs) |
| 源码行数 | ~6,800 行 |
| 测试文件 | 13 个 (.rs) |
| 测试行数 | ~3,700 行 |
| 测试用例 | 178 个 (133 同步 + 45 异步) |

## 2. 功能清单

### 2.1 守护进程管理 (Daemon)

| 功能 | CLI 命令 | 协议命令 | 说明 |
|------|---------|---------|------|
| 启动守护进程 | `bk daemon start` | - | 前台启动，其他命令会自动后台启动 |
| 停止守护进程 | `bk daemon stop` | `daemon.stop` | 优雅关闭，清理端口文件 |
| 查看状态 | `bk daemon status` | `daemon.status` | 端口、PID、运行时间、请求数 |
| 综合状态 | `bk status` | - | 聚合 daemon + browser + workspace 信息 |

### 2.2 浏览器管理 (Browser)

| 功能 | CLI 命令 | 协议命令 | 说明 |
|------|---------|---------|------|
| 连接浏览器 | `bk browser connect <host>` | `browser.connect` | 连接已有 Chrome 实例 |
| 列出浏览器 | `bk browser list` | `browser.list` | 列出所有已连接浏览器 |
| 断开浏览器 | `bk browser disconnect <host>` | `browser.disconnect` | 断开指定浏览器连接 |
| 自动发现 | - | - | 按平台搜索已安装的 Chrome/Chromium |
| 自动启动 | - | - | 无可用浏览器时自动启动 Chrome |

### 2.3 工作区管理 (Workspace)

| 功能 | CLI 命令 | 协议命令 | 说明 |
|------|---------|---------|------|
| 创建工作区 | `bk ws new` | `ws.new` | legacy workspace 入口 |
| 列出工作区 | `bk ws list` | `ws.list` | legacy workspace 入口 |
| 工作区详情 | `bk ws info [wid]` | `ws.info` | 查看工作区详细信息 |
| 关闭工作区 | `bk ws close <wid>` | `ws.close` | 关闭并清理工作区 |
| 设置默认 | `bk ws use <wid>` | `ws.use` | 设置默认工作区 |
| 查询默认 | - | `ws.default` | 获取当前默认工作区 ID |
| 前缀匹配 | 所有接受 wid 的命令 | - | 支持 wid 前缀简写 |

### 2.4 标签页管理 (Tab)

| 功能 | CLI 命令 | 协议命令 | 说明 |
|------|---------|---------|------|
| 新建标签页 | `bk tab new [url]` | `tab.new` | 创建新标签页 |
| 列出标签页 | `bk tab list` | `tab.list` | 列出工作区内所有标签页 |
| 切换标签页 | `bk tab switch <tid>` | `tab.switch` | 切换活动标签页 |
| 关闭标签页 | `bk tab close <tid>` | `tab.close` | 关闭指定标签页 |

### 2.5 页面导航 (Navigation)

| 功能 | CLI 命令 | 协议命令 | 说明 |
|------|---------|---------|------|
| 导航到 URL | `bk navigate <url>` | `navigate` | 页面导航 |
| 刷新页面 | `bk navigate --reload` | `navigate` | 刷新当前页面 |
| 后退 | `bk navigate --back` | `navigate` | 浏览器历史后退 |
| 前进 | `bk navigate --forward` | `navigate` | 浏览器历史前进 |
| 获取 URL | `bk evaluate "location.href"` | `evaluate` | 获取当前页面 URL |
| 获取标题 | `bk evaluate "document.title"` | `evaluate` | 获取当前页面标题 |
| 等待加载 | `bk wait --idle` | `wait` | 等待页面加载完成 |

### 2.6 页面交互 (Interaction)

| 功能 | CLI 命令 | 协议命令 | 说明 |
|------|---------|---------|------|
| 点击元素 | `bk click --index N` | `act.click` | 按索引点击交互元素 |
| 坐标点击 | `bk click --x X --y Y` | `act.click` | 按坐标点击 |
| 输入文本 | `bk type --index N "text"` | `act.type` | 点击聚焦后批量输入 |
| 滚动页面 | `bk scroll [direction]` | `act.scroll` | 上/下/左/右滚动 |
| 悬停元素 | `bk hover --index N` | `act.hover` | 鼠标悬停 |
| 聚焦元素 | `bk focus --index N` | `act.focus` | 聚焦元素 |
| 选择下拉 | `bk select --index N "val"` | `act.select` | 选择下拉框选项 |

### 2.7 页面检查 (Page Inspection)

| 功能 | CLI 命令 | 协议命令 | 说明 |
|------|---------|---------|------|
| 交互元素列表 | `bk snapshot` | `snapshot` | 发现页面上所有可交互元素 |
| 带页面文本状态 | `bk snapshot --full` | `snapshot` | 返回更完整的页面状态 |
| 页面搜索 | `bk page search "text"` | `page.search` | 在页面文本中搜索 |

### 2.8 页面捕获 (Capture)

| 功能 | CLI 命令 | 协议命令 | 说明 |
|------|---------|---------|------|
| 视口截图 | `bk screenshot` | `screenshot` | PNG 格式截图 |
| 全页截图 | `bk screenshot --full-page` | `screenshot` | 完整可滚动页面截图 |
| 元素截图 | `bk screenshot --selector "#id"` | `screenshot` | CSS 选择器定位元素截图 |
| 保存截图 | `bk screenshot --output file.png` | `screenshot` | 保存到文件 |
| 一键截图 | 已移除 | - | 使用 `bk open` + `bk screenshot` |
| 生成 PDF | `bk pdf -o file.pdf` | `page.pdf` | 页面导出为 PDF |
| 一键 PDF | `bk pdf <url> -o file.pdf` | - | 一次性 PDF 生成 |
| 获取 HTML | `bk html` | `page.html` | 获取完整页面 HTML |
| 元素 HTML | `bk html --selector "sel"` | `page.html` | 获取指定元素 HTML |

### 2.9 JavaScript 执行

| 功能 | CLI 命令 | 协议命令 | 说明 |
|------|---------|---------|------|
| 执行表达式 | `bk evaluate "expr"` | `evaluate` | 异步执行 JS |
| 同步执行 | 已移除 | - | 使用 `bk evaluate` |
| 执行文件 | `bk evaluate --file script.js` | `evaluate` | 读取并执行 JS 文件 |

### 2.10 存储管理 (Storage)

| 功能 | CLI 命令 | 协议命令 | 说明 |
|------|---------|---------|------|
| 获取 Cookies | `bk storage cookies get` | `storage.cookies.get` | 获取所有 Cookie |
| 设置 Cookies | `bk storage cookies set '<json>'` | `storage.cookies.set` | 批量设置 Cookie |
| 清除 Cookies | `bk storage cookies clear` | `storage.cookies.clear` | 清除所有 Cookie |
| 获取 localStorage | `bk storage local get <key>` | `storage.local.get` | 获取 localStorage 值 |
| 设置 localStorage | `bk storage local set <k> <v>` | `storage.local.set` | 设置 localStorage 值 |
| 导出存储状态 | `bk storage export` | `storage.export` | 导出完整存储状态 |
| 导入存储状态 | `bk storage import file.json` | `storage.import` | 从文件导入存储状态 |

### 2.11 调试与网络 (Debug / Network)

| 功能 | CLI 命令 | 协议命令 | 说明 |
|------|---------|---------|------|
| 网络监控 | `bk debug monitor` | `network.monitor` | 流式输出网络请求事件 |
| HAR 录制 | `bk debug har <url>` | `network.har` | 导航并录制 HAR（entries 暂为空） |
| 请求拦截 | `bk debug block "pattern"` | `network.block` | 按 URL 模式拦截请求 |
| 取消拦截 | `bk debug unblock` | `network.unblock` | 取消请求拦截 |
| 原始 CDP | `bk debug cdp <method> [params]` | `cdp.send` | 发送原始 CDP 命令 |
| CDP 事件流 | `bk debug events --filter "X"` | `cdp.events` | 流式监听 CDP 事件 |

### 2.12 便捷命令 (One-shot / Aliases)

| 功能 | CLI 命令 | 说明 |
|------|---------|------|
| 打开 URL | `bk open <url>` | 创建工作区 + 导航，保持工作区存活 |
| 抓取 HTML | `bk fetch <url>` | 一次性：创建→导航→获取 HTML→关闭 |
| Shell 补全 | `bk completions <shell>` | 生成 bash/zsh/fish 补全脚本 |

### 2.13 输出格式

| 格式 | 标志 | 说明 |
|------|------|------|
| Text | `--format text` | 人类可读（默认） |
| JSON | `--format json` | 结构化 JSON |
| TSV | `--format tsv` | Tab 分隔，适合管道处理 |

### 2.14 配置系统

- 配置文件路径：`~/.bk/config.toml`
- 支持 daemon 配置（超时、清理间隔、Chrome 路径、headless 模式、安全标志）
- 支持资源限制（最大工作区数、每工作区最大标签页数、JS 超时）
- 所有字段可选，缺失时使用合理默认值

### 2.15 状态持久化

- 持久化路径：`~/.bk/`
- 持久化文件：`browsers.json`、`workspaces.json`、`default_ws`、`daemon.port`
- 重启后自动重连浏览器、恢复工作区和标签页 CDP 会话
- 原子写入（先写 `.tmp` 再 rename）
- 防抖写入（500ms 静默窗口合并多次写入请求）

## 3. 架构分析

### 3.1 整体架构

```
┌─────────────────────────────────────────────────────┐
│  bk CLI  /  任意 TCP 客户端                          │
└──────────────────────┬──────────────────────────────┘
                       │  换行分隔 JSON (TCP)
┌──────────────────────▼──────────────────────────────┐
│                   bk daemon                         │
│  ┌─────────────┐  ┌──────────────┐  ┌───────────┐  │
│  │  workspaces │  │   browsers   │  │  persist  │  │
│  │  (DashMap)  │  │  (DashMap)   │  │  (async)  │  │
│  └─────────────┘  └──────────────┘  └───────────┘  │
└──────────────────────┬──────────────────────────────┘
                       │  CDP WebSocket
┌──────────────────────▼──────────────────────────────┐
│              Chrome / Chromium                       │
└─────────────────────────────────────────────────────┘
```

采用经典的 **客户端-守护进程-浏览器** 三层架构：

- **CLI 层**：clap 解析命令行参数，转换为 JSON 请求发送给 daemon
- **Daemon 层**：TCP 服务器，管理所有状态，路由请求到对应 handler
- **Browser 层**：通过 cdpkit 与 Chrome 建立 WebSocket CDP 连接

### 3.2 模块划分

```
src/
├── main.rs              # CLI 入口 (1,271 行) — clap 定义 + 命令分发
├── lib.rs               # 库根，导出所有模块
├── client.rs            # TCP 客户端 + 自动启动 daemon 逻辑
├── config.rs            # 配置文件加载
├── error.rs             # 统一错误类型 BkError
├── browser/
│   ├── finder.rs        # Chrome 可执行文件发现（跨平台）
│   ├── launcher.rs      # Chrome 进程启动与端口管理
│   └── mod.rs           # CDP 连接复用
├── daemon/
│   ├── mod.rs           # daemon 生命周期（启动/停止/端口文件）
│   ├── state.rs         # DaemonState 全局状态（DashMap + 原子计数器）
│   ├── server.rs        # TCP 服务器 + 工作区过期清理
│   ├── persist.rs       # 异步防抖状态持久化
│   ├── protocol.rs      # 换行分隔 JSON 协议
│   └── handler/         # 命令处理器（每个功能组一个文件）
│       ├── mod.rs       # 请求路由（47 个命令）
│       ├── common.rs    # 共享工具（上下文解析、handler 宏）
│       ├── workspace.rs, tab.rs, nav.rs, page.rs
│       ├── action.rs, js.rs, storage.rs
│       ├── browser.rs, network.rs, debug.rs
│       └── daemon.rs
├── page/
│   ├── navigation.rs    # 导航操作（带重试和指数退避等待）
│   ├── interaction.rs   # 交互操作（点击、输入、滚动等）
│   ├── capture.rs       # 截图、PDF、HTML 捕获
│   ├── state.rs         # 页面元素发现（JS 注入）
│   └── mod.rs           # Tab、ElementInfo、SearchMatch 类型定义
└── workspace/mod.rs     # Workspace 类型定义
```

### 3.3 通信协议

采用自定义的 **换行分隔 JSON** (NDJSON) 协议：

- 请求格式：`{"cmd": "命令名", "params": {...}}\n`
- 成功响应：`{"ok": true, "data": {...}}\n`
- 错误响应：`{"ok": false, "error": "错误信息"}\n`
- 流式响应：多行连续 JSON（用于 network.monitor、cdp.events 等）
- 请求大小限制：1MB（防 DoS）

### 3.4 并发模型

- **DashMap**：无锁并发读写，浏览器和工作区各用一个 DashMap
- **parking_lot::Mutex**：同步互斥锁，用于 default_wid（低竞争场景）
- **AtomicU64**：无锁计数器，用于请求计数
- **tokio::sync::Mutex**：异步互斥锁，用于序列化 Chrome 启动（防止并发 ws.new 启动多个 Chrome）
- **mpsc channel**：持久化信号通道，非阻塞 try_send

### 3.5 工作区解析优先级

1. `--ws` 标志 / `BK_WS` 环境变量
2. daemon 默认工作区（`ws.use` 设置）
3. 仅有一个工作区时自动选择
4. 报错并给出提示

## 4. 代码质量分析

### 4.1 优点

**架构设计**
- 三层架构职责清晰，CLI 层纯粹做参数解析和格式化，daemon 层管理状态和业务逻辑，page 层封装 CDP 操作
- handler 按功能组拆分为独立文件，每个文件职责单一
- `handler!` 宏消除了 handler 函数中重复的 `match Ok/Err` 样板代码
- 协议层（protocol.rs）与业务逻辑完全解耦

**Rust 惯用写法**
- 使用 `thiserror` 派生错误类型，错误变体语义清晰
- 使用 `DashMap` 替代 `RwLock<HashMap>`，避免了粗粒度锁
- `Browser` 实现了 `Drop` trait 自动清理 Chrome 子进程
- 编译期断言 `DaemonState: Send + Sync`（`static_assertions`）
- 合理使用 `Arc` 共享 CDP 连接，多个工作区复用同一浏览器连接

**健壮性**
- 导航操作带重试机制（瞬态连接错误自动重试一次）
- 页面加载等待使用指数退避轮询（50ms → 100ms → 200ms → 500ms 封顶）
- 持久化使用原子写入（write tmp + rename），防止进程崩溃导致文件损坏
- 持久化使用防抖（500ms 静默窗口），避免高频写入阻塞请求处理
- Chrome 启动时持有 TcpListener 占位端口，防止 TOCTOU 竞态
- 请求大小限制 1MB，防止恶意客户端耗尽内存
- 工作区过期清理时二次检查 `last_active`，避免误删刚被激活的工作区

**测试覆盖**
- 178 个测试用例，测试代码占总代码量约 35%
- 单元测试覆盖核心逻辑：协议序列化、状态管理、配置解析、元素交互计算
- 集成测试覆盖完整流程：daemon 启动/停止、多工作区隔离、标签页管理
- 属性测试（property-based testing）覆盖：协议往返、持久化往返、CLI 参数解析、浏览器发现

### 4.2 问题与改进建议

**[严重] main.rs 过于庞大**
- `main.rs` 有 1,271 行，包含了所有 CLI 定义、命令分发、输出格式化
- 建议拆分为：`cli.rs`（clap 定义）、`dispatch.rs`（命令分发）、`output.rs`（输出格式化）

**[严重] 交互元素选择器硬编码且重复**
- `'a, button, input, textarea, select, [role="button"], [onclick]'` 这个选择器在 `state.rs`、`interaction.rs` 的多个函数中重复出现
- 如果需要调整选择器（比如增加 `[role="link"]`），需要同时修改多处
- 建议提取为常量或共享的 JS 片段

**[警告] HAR 录制功能未完成**
- README 中标注 `stub: entries always empty`，HAR 录制只是占位实现
- 建议在 CLI help 中明确标注为实验性功能，或暂时移除

**[警告] 缺少 graceful shutdown 的完整实现**
- `daemon.stop` 发送 shutdown 信号后，已连接的客户端可能收不到完整响应
- 工作区关闭时的 CDP 清理是 best-effort（忽略错误），但没有超时保护

**[建议] JS 注入缺少版本化或校验**
- 页面状态发现、搜索、交互都依赖注入的 JS 代码
- 如果页面的 CSP (Content Security Policy) 禁止 eval，所有功能都会静默失败
- 建议在 JS 执行失败时给出更明确的错误提示

**[建议] 缺少日志级别的运行时调整**
- 当前日志通过 `RUST_LOG` 环境变量控制，daemon 启动后无法动态调整
- 可以考虑增加 `daemon.log-level` 命令

## 5. 安全性分析

### 5.1 做得好的方面

- 守护进程仅监听 `127.0.0.1`，不暴露到网络
- 请求大小限制 1MB，防止内存耗尽攻击
- JS 字符串注入使用 `serde_json::to_string` + `JSON.parse` 双重转义，防止 JS 注入
- CSS 选择器同样使用 JSON 转义后在 JS 侧 parse，避免选择器注入
- 原子文件写入防止状态文件损坏

### 5.2 潜在风险

**[严重] `disable_security` 默认为 true**
- 默认启动 Chrome 时传递 `--ignore-certificate-errors` 和 `--disable-web-security`
- 这意味着默认情况下 HTTPS 证书验证被禁用，跨域限制被移除
- 虽然对自动化场景有便利性，但如果用户不知情地访问恶意网站，可能导致安全问题
- 建议：默认值改为 `false`，或在首次使用时提示用户

**[警告] 无认证机制**
- daemon TCP 端口无任何认证，本机任何进程都可以连接并控制浏览器
- 在多用户系统上，其他用户的进程也可能连接到 daemon
- 建议：考虑增加 token 认证或 Unix socket 替代 TCP

**[警告] 端口文件权限未限制**
- `~/.bk/daemon.port` 文件权限取决于 umask，可能被其他用户读取
- 建议：创建文件时显式设置 `0600` 权限

**[建议] `cdp.send` 命令无限制**
- 原始 CDP 命令可以执行任意 CDP 方法，包括 `Browser.close` 等破坏性操作
- 这是设计上的权衡（调试工具需要灵活性），但应在文档中明确警告

## 6. 性能分析

### 6.1 优点

- **DashMap** 提供了细粒度的并发访问，多个客户端操作不同工作区时互不阻塞
- **CDP 连接复用**：同一 Chrome 实例的多个工作区共享一个 WebSocket 连接
- **防抖持久化**：避免每次状态变更都触发磁盘 I/O
- **spawn_blocking**：持久化的文件 I/O 在专用线程执行，不阻塞 tokio 运行时
- **指数退避**：页面加载等待从 50ms 开始递增，快速页面几乎无延迟，慢速页面不会产生过多 CDP 流量
- **Chrome 启动锁**：防止并发 `ws.new` 请求启动多个 Chrome 进程

### 6.2 潜在瓶颈

**[警告] 页面状态发现的 JS 注入**
- `get_page_state` 每次调用都注入完整的 JS 代码遍历 DOM
- 对于 DOM 节点非常多的页面（如大型表格），可能产生明显延迟
- 元素文本截断为 100 字符是合理的优化

**[警告] 全页截图的内存消耗**
- `capture_full_page` 获取完整页面尺寸后一次性截图
- 对于非常长的页面（如无限滚动），可能产生巨大的 PNG 数据
- 建议：增加最大尺寸限制

**[建议] 工作区清理的遍历开销**
- `cleanup_expired_workspaces` 遍历所有工作区检查过期
- 当前实现对于合理数量的工作区（几十个）完全没问题
- 如果未来支持大量工作区，可以考虑使用优先队列按过期时间排序

## 7. 可维护性分析

### 7.1 优点

- **模块化清晰**：handler 按功能组拆分，新增命令只需在对应文件中添加函数并在 mod.rs 注册路由
- **统一错误处理**：`BkError` 枚举覆盖所有错误场景，`handler!` 宏自动转换为 Response
- **类型安全**：充分利用 Rust 类型系统，`Request`/`Response` 有明确的序列化/反序列化定义
- **代码注释充分**：几乎每个公开函数都有文档注释，关键设计决策有行内注释说明"为什么"
- **测试即文档**：测试用例命名清晰（如 `cleanup_boundary_exactly_30_minutes_not_expired`），可以作为行为规范参考

### 7.2 改进空间

**[建议] 缺少 CHANGELOG**
- 项目处于 0.1.0 版本，建议从现在开始维护 CHANGELOG

**[建议] 缺少 CI 配置**
- 没有看到 GitHub Actions 或其他 CI 配置
- 建议增加：`cargo test`、`cargo clippy`、`cargo fmt --check`

**[建议] 集成测试依赖真实 Chrome**
- `integration_tests.rs` 中的部分测试需要真实 Chrome 实例
- 建议标记为 `#[ignore]` 或使用 feature flag 控制，避免在无 Chrome 环境下 CI 失败

## 8. 总结

browserkit 是一个设计良好的浏览器自动化工具，架构清晰、代码质量高、测试覆盖充分。核心亮点：

1. **三层架构**解耦了 CLI、daemon、浏览器控制，支持多客户端并发访问
2. **DashMap + 原子操作**的并发模型避免了粗粒度锁
3. **47 个协议命令**覆盖了浏览器自动化的主要场景
4. **防抖持久化 + 原子写入**保证了状态的可靠性
5. **178 个测试用例**（含属性测试）提供了良好的质量保障

主要改进方向：
1. 拆分 `main.rs`，消除交互选择器的重复
2. `disable_security` 默认值应改为 `false`
3. 增加 daemon 认证机制
4. 补充 CI 配置和 CHANGELOG
5. 完成 HAR 录制功能或标记为实验性
