# 需求文档

## 简介

browserkit（简称 `bk`）是一个基于 Chrome DevTools Protocol (CDP) 的浏览器控制 CLI 工具。采用 daemon 常驻架构，通过单一后台进程持有所有 CDP 连接，支持跨进程共享 Workspace。定位为 MCP tool / AI agent 的浏览器功能层，提供纯粹的浏览器操控能力。

本次重写基于现有 DESIGN.md 设计文档，参考 cdpkit-rs 的 CDP 协议能力和 Playwright 的浏览器自动化模式，对 browserkit 进行全面重构。

### 跨平台设计原则

本项目采用跨平台优先的设计原则，尽量避免平台特定的适配代码：
- 通信层使用 TCP（`127.0.0.1`）而非 Unix Socket，天然跨平台
- 进程管理使用跨平台的 Rust 标准库 API
- Chrome 可执行文件发现使用各平台已知安装路径硬编码（参考 Playwright 方式），属于少量可接受的平台特定代码

### 多项目并发使用设计原则

本工具的核心使用场景是多个项目同时/交叉调用 browserkit。设计上需要支持：
- **编程式/MCP 调用**：多个不同项目的 AI Agent 或工具同时调用，每次调用必须显式携带 workspace ID（`--ws <wid>`），不依赖任何全局状态
- **交互式 CLI 使用**：单用户在终端中简单使用，可通过 `bk ws use <wid>` 设置默认 workspace 作为便捷方式
- `~/.bk/current` 文件仅作为 CLI 便捷功能，不是推荐的编程式使用方式

### 术语命名说明：Workspace vs Session

在 cdpkit-rs 中，`session_id`（`Target.SessionId`）是 CDP 协议层的概念，用于将命令路由到特定的 Target（页面/标签页）。这是一个底层协议概念，由 `AttachToTarget` 返回。

browserkit 中原先的 `Session` 是一个业务层的隔离单元，基于 CDP `BrowserContext`，包含多个页面，提供 cookie/storage 隔离。这与 cdpkit-rs 的 `session_id` 是完全不同的概念。

为避免混淆，browserkit 将业务隔离单元重命名为 **Workspace**，明确区分：
- **Workspace**（browserkit 概念）：业务隔离单元，基于 CDP BrowserContext，包含多个 tab
- **session_id**（CDP 协议概念）：cdpkit-rs 中用于路由命令到特定 Target 的协议标识符

## 术语表

- **Daemon**：后台常驻进程，监听 TCP `127.0.0.1` 端口，管理所有浏览器连接和 Workspace
- **Browser**：Chrome 浏览器实例，通过 CDP 连接控制，可以是 daemon 自动启动（managed）或用户手动连接（unmanaged）
- **Browser_Finder**：Chrome 可执行文件发现模块，通过各平台已知安装路径硬编码查找 Chrome（参考 Playwright 的 registry 实现）
- **Workspace**：业务隔离单元，基于 CDP BrowserContext，提供独立的 cookie/storage 隔离。原名 Session，为避免与 cdpkit-rs 的 CDP session_id 概念冲突而重命名
- **Tab**：标签页，Workspace 内的页面单元，每个 Workspace 有一个 active_tab
- **CDP**：Chrome DevTools Protocol，浏览器远程调试协议
- **BrowserContext**：CDP 提供的浏览器上下文隔离机制
- **Protocol**：daemon 与 client 之间的 JSON 通信协议
- **Client**：通过 TCP 与 daemon 通信的客户端（CLI、MCP Server、AI Agent 等）
- **Element**：页面中的可交互元素，通过 index 标识
- **MCP**：Model Context Protocol，AI agent 与工具之间的通信协议
- **Health_Check**：daemon 提供的健康检查端点，用于验证 daemon 服务是否正常运行（区别于仅检测端口占用）

## 需求

### 需求 1：Daemon 生命周期管理

**用户故事：** 作为开发者，我希望 daemon 能以后台进程方式运行并自动管理生命周期，以便多个 client 可以共享浏览器连接。

#### 验收标准

