# bk v2 命令速查表

> 基于 `bk --help` 实际输出，所有命令均经过验证。输出永远为 JSON。

---

## 全局选项

每条命令都可以加：

| 选项 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `--session <NAME>` | string | — | 指定 session 名称（或设 `BK_SESSION` 环境变量） |
| `--target <ID>` | string | — | 指定目标 tab（targetId） |
| `--timeout <MS>` | integer | `30000` | 超时毫秒数 |
| `--no-state-diff` | flag | — | 跳过 act 响应中的 state_diff |
| `--focus` | flag | — | 将目标 tab 带到前台 |
| `-h` | flag | — | 简短帮助 |
| `--help` | flag | — | 完整帮助 |
| `--version` | flag | — | 打印版本号 |

```bash
# 指定 session
bk --session my-session snapshot

# 指定 tab
bk --target <targetId> snapshot

# 环境变量
export BK_SESSION=my-session
bk snapshot
```

---

## setup

一次性设置 Chrome 远程调试（交互式引导）。

```
Usage: bk setup
```

无特有参数。运行后交互式引导用户开启 Chrome 远程调试。

```bash
bk setup
```

---

## connect

连接到浏览器（幂等，多次调用安全）。

```
Usage: bk connect [OPTIONS]
```

无特有参数，使用全局选项。自动发现并连接本地 Chrome。

```bash
bk connect
bk connect --session work
```

---

## snapshot

获取页面状态：可交互元素列表 + 页面文本 + viewport 信息。

```
Usage: bk snapshot [OPTIONS]
```

| 参数 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `--full` | flag | — | 包含所有元素（不截断） |
| `--no-page-text` | flag | — | 不返回页面文本（只要元素列表，更快） |
| `--wait <STRATEGY>` | string | `dom-stable` | 等待策略：`dom-stable` \| `networkidle` \| `none` |

```bash
bk snapshot
bk snapshot --full
bk snapshot --no-page-text
bk snapshot --wait networkidle
bk snapshot --wait none
```

> 每次操作前必须先 `bk snapshot` 获取元素 ref。元素 ref 在节点未被移除时始终稳定。

---

## act

执行交互动作：click、type、press。

```
Usage: bk act [OPTIONS] [KIND]
```

| 参数 | 类型 | 说明 |
|------|------|------|
| `[KIND]` | positional | 动作类型：`click` \| `type` \| `press` |
| `--ref <ELEMENT_REF>` | string | 元素 ref（backendNodeId，从 snapshot 获取） |
| `--text <TEXT>` | string | type 动作的输入文本 |
| `--append` | flag | type 追加模式（默认为替换） |
| `--keys <KEYS>...` | string[] | press 动作的按键序列 |
| `--x <X>` | integer | click 的 X 坐标（与 --ref 互斥） |
| `--y <Y>` | integer | click 的 Y 坐标（与 --ref 互斥） |

### click

```bash
bk act click --ref 42
bk act click --x 300 --y 200
```

### type

```bash
bk act type --ref 42 --text "hello world"
bk act type --ref 42 --text "追加内容" --append
```

> type 默认为**替换**模式（清空后输入）。需要追加时加 `--append`。

### press

```bash
bk act press --keys Enter
bk act press --keys Tab
bk act press --keys Control+a
bk act press --keys Shift+Enter
bk act press --keys ArrowDown
```

---

## navigate

导航到 URL，或前进/后退/刷新。

```
Usage: bk navigate [OPTIONS] [URL]
```

| 参数 | 类型 | 说明 |
|------|------|------|
| `[URL]` | positional | 目标 URL（与 --back/--forward/--reload 互斥） |
| `--back` | flag | 后退 |
| `--forward` | flag | 前进 |
| `--reload` | flag | 刷新 |

```bash
bk navigate https://example.com
bk navigate file:///tmp/test.html
bk navigate --back
bk navigate --forward
bk navigate --reload
```

---

## open

在新标签页中打开 URL。

```
Usage: bk open <URL>
```

