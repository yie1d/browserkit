---
name: bk-browser
display_name: bk 浏览器运行时
description: 用 bk CLI 连接 browserkit 持久浏览器运行时，控制用户已运行的 Chrome（保留登录态/cookie），执行导航、交互、截图、数据抽取等操作。
allowed-tools: Bash(bk *) Bash(bk.exe *) Bash(tasklist*) Bash(pgrep*) Bash(test -f*)
metadata:
  code: bk-browser
  skillCode: bk-browser
version: 0.2.0
tags: [浏览器自动化, 网页交互, RPA, AI-Agent, 数据抽取]
---

# 用 bk CLI 操作 browserkit 持久浏览器运行时

`bk` 是 browserkit runtime 的默认 CLI client，通过后台 daemon 维持持久 CDP 连接。所有输出为 JSON。

> **必须使用用户自己的浏览器（接管模式）。** bk 连接用户已运行的 Chrome，在用户可见窗口操作，复用已登录的 cookie 和会话，无需重新登录。**禁止启动隔离的无头浏览器。**

---

## 前置：确认 bk 可用并连接浏览器

### 第一步：确认 bk 已安装

```bash
bk status
```

- **成功（返回连接状态）** → 跳到第二步
- **失败/命令未找到** → 提示用户从 https://github.com/yie1d/browserkit/releases 下载对应平台的压缩包，解压后将 `bk`（Windows 为 `bk.exe`）放到 PATH 目录，再重试

### 第二步：连接用户的 Chrome

```bash
bk connect
```

- **成功** → 直接进入工作流。
- **失败** → 按下方排查流程处理。

---

**connect 失败排查流程：**

**① 检查 Chrome 是否在运行**

- Windows：`tasklist | findstr chrome`
- macOS/Linux：`pgrep -l -i chrome`

**Chrome 在运行 → 检查远程调试是否已开启**

检查 `DevToolsActivePort` 文件是否存在：

| 系统 | 路径 |
|---|---|
| Windows | `%LOCALAPPDATA%\Google\Chrome\User Data\DevToolsActivePort` |
| macOS | `~/Library/Application Support/Google/Chrome/DevToolsActivePort` |
| Linux | `~/.config/google-chrome/DevToolsActivePort` |

- **文件不存在** → 运行 `bk setup` 引导用户开启远程调试，或手动引导：
  > "需要在 Chrome 中开启远程调试，只需设置一次，重启后保留。请在 Chrome 地址栏打开 `chrome://inspect/#remote-debugging`，勾选"开启远程调试"，完成后告诉我。"
  用户开启后再执行 `bk connect`。

- **文件存在但 connect 仍失败** → 用 admin command 直接连接文件中的 endpoint：
  ```bash
  # 文件第一行=端口，第二行=ws路径
  bk browser connect "ws://127.0.0.1:<端口><ws路径>"
  ```

**Chrome 没有进程 → 检查 Edge 是否在运行**

- Windows：`tasklist | findstr msedge`
- macOS/Linux：`pgrep -l -i "microsoft edge"`

**Edge 在运行 → 检查 Edge 的 DevToolsActivePort**

| 系统 | 路径 |
|---|---|
| Windows | `%LOCALAPPDATA%\Microsoft\Edge\User Data\DevToolsActivePort` |
| macOS | `~/Library/Application Support/Microsoft Edge/DevToolsActivePort` |
| Linux | `~/.config/microsoft-edge/DevToolsActivePort` |

- 文件存在 → 读取端口和 ws 路径，用 admin command 连接：
  ```bash
  bk browser connect "ws://127.0.0.1:<端口><ws路径>"
  ```
- 文件不存在 → 引导用户开启 Edge 远程调试：
  > "需要在 Edge 中开启远程调试。请在 Edge 地址栏打开 `edge://inspect/#remote-debugging`，勾选"开启远程调试"，完成后告诉我。"

**Chrome 和 Edge 都没有进程 →**
> "未检测到 Chrome 或 Edge。请先打开 Chrome，完成后告诉我继续。"

---

**接管模式连接成功后**，若触发授权弹窗（首次连接）：

1. 执行连接命令 → 弹出授权框，命令可能超时退出（正常现象）
2. 告知用户：
   > "浏览器弹出了授权对话框，请点击"允许"，完成后告诉我。"
3. 用户点允许后再执行一次 `bk connect`
4. 授权完成后后续会话不再弹窗


---

## 核心工作流

1. **连接**：`bk connect`（自动发现并连接用户 Chrome，幂等）
2. **选择目标**：在 default session 操作现有用户 tab 时用 `bk attach <唯一 URL/title 片段>`；隔离 session 或新 tab 用 `bk open <url>`
3. **观察**：`bk snapshot` —— 返回带 ref 的可交互元素 + 页面文本（**每次操作前必须先 snapshot**）
4. **交互**：用 ref 操作（`bk act click --ref 42`、`bk act type --ref 42 --text "文本"`）
5. **验证**：再次 `bk snapshot` 或 `bk wait` + `bk snapshot` 确认结果
6. **结束**：`bk close` detach/关闭当前 tab，或 `bk session close` 清理当前 session

daemon 在命令之间保持连接，无需重复 connect。

---

## 常用命令速查

