# 实现计划：browserkit 重写

## 概述

基于需求文档和设计文档，将 browserkit 从现有实现重写为基于 TCP 通信、Workspace 隔离的浏览器自动化 CLI 工具。采用增量开发方式，从核心基础设施开始，逐步构建上层功能，每个任务都建立在前一个任务的基础上。

## 任务

- [x] 1. 项目基础结构与核心类型定义
  - [x] 1.1 创建项目目录结构和模块骨架
    - 按照设计文档的模块结构创建 `src/error.rs`、`src/client.rs`、`src/browser/`、`src/daemon/`、`src/workspace/`、`src/page/`、`src/mcp/` 目录和 `mod.rs` 文件
    - 在 `src/lib.rs` 中声明所有模块并 re-export
    - 在 `Cargo.toml` 中添加依赖：`cdpkit`、`tokio`、`clap`、`serde`、`serde_json`、`tracing`、`tracing-subscriber`、`thiserror`、`rand`
    - _需求: 14.1_

  - [x] 1.2 实现 `BkError` 错误类型
    - 在 `src/error.rs` 中定义完整的 `BkError` 枚举，包含设计文档中所有错误变体
    - 实现 `From<BkError>` 到 `Response` 的转换
    - _需求: 16.3, 16.4_

  - [x] 1.3 实现通信协议（`Request`/`Response`）
    - 在 `src/daemon/protocol.rs` 中定义 `Request` 和 `Response` 结构体，实现 `Serialize`/`Deserialize`
    - 实现 `Response::ok(data)` 和 `Response::err(msg)` 便捷构造方法
    - 实现换行分隔 JSON 的读写辅助函数（`read_request`/`write_response`）
    - _需求: 2.1, 2.2, 2.3, 2.4_

  - [x] 1.4 编写协议序列化属性测试
    - **Property 1: 协议请求序列化往返一致性** — 随机生成 Request，序列化再反序列化应等价
    - **Property 2: 协议响应序列化往返一致性** — 随机生成 Response（成功/错误），序列化再反序列化应等价
    - **Property 3: 协议消息结构正确性** — 验证 JSON 结构包含正确字段
    - **Property 4: 无效 JSON 输入产生错误响应** — 随机生成非法字符串，验证返回错误
    - **验证: 需求 2.2, 2.3, 2.4, 2.5, 2.7, 2.8**

  - [x] 1.5 实现核心数据模型
    - 在 `src/daemon/state.rs` 中定义 `DaemonState`、`Browser`
    - 在 `src/workspace/mod.rs` 中定义 `Workspace`
    - 在 `src/page/mod.rs` 中定义 `Tab`、`ElementInfo`
    - 实现 4 位随机 hex ID 生成函数 `generate_hex_id()`
    - _需求: 4.1, 5.1_

  - [x] 1.6 编写 ID 生成格式属性测试
    - **Property 8: ID 生成格式正确性** — 多次调用 `generate_hex_id()`，验证结果匹配 `^[0-9a-f]{4}$`
    - **验证: 需求 4.1, 5.1**

- [x] 2. 检查点 — 确保所有测试通过
  - 确保所有测试通过，如有问题请询问用户。