1. WHEN 用户执行 `bk daemon start` 命令，THE Daemon SHALL 以后台进程方式启动，监听 TCP `127.0.0.1` 上的可用端口，并将端口号写入 `~/.bk/daemon.port` 文件
2. WHEN 用户执行 `bk daemon start` 且 daemon 已在运行时，THE Daemon SHALL 通过 Health_Check 验证已有 daemon 是否正常响应，若正常则返回启动成功信息（包含已有 daemon 的端口号）
3. WHEN 用户执行 `bk daemon start` 且 daemon 端口文件存在但 Health_Check 失败时，THE Daemon SHALL 清理残留的端口文件后重新启动
4. WHEN 用户执行 `bk daemon stop` 命令，THE Daemon SHALL 等待所有 pending 请求完成后退出，并清理端口文件
5. WHEN 用户执行 `bk daemon status` 命令，THE Daemon SHALL 通过 Health_Check 验证 daemon 是否正常运行，并返回当前运行状态，包含 PID、监听端口、浏览器连接数和 workspace 数
6. THE Daemon SHALL 提供 `ping` 命令作为 Health_Check 端点，返回 `{"ok":true,"data":{"status":"running"}}` 以验证 daemon 服务正常运行
7. THE Daemon SHALL 监听 TCP `127.0.0.1` 接受 client 连接，使用跨平台的 TCP 通信方式

### 需求 2：通信协议

**用户故事：** 作为开发者，我希望 daemon 和 client 之间有统一的 JSON 通信协议，以便 CLI、MCP Server 和 AI Agent 都能使用相同的接口。

#### 验收标准

1. THE Protocol SHALL 使用换行分隔的 JSON 格式进行请求和响应的序列化传输
2. THE Protocol SHALL 定义请求格式为 `{"cmd":"<command>","params":{...}}`，其中 cmd 为命令名称，params 为命令参数
3. THE Protocol SHALL 定义成功响应格式为 `{"ok":true,"data":{...}}`
4. THE Protocol SHALL 定义错误响应格式为 `{"ok":false,"error":"<error_message>"}`
5. WHEN 收到无法解析的 JSON 请求，THE Protocol_Handler SHALL 返回包含解析错误描述的错误响应
6. THE Protocol SHALL 支持流式响应模式，持续写入多行 JSON 直到 client 断开连接，用于 network monitor 和 cdp events 等持续输出命令
7. FOR ALL 有效的 Request 对象，序列化为 JSON 再反序列化 SHALL 产生等价的 Request 对象（往返一致性）
8. FOR ALL 有效的 Response 对象，序列化为 JSON 再反序列化 SHALL 产生等价的 Response 对象（往返一致性）

### 需求 3：浏览器生命周期管理

**用户故事：** 作为开发者，我希望 daemon 能自动管理 Chrome 浏览器的启动和连接，以便我无需手动管理浏览器进程。

#### 验收标准

1. WHEN 用户创建 workspace 且没有可用浏览器时，THE Daemon SHALL 自动启动一个 Chrome 实例，使用 9222-9322 范围内的可用端口
2. WHEN 自动启动 Chrome 时，THE Daemon SHALL 在 5 秒内等待 CDP 端点可用，超时则返回错误
3. WHEN 用户执行 `bk browser connect <host>` 命令，THE Daemon SHALL 连接到指定地址的已有浏览器实例，并标记为 unmanaged
4. WHEN 用户执行 `bk browser list` 命令，THE Daemon SHALL 返回所有浏览器的信息列表，包含 host、managed 状态、workspace 数和 PID
5. WHEN 用户执行 `bk browser disconnect <host>` 命令，THE Daemon SHALL 断开与指定浏览器的连接但不关闭浏览器进程
6. WHEN managed 浏览器的最后一个 workspace 被关闭，THE Daemon SHALL 自动关闭该浏览器进程并清理相关资源
7. THE Daemon SHALL 对同一 Chrome 实例只建立一个 WebSocket 连接，多个 workspace 复用该连接
8. THE Browser_Finder SHALL 按以下优先级查找 Chrome 可执行文件：依次检查 chrome stable、chrome beta、chrome dev、chrome canary 的已知安装路径，返回第一个存在的可执行文件
9. THE Browser_Finder SHALL 使用各平台硬编码的已知安装路径查找 Chrome：macOS 检查 `/Applications/Google Chrome.app/Contents/MacOS/Google Chrome` 等路径；Linux 检查 `/opt/google/chrome/chrome` 等路径；Windows 检查 `%LOCALAPPDATA%`、`%PROGRAMFILES%`、`%PROGRAMFILES(X86)%` 下的 `\Google\Chrome\Application\chrome.exe` 等路径
10. IF Chrome 可执行文件在所有已知路径中均未找到，THEN THE Browser_Finder SHALL 返回明确的错误信息，列出已检查的路径列表