| 参数 | 类型 | 说明 |
|------|------|------|
| `<URL>` | positional（必填） | 要打开的 URL |

```bash
bk open https://example.com
bk open https://github.com
```

---

## close

关闭当前 tab（或用 `--target` 指定的 tab）。

```
Usage: bk close [OPTIONS]
```

无特有参数。通过全局 `--target` 指定要关闭的 tab。

```bash
bk close
bk close --target <targetId>
```

---

## tabs

列出当前 session 的所有标签页。

```
Usage: bk tabs [OPTIONS]
```

无特有参数。返回所有 tab 的 targetId、URL、title。

```bash
bk tabs
bk tabs --session work
```

---

## wait

等待页面条件满足。

```
Usage: bk wait [OPTIONS]
```

| 参数 | 类型 | 说明 |
|------|------|------|
| `--selector <CSS>` | string | 等待元素可见 |
| `--text <TEXT>` | string | 等待文本出现 |
| `--text-gone <TEXT>` | string | 等待文本消失 |
| `--url <PATTERN>` | string | 等待 URL 匹配 |
| `--idle` | flag | 等待网络空闲 |
| `--fn <EXPR>` | string | 等待 JS 表达式返回 truthy |
| `--time <MS>` | integer | 固定等待 N 毫秒 |

```bash
bk wait --selector ".modal"
bk wait --text "提交成功"
bk wait --text-gone "Loading..."
bk wait --idle
bk wait --url "/dashboard"
bk wait --fn "document.querySelectorAll('li').length > 5"
bk wait --time 2000
bk wait --selector "#btn" --timeout 10000
```

---

## evaluate

执行 JavaScript 表达式。

```
Usage: bk evaluate [OPTIONS] [EXPRESSION]
```

| 参数 | 类型 | 说明 |
|------|------|------|
| `[EXPRESSION]` | positional | JS 表达式（内联） |
| `--file <PATH>` | string | 从文件加载 JS 执行 |

```bash
bk evaluate "document.title"
bk evaluate "await fetch('/api').then(r => r.json())"
bk evaluate "document.querySelectorAll('a').length"
bk evaluate --file script.js
```

---

## screenshot

截取页面截图。

```
Usage: bk screenshot [OPTIONS]
```

| 参数 | 类型 | 说明 |
|------|------|------|
| `--output <FILE>` | string | 保存路径（不给则输出 base64） |
| `--full-page` | flag | 整页截图 |

```bash
bk screenshot --output page.png
bk screenshot --full-page --output full.png
bk screenshot
```

> 整页截图 token 消耗极大，优先用 `bk snapshot` 文本方式。

---

## session

Session 管理（关闭/列表/cookie）。

```
Usage: bk session <COMMAND>
```

### session close

关闭当前 session。

```bash
bk session close
bk session close --session work
```

### session list

列出所有 session。

```bash
bk session list
```

### session cookies

Cookie 操作。

```
Usage: bk session cookies <COMMAND>
```

| 子命令 | 说明 |
|--------|------|
| `get` | 获取 cookies |
| `set` | 从 JSON 文件设置 cookies |
| `clear` | 清除所有 cookies |

```bash
bk session cookies get
bk session cookies set cookies.json
bk session cookies clear
```

---

## status

查看连接状态（daemon + 浏览器 + session 概览）。

```
Usage: bk status [OPTIONS]
```

无特有参数。

```bash
bk status
```

---

## Removed Aliases

以下 v1 别名已移除，直接使用对应 v2 命令：

| 已移除命令 | 使用 |
|---------|------|
| `bk goto <url>` | `bk navigate <url>` |
| `bk info` | `bk snapshot` |
| `bk eval <expr>` | `bk evaluate <expr>` |
| `bk shot` | `bk screenshot` |

其余 v1 legacy 命令（ws/tab/browser/daemon/storage/dialog/debug/click/type/fill/select/scroll/hover/drag/focus/upload/keys/find/search/html/url/title/console/options/pdf/open/fetch）仍可用但将在 Phase 3 移除。