- [x] 3. BrowserFinder 与浏览器启动
  - [x] 3.1 实现 `BrowserFinder`
    - 在 `src/browser/finder.rs` 中实现 `BrowserFinder::find()` 和 `known_paths()`
    - 按 stable → beta → dev → canary 优先级查找 Chrome
    - 支持 macOS、Linux、Windows 三平台的已知安装路径
    - 未找到时返回 `BkError::BrowserNotFound`，列出已检查路径
    - _需求: 3.8, 3.9, 3.10_

  - [x] 3.2 编写 BrowserFinder 优先级属性测试
    - **Property 5: BrowserFinder 优先级正确性** — 随机生成存在/不存在的路径组合，验证返回优先级最高的路径
    - **验证: 需求 3.8**

  - [x] 3.3 实现 Chrome 进程启动器
    - 在 `src/browser/launcher.rs` 中实现 Chrome 启动逻辑
    - 启动参数：`--remote-debugging-port=PORT`、`--user-data-dir=~/.bk/chrome-{port}/`、`--no-first-run`、`--no-default-browser-check`
    - 端口范围 9222-9322，自动选择可用端口
    - 实现 5 秒超时等待 CDP 端点可用
    - _需求: 3.1, 3.2_

  - [x] 3.4 实现 `Browser` 结构体与连接管理
    - 在 `src/browser/mod.rs` 中实现 `Browser` 结构体
    - 实现 `connect(host)` 方法，使用 `cdpkit CDP::connect` 建立 WebSocket 连接
    - 实现连接复用逻辑：同一 host 共享一个 `Arc<CDP>`
    - _需求: 3.3, 3.7_

  - [x] 3.5 编写连接复用属性测试
    - **Property 6: 连接复用不变量** — 多个 Workspace 连接同一 host 时，DaemonState 中只存在一个 Browser 条目
    - **验证: 需求 3.7**

- [x] 4. Daemon TCP Server 与命令处理框架
  - [x] 4.1 实现 Daemon TCP Server
    - 在 `src/daemon/server.rs` 中实现 TCP server，绑定 `127.0.0.1:0`（随机端口）
    - 为每个连接创建独立的 tokio task
    - 实现换行分隔 JSON 的请求读取和响应写入
    - 使用 `Arc<RwLock<DaemonState>>` 共享状态
    - _需求: 1.1, 1.7, 16.1, 16.2_

  - [x] 4.2 实现命令分发处理器
    - 在 `src/daemon/handler.rs` 中实现命令分发逻辑
    - 根据 `Request.cmd` 字段路由到对应的处理函数
    - 实现 `ping` 命令返回 `{"ok":true,"data":{"status":"running"}}`
    - 实现 `daemon.status` 命令返回 PID、端口、浏览器数、workspace 数
    - 实现 `daemon.stop` 命令优雅关闭
    - _需求: 1.4, 1.5, 1.6, 2.5_

  - [x] 4.3 实现 Daemon 启动/停止生命周期
    - 在 `src/daemon/mod.rs` 中实现 daemon 启动流程：检查端口文件 → ping 验证 → 启动/复用
    - 启动时写入端口号到 `~/.bk/daemon.port`
    - 停止时清理端口文件
    - 处理残留端口文件的清理逻辑
    - _需求: 1.1, 1.2, 1.3_

- [x] 5. 检查点 — 确保所有测试通过
  - 确保所有测试通过，如有问题请询问用户。