### 需求 4：Workspace 管理

**用户故事：** 作为开发者，我希望能创建相互隔离的 workspace 来管理不同的业务场景，以便多个项目的浏览器操作互不干扰，并支持多个项目同时/交叉调用。

#### 验收标准

1. WHEN 用户执行 `bk ws new` 命令，THE Daemon SHALL 创建一个基于 CDP BrowserContext 的新 workspace，生成 4 位随机 hex 作为 workspace ID（简称 wid），并自动创建第一个 tab 作为 active_tab
2. WHEN 用户执行 `bk ws new --host <host>` 命令，THE Daemon SHALL 在指定浏览器上创建 workspace
3. WHEN 用户执行 `bk ws new --label <name>` 命令，THE Daemon SHALL 为 workspace 设置业务标签
4. WHEN 用户执行 `bk ws list` 命令，THE Daemon SHALL 返回所有 workspace 的信息列表，包含 wid、host、label、tab 数和创建时间
5. WHEN 用户执行 `bk ws close <wid>` 命令，THE Daemon SHALL 关闭该 workspace 的所有 tab 并删除对应的 BrowserContext
6. WHEN 用户执行 `bk ws use <wid>` 命令，THE CLI SHALL 将该 wid 写入 `~/.bk/current` 作为 CLI 交互模式下的默认 workspace（仅用于单用户 CLI 便捷操作）
7. WHEN 用户执行 `bk ws info <wid>` 命令，THE Daemon SHALL 返回该 workspace 的详细信息，包含所有 tab 列表
8. THE Daemon SHALL 为每个 workspace 记录 created_at 和 last_active 时间戳，每次操作更新 last_active
9. THE Daemon SHALL 定期清理超过 30 分钟无活动的 workspace
10. WHEN 用户提供的 wid 参数为前缀时，THE CLI SHALL 自动匹配完整的 workspace ID（如输入 `a3` 匹配 `a3f2`）
11. THE CLI SHALL 支持 `--ws <wid>`（或 `-w <wid>`）作为全局选项，用于所有需要 workspace 上下文的命令（goto、click、type、shot、eval 等页面级操作）
12. WHEN 用户省略 `--ws` 参数且 `~/.bk/current` 文件存在时，THE CLI SHALL 使用 `~/.bk/current` 中记录的默认 workspace 作为回退
13. IF 用户省略 `--ws` 参数且 `~/.bk/current` 文件不存在或为空，THEN THE CLI SHALL 返回明确的错误信息，提示需要通过 `--ws <wid>` 指定 workspace 或通过 `bk ws use <wid>` 设置默认值

### 需求 5：Tab 管理

**用户故事：** 作为开发者，我希望能在 workspace 内管理多个标签页，以便同时操作多个页面。

#### 验收标准

1. WHEN 用户执行 `bk tab new <wid> [url]` 命令，THE Daemon SHALL 在该 workspace 的 BrowserContext 中创建新标签页，生成 4 位随机 hex 作为 tab ID
2. WHEN 用户执行 `bk tab list <wid>` 命令，THE Daemon SHALL 返回该 workspace 的所有标签页信息，包含 tid、url、title 和 active 状态
3. WHEN 用户执行 `bk tab switch <wid> <tid>` 命令，THE Daemon SHALL 更新该 workspace 的 active_tab 为指定 tab
4. WHEN 用户执行 `bk tab close <wid> <tid>` 命令且关闭的是 active_tab，THE Daemon SHALL 自动切换到第一个剩余 tab
5. WHEN 命令未指定 `--tab` 参数时，THE Daemon SHALL 对 workspace 的 active_tab 执行操作

### 需求 6：页面导航