```bash
# 连接与状态
bk connect                            # 连接浏览器（幂等）
bk status                             # 查看连接状态
bk tabs                               # 列出当前 session 追踪的 tab
bk attach <unique-url-or-title>        # 接管现有用户 tab（close 时只 detach）

# 导航
bk navigate <url>                     # 打开 URL
bk navigate --back                    # 后退
bk navigate --forward                 # 前进
bk navigate --reload                  # 刷新

# 页面状态（操作前必须先执行）
bk snapshot                           # 元素列表 + 页面文本 + viewport
bk snapshot --no-page-text            # 只要元素列表（更快）
bk snapshot --full                    # 包含所有元素（不截断）
bk snapshot --wait networkidle        # 等网络空闲后再快照

# 交互（ref = snapshot 返回的 backendNodeId）
bk act click --ref <N>                # 点击元素
bk act click --x <X> --y <Y>         # 坐标点击
bk act type --ref <N> --text "text"   # 输入文本（默认替换）
bk act type --ref <N> --text "text" --append  # 追加模式
bk act press --keys Enter             # 按键
bk act press --keys Control+a         # 组合键

# 标签页
bk open <url>                         # 新标签页打开 URL
bk close                              # 关闭当前 tab
bk close --target <targetId>          # 关闭指定 tab

# 等待
bk wait --selector ".cls"             # 等待元素可见
bk wait --text "成功"                 # 等待文本出现
bk wait --text-gone "Loading"         # 等待文本消失
bk wait --idle                        # 等待网络空闲
bk wait --url "/dashboard"            # 等待 URL 匹配
bk wait --fn "expr"                   # 等待 JS truthy
bk wait --time 2000                   # 固定等待

# JavaScript
bk evaluate "document.title"
bk evaluate "await fetch('/api').then(r=>r.json())"
bk evaluate --file script.js

# 截图
bk screenshot --output page.png
bk screenshot --full-page --output full.png

# Session 管理
bk session list                       # 列出所有 session
bk session close                      # 关闭当前 session
bk session cookies get                # 获取 cookies
bk session cookies clear              # 清除 cookies
```

完整参数说明见 [`references/commands.md`](references/commands.md)。

---

## 元素定位：两种方式

| 方式 | 命令示例 | 适用场景 |
|------|---------|---------|
| **ref（稳定引用）** | `bk act click --ref 42` | 默认方式，从 `bk snapshot` 输出获取 |
| **坐标** | `bk act click --x 300 --y 200` | canvas、SVG、无法定位的元素 |

**ref**（backendNodeId）在节点未被移除时始终稳定，不会因 DOM 增减元素而漂移。

---

## 注意事项（重要）

1. **确认操作目标**：要操作用户已经打开的 tab，先用唯一 URL/title 片段 `bk attach <pattern>`；`bk tabs` 只列出当前 session 已追踪的 tab。

2. **永远先 `bk snapshot`**：每次操作前先拿到元素 ref，不要猜 ref

3. **不要截断 snapshot 输出**：完整元素列表才能正确定位；截断会导致操作失败率大幅上升

4. **`bk act type` 默认替换**：默认清空后输入。需要追加时加 `--append`

5. **ref 是稳定的**：ref（backendNodeId）在节点未被移除时不会变化，多步操作无需重复 snapshot（除非页面导航或大量 DOM 变化）

6. **接管模式不关用户 tab**：attached tab 的 `bk close` / `bk session close` 只 detach；`bk open` 创建的 owned tab 会被关闭

7. **screenshot 消耗 token**：整页截图 token 消耗极大，优先用 `bk snapshot` 文本方式

8. **daemon 自动启动**：第一次运行任何命令会自动启动 daemon，无需手动启动

9. **输出永远 JSON**：所有命令输出 JSON 格式，无 --format 选项

---

## 常用操作示例

### 登录表单

```bash
bk connect
bk navigate https://example.com/login
bk snapshot
# 假设 email ref=42, password ref=43, 登录按钮 ref=44
bk act type --ref 42 --text "user@example.com"
bk act type --ref 43 --text "mypassword"
bk act click --ref 44
bk wait --url "/dashboard"
bk snapshot
```

### 等待动态内容加载

```bash
bk navigate https://example.com/data
bk wait --selector "#data-table" --timeout 10000
bk snapshot
```

### 抓取数据

```bash
bk navigate https://example.com/list
bk wait --idle
bk evaluate "Array.from(document.querySelectorAll('.item-title')).map(e => e.textContent.trim())"
```

### 多标签页操作

```bash
bk open https://site-a.com
bk open https://site-b.com
bk tabs
# 用 --target 切换操作对象
bk snapshot --target <targetId-a>
bk snapshot --target <targetId-b>
```

### 接管已登录的 Chrome

```bash
bk connect
bk attach "唯一的标题或 URL 片段"
bk snapshot
bk navigate https://app.example.com/dashboard
```

### 键盘操作

```bash
bk act press --keys Escape
bk act press --keys Control+a
bk act press --keys Shift+Enter
```

---

## 排错

| 问题 | 解决方法 |
|------|---------|
| connect 失败 | 运行 `bk setup` 引导开启远程调试 |
| session not found | `bk session list` 查看，`bk connect` 重新连接 |
| 找不到元素 | 页面可能未完全加载，先 `bk wait --idle` 再 `bk snapshot` |
| daemon 异常 | `bk session close`，再重新 `bk connect` |
| ref 对不上 | 页面发生了导航或大量 DOM 变化，重新 `bk snapshot` |
| attach 匹配多个 tab | 换更唯一的 URL/title 片段，或显式 `--target <targetId>` |

## 清理

```bash
bk close                 # attached 用户 tab 只 detach；owned tab 会关闭
bk session close         # 清理当前 session
```

---

## 参考

- 完整命令参数速查：[`references/commands.md`](references/commands.md)
- bk 项目主页：https://github.com/yie1d/browserkit