- [x] 6. Workspace 管理
  - [x] 6.1 实现 `ws.new` 命令处理
    - 在 handler 中实现 workspace 创建逻辑
    - 调用 CDP `Target.createBrowserContext`（`disposeOnDetach: true`）创建隔离上下文
    - 调用 CDP `Target.createTarget`（`url: "about:blank"`）创建首个 tab
    - 调用 CDP `Target.attachToTarget`（`flatten: true`）获取 CDP session_id
    - 启用核心 CDP 域：`Page.enable`、`Page.setLifecycleEventsEnabled`、`Runtime.enable`、`Network.enable`
    - 支持 `--host` 和 `--label` 参数
    - 无可用浏览器时自动启动 Chrome
    - 记录 `created_at` 和 `last_active` 时间戳
    - _需求: 4.1, 4.2, 4.3, 4.8, 3.1_

  - [x] 6.2 实现 `ws.list`、`ws.info`、`ws.close` 命令处理
    - `ws.list`：返回所有 workspace 信息（wid、host、label、tab 数、创建时间）
    - `ws.info`：返回指定 workspace 详情，包含 tab 列表
    - `ws.close`：关闭所有 tab（`Target.closeTarget`）→ 删除 BrowserContext（`Target.disposeBrowserContext`）→ 从 state 移除
    - managed 浏览器最后一个 workspace 关闭时自动清理浏览器
    - _需求: 4.4, 4.5, 4.7, 3.6_

  - [x] 6.3 实现 wid 前缀匹配解析
    - 实现 `resolve_wid()` 函数：唯一匹配返回完整 wid，多匹配返回歧义错误，无匹配返回未找到错误
    - 所有需要 wid 的命令处理中调用此函数
    - _需求: 4.10_

  - [x] 6.4 编写 Workspace 管理属性测试
    - **Property 7: Managed 浏览器自动清理** — 所有 workspace 关闭后 managed 浏览器应被移除
    - **Property 9: Workspace 列表完整性** — ws.list 返回所有未关闭 workspace
    - **Property 10: Workspace 关闭后不可访问** — 关闭后操作应返回 not found 错误
    - **Property 11: last_active 时间戳单调递增** — 每次操作后 last_active 不减小
    - **Property 12: wid 前缀匹配正确性** — 唯一前缀返回完整 wid，多匹配返回歧义，无匹配返回未找到
    - **验证: 需求 3.6, 4.4, 4.5, 4.8, 4.10, 16.3**

  - [x] 6.5 实现 Workspace 超时清理
    - 实现定期检查（tokio interval），清理超过 30 分钟无活动的 workspace
    - _需求: 4.9_

- [x] 7. Tab 管理
  - [x] 7.1 实现 `tab.new`、`tab.list`、`tab.switch`、`tab.close` 命令处理
    - `tab.new`：在 workspace 的 BrowserContext 中创建新 tab（`Target.createTarget` + `AttachToTarget` + 启用核心 CDP 域），支持可选 url 参数
    - `tab.list`：返回 workspace 的所有 tab 信息（tid、url、title、active 状态）
    - `tab.switch`：更新 workspace 的 `active_tab`
    - `tab.close`：关闭 tab（`Target.closeTarget`），若关闭的是 active_tab 则自动切换到第一个剩余 tab
    - 未指定 `--tab` 参数时默认使用 active_tab
    - _需求: 5.1, 5.2, 5.3, 5.4, 5.5_

  - [x] 7.2 编写 Tab 管理属性测试
    - **Property 13: Tab 列表完整性** — tab.list 返回所有未关闭 tab
    - **Property 14: Tab 切换正确性** — tab.switch 后 active_tab 等于指定 tid
    - **Property 15: 关闭 active_tab 后自动切换** — 关闭 active_tab 后自动切换到第一个剩余 tab
    - **Property 16: 默认 Tab 解析** — 未指定 --tab 时使用 active_tab
    - **验证: 需求 5.2, 5.3, 5.4, 5.5**

- [x] 8. 检查点 — 确保所有测试通过
  - 确保所有测试通过，如有问题请询问用户。

- [x] 9. 页面导航
  - [x] 9.1 实现导航命令处理
    - 在 `src/page/navigation.rs` 中实现导航操作
    - `goto`：使用 CDP `Page.navigate` 导航到 URL，等待 `Page.lifecycleEvent`（load）
    - `reload`：使用 CDP `Page.reload`
    - `back`/`forward`：使用 `Page.getNavigationHistory` 获取历史 → `Page.navigateToHistoryEntry` 导航
    - `nav.url`：通过 `Runtime.evaluate("window.location.href")` 获取 URL
    - `nav.title`：通过 `Runtime.evaluate("document.title")` 获取标题
    - `nav.wait`：监听 `Page.lifecycleEvent` 等待 load 事件
    - _需求: 6.1, 6.2, 6.3, 6.4, 6.5, 6.6, 6.7_