**用户故事：** 作为开发者，我希望能控制页面的导航行为，以便浏览和操作目标网页。

#### 验收标准

1. WHEN 用户执行 `bk goto <url>` 命令，THE Daemon SHALL 使用 CDP Page.navigate 导航到指定 URL，并等待页面加载完成
2. WHEN 用户执行 `bk reload` 命令，THE Daemon SHALL 刷新当前页面
3. WHEN 用户执行 `bk nav back` 命令，THE Daemon SHALL 执行浏览器后退操作
4. WHEN 用户执行 `bk nav forward` 命令，THE Daemon SHALL 执行浏览器前进操作
5. WHEN 用户执行 `bk url` 命令，THE Daemon SHALL 返回当前页面的 URL
6. WHEN 用户执行 `bk title` 命令，THE Daemon SHALL 返回当前页面的标题
7. WHEN 用户执行 `bk nav wait` 命令，THE Daemon SHALL 等待页面加载完成后返回

### 需求 7：页面捕获

**用户故事：** 作为开发者，我希望能捕获页面的截图、PDF 和 HTML 内容，以便记录和分析页面状态。

#### 验收标准

1. WHEN 用户执行 `bk shot` 命令，THE Daemon SHALL 使用 CDP Page.captureScreenshot 捕获当前视口截图并返回 base64 编码数据
2. WHEN 用户执行 `bk shot --full-page` 命令，THE Daemon SHALL 先获取页面完整高度，再捕获全页截图
3. WHEN 用户执行 `bk shot --selector <css>` 命令，THE Daemon SHALL 捕获指定 CSS 选择器匹配元素的截图
4. WHEN 用户执行 `bk shot -o <file>` 命令，THE Daemon SHALL 将截图保存到指定文件路径
5. WHEN 用户执行 `bk pdf` 命令，THE Daemon SHALL 使用 CDP Page.printToPDF 生成 PDF 文件
6. WHEN 用户执行 `bk html` 命令，THE Daemon SHALL 返回当前页面的完整 HTML 内容
7. WHEN 用户执行 `bk html --selector <css>` 命令，THE Daemon SHALL 返回指定 CSS 选择器匹配元素的 HTML 内容

### 需求 8：页面状态获取

**用户故事：** 作为 AI Agent，我希望能获取页面上所有可交互元素的结构化信息，以便通过 index 定位和操作元素。

#### 验收标准

1. WHEN 用户执行 `bk page state` 命令，THE Daemon SHALL 返回页面中所有可交互元素的列表，每个元素包含 index、tag、text、坐标（x, y）、尺寸（width, height）信息
2. THE Daemon SHALL 识别以下可交互元素类型：a、button、input、textarea、select、具有 role="button" 属性的元素、具有 onclick 属性的元素
3. THE Daemon SHALL 过滤掉宽度或高度为 0 的不可见元素
4. WHEN 用户执行 `bk page state --screenshot` 命令，THE Daemon SHALL 在返回元素列表的同时附带页面截图的 base64 数据
5. WHEN 用户执行 `bk page search <text>` 命令，THE Daemon SHALL 在页面文本中搜索匹配内容并返回匹配位置

### 需求 9：页面交互操作

**用户故事：** 作为 AI Agent，我希望能通过 index 或坐标对页面元素执行点击、输入等操作，以便自动化完成页面交互。

#### 验收标准

1. WHEN 用户执行 `bk click --index <n>` 命令，THE Daemon SHALL 通过 page state 获取第 n 个元素的坐标，计算中心点后使用 CDP Input.dispatchMouseEvent 执行点击
2. WHEN 用户执行 `bk click --x <n> --y <n>` 命令，THE Daemon SHALL 在指定坐标执行点击操作
3. THE Daemon SHALL 按照 mouseMoved → mousePressed → mouseReleased 的顺序发送鼠标事件以模拟真实点击
4. WHEN 用户执行 `bk type --index <n> <text>` 命令，THE Daemon SHALL 先点击目标元素聚焦，然后逐字符发送 keyDown/keyUp 事件输入文本
5. WHEN 用户执行 `bk scroll [up|down]` 命令，THE Daemon SHALL 执行页面滚动操作
6. WHEN 用户执行 `bk act select --index <n> <value>` 命令，THE Daemon SHALL 在下拉框中选择指定值
7. WHEN 用户执行 `bk act hover --index <n>` 命令，THE Daemon SHALL 将鼠标移动到指定元素上方
8. WHEN 用户执行 `bk act focus --index <n>` 命令，THE Daemon SHALL 聚焦到指定元素
9. IF 指定的 index 超出元素列表范围，THEN THE Daemon SHALL 返回明确的错误信息