- [x] 10. 页面捕获（截图、PDF、HTML）
  - [x] 10.1 实现截图命令处理
    - 在 `src/page/capture.rs` 中实现截图逻辑
    - 视口截图：`Page.captureScreenshot`
    - 全页面截图：`Page.getLayoutMetrics` 获取完整尺寸 → `Page.captureScreenshot`（`captureBeyondViewport: true`）
    - 元素截图：`DOM.scrollIntoViewIfNeeded` → `DOM.getContentQuads` → `Page.captureScreenshot`（clip 参数）
    - 支持 `-o <file>` 保存到文件
    - 返回 base64 编码数据
    - _需求: 7.1, 7.2, 7.3, 7.4_

  - [x] 10.2 实现 PDF 和 HTML 命令处理
    - PDF：使用 CDP `Page.printToPDF`
    - HTML：通过 `Runtime.evaluate("document.documentElement.outerHTML")` 获取完整 HTML
    - HTML + selector：通过 `Runtime.evaluate` 执行 `document.querySelector(selector).outerHTML`
    - _需求: 7.5, 7.6, 7.7_

- [x] 11. 页面状态与交互
  - [x] 11.1 实现 `page.state` 命令处理
    - 在 `src/page/state.rs` 中实现页面状态获取
    - 通过 `Runtime.evaluate` 注入 JS 脚本遍历 DOM，查询所有可交互元素（a、button、input、textarea、select、role="button"、onclick）
    - 返回 `ElementInfo` 列表（index、tag、text、x、y、width、height、href、placeholder）
    - 过滤掉 width 或 height 为 0 的不可见元素
    - 支持 `--screenshot` 参数附带截图
    - _需求: 8.1, 8.2, 8.3, 8.4_

  - [x] 11.2 实现 `page.search` 命令处理
    - 通过 `Runtime.evaluate` 注入 JS 在页面文本中搜索匹配内容
    - _需求: 8.5_

  - [x] 11.3 编写页面状态属性测试
    - **Property 17: 页面状态元素过滤** — page.state 返回的所有元素 width > 0 且 height > 0
    - **验证: 需求 8.3**

  - [x] 11.4 实现交互命令处理
    - 在 `src/page/interaction.rs` 中实现交互操作
    - `click --index`：通过 page state 获取元素坐标 → `DOM.scrollIntoViewIfNeeded` → `Input.dispatchMouseEvent`（mouseMoved → mousePressed → mouseReleased）
    - `click --x --y`：直接发送 `Input.dispatchMouseEvent` 三连
    - `type`：先点击元素聚焦 → `Input.insertText` 批量输入
    - `scroll`：`Input.dispatchMouseEvent`（mouseWheel，deltaY）
    - `act.select`：通过 `Runtime.evaluate` 设置 select 元素值
    - `act.hover`：发送 `Input.dispatchMouseEvent`（mouseMoved）
    - `act.focus`：通过 `Runtime.evaluate` 聚焦元素
    - index 越界时返回 `BkError::ElementIndexOutOfRange`
    - _需求: 9.1, 9.2, 9.3, 9.4, 9.5, 9.6, 9.7, 9.8, 9.9_

- [x] 12. 检查点 — 确保所有测试通过
  - 确保所有测试通过，如有问题请询问用户。

- [x] 13. JavaScript 执行
  - [x] 13.1 实现 JS 执行命令处理
    - `eval`：使用 CDP `Runtime.evaluate` 执行表达式，返回序列化结果
    - `js.await`：使用 `Runtime.evaluate`（`awaitPromise: true`）执行异步表达式
    - `js.file`：读取文件内容后通过 `Runtime.evaluate` 执行
    - JS 异常时返回包含异常信息的错误响应
    - _需求: 10.1, 10.2, 10.3, 10.4_

- [x] 14. Storage 管理
  - [x] 14.1 实现 Storage 命令处理
    - Cookie 操作通过 browser session（`session_id = None`）发送，使用 `Storage` 域 + `browserContextId` 隔离
    - `storage.cookies.get`：`Storage.getCookies`（`browserContextId`）
    - `storage.cookies.set`：`Storage.setCookies`（`cookies`, `browserContextId`）
    - `storage.cookies.clear`：`Storage.clearCookies`（`browserContextId`）
    - `storage.local.get/set`：通过 `Runtime.evaluate` 操作 `window.localStorage`
    - `storage.export`：导出 cookie + localStorage 为 JSON
    - `storage.import`：从 JSON 导入 storage 状态
    - _需求: 11.1, 11.2, 11.3, 11.4, 11.5, 11.6, 11.7_

  - [x] 14.2 编写 Storage 往返属性测试
    - **Property 18: Storage 导出/导入往返一致性** — export 后 import 应恢复等价状态
    - **验证: 需求 11.8**

- [x] 15. 网络监控与原始 CDP
  - [x] 15.1 实现网络监控命令处理
    - `network.monitor`：启用 `Network.enable`，监听 `Network.requestWillBeSent`/`Network.responseReceived`/`Network.loadingFinished`/`Network.loadingFailed` 事件，以流式 JSON 输出
    - `network.har`：导航到 URL 并录制 HAR 格式网络日志
    - `network.block`：使用 `Network.setBlockedURLs` 屏蔽匹配 pattern 的请求
    - `network.unblock`：取消屏蔽
    - _需求: 12.1, 12.2, 12.3, 12.4_

  - [x] 15.2 实现原始 CDP 命令处理
    - `cdp.send`：将 method 和 params 直接通过 `cdp.send()` 发送，返回原始响应
    - `cdp.events`：以流式方式输出 CDP 事件，支持 filter pattern 过滤
    - CDP 错误时返回包含错误码和错误信息的响应
    - _需求: 13.1, 13.2, 13.3_

- [x] 16. 状态持久化
  - [x] 16.1 实现状态持久化与恢复
    - 在 `src/daemon/persist.rs` 中实现持久化逻辑
    - 定义 `PersistedBrowser`、`PersistedWorkspace`、`PersistedTab` 结构体
    - 将浏览器状态写入 `~/.bk/browsers.json`
    - 将 workspace 元数据写入 `~/.bk/workspaces.json`
    - daemon 启动时从文件恢复状态，重新建立 CDP 连接
    - 文件损坏时记录警告日志，以空状态启动
    - 在状态变更时（workspace 创建/关闭、browser 连接/断开）触发持久化
    - _需求: 15.1, 15.2, 15.3, 15.4_

  - [x] 16.2 编写状态持久化属性测试
    - **Property 19: 状态持久化往返一致性** — 随机生成 DaemonState 元数据，持久化后恢复应等价
    - **验证: 需求 15.3**

- [x] 17. 检查点 — 确保所有测试通过
  - 确保所有测试通过，如有问题请询问用户。