### 需求 10：JavaScript 执行

**用户故事：** 作为开发者，我希望能在页面上下文中执行 JavaScript 代码，以便进行自定义操作和数据提取。

#### 验收标准

1. WHEN 用户执行 `bk eval <expression>` 命令，THE Daemon SHALL 使用 CDP Runtime.evaluate 执行 JavaScript 表达式并返回序列化结果
2. WHEN 用户执行 `bk js await <expression>` 命令，THE Daemon SHALL 执行异步 JavaScript 表达式并等待 Promise 完成后返回结果
3. WHEN 用户执行 `bk js file <script.js>` 命令，THE Daemon SHALL 读取指定文件内容并执行
4. IF JavaScript 执行过程中发生异常，THEN THE Daemon SHALL 返回包含异常信息的错误响应

### 需求 11：Storage 管理

**用户故事：** 作为开发者，我希望能管理页面的 cookie 和 localStorage，以便保存和恢复登录态。

#### 验收标准

1. WHEN 用户执行 `bk storage cookies get` 命令，THE Daemon SHALL 使用 CDP Network.getCookies 返回当前页面的所有 cookie（JSON 格式）
2. WHEN 用户执行 `bk storage cookies set <json>` 命令，THE Daemon SHALL 设置指定的 cookie
3. WHEN 用户执行 `bk storage cookies clear` 命令，THE Daemon SHALL 清除当前页面的所有 cookie
4. WHEN 用户执行 `bk storage local get <key>` 命令，THE Daemon SHALL 返回指定 localStorage 键的值
5. WHEN 用户执行 `bk storage local set <key> <value>` 命令，THE Daemon SHALL 设置指定 localStorage 键值对
6. WHEN 用户执行 `bk storage export` 命令，THE Daemon SHALL 导出完整的 storage 状态（cookie + localStorage）为 JSON
7. WHEN 用户执行 `bk storage import <state.json>` 命令，THE Daemon SHALL 从 JSON 文件导入 storage 状态
8. FOR ALL 有效的 storage 状态，执行 export 后再 import SHALL 恢复等价的 storage 状态（往返一致性）

### 需求 12：网络监控

**用户故事：** 作为开发者，我希望能监控和控制页面的网络请求，以便调试和分析网络行为。

#### 验收标准

1. WHEN 用户执行 `bk network monitor` 命令，THE Daemon SHALL 启用 CDP Network.enable 并以流式方式实时输出网络请求和响应事件
2. WHEN 用户执行 `bk network har <url>` 命令，THE Daemon SHALL 导航到指定 URL 并录制 HAR 格式的网络日志
3. WHEN 用户执行 `bk network block <pattern>` 命令，THE Daemon SHALL 屏蔽匹配指定 pattern 的网络请求
4. WHEN 用户执行 `bk network unblock <pattern>` 命令，THE Daemon SHALL 取消对指定 pattern 的网络请求屏蔽

### 需求 13：原始 CDP 命令

**用户故事：** 作为高级用户，我希望能直接发送任意 CDP 命令，以便使用 browserkit 未封装的 CDP 功能。

#### 验收标准

1. WHEN 用户执行 `bk cdp send <method> [params_json]` 命令，THE Daemon SHALL 将指定的 CDP 方法和参数发送到浏览器并返回原始响应
2. WHEN 用户执行 `bk cdp events [--filter <pattern>]` 命令，THE Daemon SHALL 以流式方式输出 CDP 事件，支持按 pattern 过滤
3. IF CDP 命令执行失败，THEN THE Daemon SHALL 返回包含 CDP 错误码和错误信息的错误响应

### 需求 14：CLI 命令行界面

**用户故事：** 作为开发者，我希望有一个简洁易用的 CLI 界面，支持快捷别名和灵活的参数格式，以便在单用户交互和多项目编程式调用场景下都能高效地控制浏览器。

#### 验收标准

1. THE CLI SHALL 使用 clap 库解析命令行参数，支持子命令和全局选项
2. THE CLI SHALL 支持 `--format text`（默认）和 `--format json` 两种输出格式
3. THE CLI SHALL 为常用命令提供快捷别名：`bk new`（ws new）、`bk ls`（ws list）、`bk rm`（ws close）、`bk goto`（nav goto）、`bk shot`（capture screenshot）等
4. WHEN CLI 无法连接到 daemon 时，THE CLI SHALL 自动启动 daemon 并重试一次
5. WHEN 命令执行失败时，THE CLI SHALL 打印错误信息并以非零退出码退出
6. THE CLI SHALL 支持一次性快捷命令（`bk open`、`bk shot <url>`、`bk pdf <url>`、`bk fetch <url>`），自动创建 workspace、执行操作、关闭 workspace
7. THE CLI SHALL 支持 `--ws <wid>`（或 `-w <wid>`）作为全局选项，所有需要 workspace 上下文的命令（goto、click、type、shot、eval、page state 等）均通过该选项指定目标 workspace
8. WHEN 用户未提供 `--ws` 选项时，THE CLI SHALL 回退到 `~/.bk/current` 中记录的默认 workspace；若默认 workspace 也不存在，则返回错误提示

### 需求 15：状态持久化与恢复

**用户故事：** 作为开发者，我希望 daemon 重启后能恢复之前的 workspace 元数据，以便不丢失工作状态。

#### 验收标准

1. THE Daemon SHALL 将浏览器状态持久化到 `~/.bk/browsers.json`，包含 host、managed 状态和 PID
2. THE Daemon SHALL 将 workspace 元数据持久化到 `~/.bk/workspaces.json`
3. WHEN Daemon 重启时，THE Daemon SHALL 从持久化文件恢复 workspace 元数据，并重新建立 CDP 连接
4. IF 持久化文件损坏或不可读，THEN THE Daemon SHALL 记录警告日志并以空状态启动

### 需求 16：并发安全与错误处理

**用户故事：** 作为开发者，我希望 daemon 能安全地处理多个并发 client 请求，并提供清晰的错误信息。

#### 验收标准

1. THE Daemon SHALL 使用 `Arc<RwLock<>>` 保护 DaemonState，确保多个并发请求的数据安全
2. WHEN 多个 client 同时发送请求时，THE Daemon SHALL 为每个连接创建独立的 tokio task 并行处理
3. IF workspace 不存在，THEN THE Daemon SHALL 返回 "workspace not found: <wid>" 错误
4. IF 浏览器连接断开，THEN THE Daemon SHALL 返回连接失败错误并清理相关状态
5. THE Daemon SHALL 使用 tracing 库记录结构化日志，包含请求处理、错误和状态变更信息

### 需求 17：MCP 集成

**用户故事：** 作为 AI Agent 开发者，我希望 browserkit 提供 MCP 工具接口，以便多个 AI Agent 能通过标准协议并发控制各自的浏览器 workspace。

#### 验收标准

1. THE MCP_Server SHALL 作为 daemon 的一个独立 client，通过 TCP 与 daemon 通信
2. THE MCP_Server SHALL 提供以下 MCP tools：browser_workspace_new、browser_navigate、browser_click、browser_type、browser_scroll、browser_go_back、browser_get_state、browser_get_html、browser_screenshot、browser_tab_list、browser_tab_switch、browser_tab_close、browser_tab_new、browser_workspace_list、browser_workspace_close
3. THE MCP_Server SHALL 将 MCP tool 调用转换为对应的 daemon 请求并返回结果
4. THE MCP_Server SHALL 输出 JSON 格式的响应，便于 AI Agent 解析
5. THE MCP_Server SHALL 要求所有 workspace 级别的 tool 调用（navigate、click、type、scroll、go_back、get_state、get_html、screenshot 等）必须显式传入 workspace_id 参数，不依赖 `~/.bk/current` 全局状态
6. THE MCP_Server SHALL 支持多个 AI Agent 同时调用不同的 workspace，每个调用通过 workspace_id 隔离