- [x] 18. CLI 命令行界面
  - [x] 18.1 实现 CLI 命令解析（clap derive）
    - 在 `src/main.rs` 中定义 `Cli` 结构体和 `Command` 枚举
    - 实现全局选项：`--ws <wid>`（`-w`）、`--format text|json`
    - 实现所有子命令：`daemon`、`browser`、`ws`、`tab`、`nav`、`page`、`act`、`js`、`storage`、`network`、`cdp`
    - 实现快捷别名：`new`→`ws new`、`ls`→`ws list`、`rm`→`ws close`、`goto`→`nav goto`、`shot`→`screenshot`、`click`、`type`、`eval`、`scroll`、`open`
    - _需求: 14.1, 14.3, 14.6_

  - [x] 18.2 实现 TCP Client
    - 在 `src/client.rs` 中实现 TCP client
    - 实现 `connect_or_start()`：尝试连接 → 失败则自动启动 daemon → 轮询 ping 等待就绪（5 秒超时）→ 重试连接
    - 读取 `~/.bk/daemon.port` 获取端口号
    - 实现请求发送和响应接收（换行分隔 JSON）
    - 实现流式响应读取（用于 network.monitor、cdp.events）
    - _需求: 14.4_

  - [x] 18.3 实现 CLI 输出格式化与 workspace 解析
    - 实现 `--format text` 和 `--format json` 两种输出格式
    - 实现 `--ws` 全局选项解析：优先使用命令行参数 → 回退到 `~/.bk/current` → 无则报错
    - 实现 `ws use <wid>` 命令写入 `~/.bk/current`
    - 错误响应时打印到 stderr 并以退出码 1 退出
    - _需求: 14.2, 14.5, 14.7, 14.8, 4.6, 4.11, 4.12, 4.13_

  - [x] 18.4 编写 CLI 属性测试
    - **Property 20: CLI 别名等价性** — 快捷别名命令生成的 Request 与完整命令等价
    - **Property 21: 错误响应退出码** — daemon 返回 ok:false 时 CLI 以非零退出码退出
    - **验证: 需求 14.3, 14.5**

  - [x] 18.5 实现一次性快捷命令
    - `bk open <url>`：自动创建 workspace → goto → 返回 wid（不关闭）
    - `bk shot <url>`：创建 workspace → goto → 截图 → 关闭 workspace
    - `bk pdf <url>`：创建 workspace → goto → 生成 PDF → 关闭 workspace
    - `bk fetch <url>`：创建 workspace → goto → 获取 HTML → 关闭 workspace
    - _需求: 14.6_

- [x] 19. 检查点 — 确保所有测试通过
  - 确保所有测试通过，如有问题请询问用户。

- [x] 20. MCP Server 集成
  - [x] 20.1 实现 MCP Server
    - 在 `src/mcp/mod.rs` 中实现 MCP Server 入口
    - 在 `src/mcp/tools.rs` 中定义所有 MCP tool 及其到 daemon 请求的映射
    - 实现 tool 列表：`browser_workspace_new`、`browser_navigate`、`browser_click`、`browser_type`、`browser_scroll`、`browser_go_back`、`browser_get_state`、`browser_get_html`、`browser_screenshot`、`browser_tab_list`、`browser_tab_switch`、`browser_tab_close`、`browser_tab_new`、`browser_workspace_list`、`browser_workspace_close`
    - 通过 TCP 与 daemon 通信，将 MCP tool 调用转换为 daemon Request
    - 所有 workspace 级别 tool 必须显式传入 `workspace_id`，缺失时返回错误
    - 输出 JSON 格式响应
    - _需求: 17.1, 17.2, 17.3, 17.4, 17.5, 17.6_

  - [x] 20.2 编写 MCP 映射属性测试
    - **Property 22: MCP tool 到 daemon 请求映射正确性** — 每个 MCP tool 调用生成正确的 daemon Request
    - **Property 23: MCP workspace 级别 tool 必须携带 workspace_id** — 缺失 workspace_id 时返回错误
    - **验证: 需求 17.3, 17.4, 17.5**

- [x] 21. Browser 管理命令
  - [x] 21.1 实现 `browser.connect`、`browser.list`、`browser.disconnect` 命令处理
    - `browser.connect`：连接到指定 host 的已有浏览器，标记为 unmanaged
    - `browser.list`：返回所有浏览器信息（host、managed、workspace 数、PID）
    - `browser.disconnect`：断开连接但不关闭浏览器进程，清理关联状态
    - _需求: 3.3, 3.4, 3.5_

- [x] 22. 最终检查点 — 确保所有测试通过
  - 确保所有测试通过，如有问题请询问用户。
  - 验证所有 17 个需求的验收标准均已覆盖。
  - 验证所有 23 个正确性属性均有对应的属性测试任务。

## 备注

- 每个任务引用了具体的需求编号，确保可追溯性
- 所有属性测试任务均为必须任务
- 检查点任务确保增量验证
- 属性测试使用 `proptest` 库，验证通用正确性属性
- 单元测试验证具体示例和边界情况
- 需要实际 Chrome 浏览器的测试作为集成测试运行
