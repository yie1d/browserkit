# browserkit v1 legacy 保留必要性评估

> 日期：2026-07-13
>
> 结论：v1 legacy 的价值主要是过渡兼容和调试兜底，不是 browserkit 的长期产品价值。建议短期保留必要兼容入口，中期完成 Phase 3 后移除普通 agent 可见的 v1 命令，只保留少量明确标注的内部诊断能力。

## 背景

browserkit 当前定位已经从“浏览器自动化 CLI”调整为“面向 AI agent 的持久浏览器运行时”。v2 的 agent-facing API 是：

- `bk connect`
- `bk open`
- `bk snapshot`
- `bk act`
- `bk navigate`
- `bk wait`
- `bk evaluate`
- `bk screenshot`
- `bk tabs`
- `bk close`
- `bk session`
- `bk status`

v1 legacy 仍存在于代码和部分文档中，主要包括：

- workspace 外部命令：`ws`, `new`, `ls`, `rm`
- 旧 tab/browser/daemon/storage/dialog/debug 命令组
- 旧页面操作命令：`goto`, `click`, `type`, `fill`, `select`, `scroll`, `hover`, `drag`, `focus`, `upload`, `keys`
- 旧检查/捕获命令：`info`, `find`, `search`, `eval`, `html`, `url`, `title`, `console`, `options`, `shot`, `pdf`, `fetch`
- `BK_WS` 和 workspace resolution 逻辑

这些入口在 `src/main.rs` 中仍真实存在，不只是文档残留。

## v1 仍有作用的地方

### 1. 向后兼容

如果已有脚本、agent skill、人工流程还在调用 v1 命令，直接删除会造成破坏。典型例子：

- `bk goto <url>`
- `bk info`
- `bk click --ref N`
- `bk tab list`
- `bk ws attach`
- `bk browser discover`

保留 v1 可以给外部使用者一个迁移窗口。

### 2. 功能兜底

部分能力在 v2 设计中应归入 `bk act` 或 `bk session`，但当前 README 仍说明某些 Phase 2 actions 暂时通过 legacy commands 暴露：

- `fill`
- `select`
- `scroll`
- `hover`
- `drag`
- `upload`
- `dialog`

在 v2 命令完全覆盖并验证前，删除 legacy 会造成功能缺口。

### 3. 调试和运维入口

以下命令对开发排障仍有价值，但不一定适合作为 agent 主 API：

- `bk browser discover/connect/list/disconnect`
- `bk daemon start/stop/status`
- `bk debug cdp/events`
- `bk dialog list/policy`

这类命令可以保留为 internal/debug surface，但不应在 README 主路径宣传。

### 4. 内部状态尚未完全迁移

当前代码仍有 `workspace` runtime 类型、handler、持久化迁移和 legacy resolution。即使对外使用 session 概念，底层立即删除 workspace 会牵动：

- tab 生命周期
- attached workspace / unmanaged tab 安全模型
- persisted state 迁移
- legacy 命令测试
- `bk status` 中的 workspace 聚合

这部分需要作为 Phase 3 工程任务处理，不能只靠文档改名解决。

## 保留 v1 的成本

### 1. 定位混乱

v1 把 browserkit 拉回“CLI 工具集合”的认知：`ws`, `tab`, `browser`, `daemon`, `debug`, `storage` 等命令都暴露底层结构。它削弱了当前“browser runtime + observe/act/session”的定位。

### 2. agent 误用风险

后续 agent 搜索代码或文档时，容易选择旧命令：

- 用 `ws` 而不是 `session`
- 用 `goto/info` 而不是 `navigate/snapshot`
- 用旧 `click/type` 而不是 `act click/type`

这会增加上下文负担，也会让输出和错误处理路径不一致。

### 3. 维护面扩大

保留两套外部 API 意味着修 bug 时要考虑两条路径：

- v2 primary commands
- v1 legacy aliases / command groups

对并发、持久化、tab 生命周期、用户 Chrome 安全模型这类敏感逻辑尤其不利。

### 4. 文档污染

历史文档如 `project-analysis.md`、`SESSION-SUMMARY.md`、旧 plan 文件保存了有价值上下文，但它们也会让全文搜索结果充满旧口径。没有归档标识时，后续 agent 容易把历史快照当成当前架构。

## 必要性分级

| 类别 | 当前必要性 | 建议 |
|------|------------|------|
| `goto/info/eval/shot` deprecated aliases | 低 | v2 已有等价命令，可在 Phase 3 删除 |
| 旧 page action 命令 | 中 | 等 `act fill/select/scroll/hover/drag/upload/dialog` 全部稳定后删除 |
| `ws/new/ls/rm` workspace 外部入口 | 低到中 | 如果没有旧脚本依赖，应优先移除对外入口 |
| `tab` 外部入口 | 中 | v2 `tabs/open/close --target` 稳定后移除 |
| `browser` 命令组 | 中到高 | 保留为 internal/debug，隐藏出主文档 |
| `daemon` 命令组 | 中到高 | 保留为 internal/debug，隐藏出主文档 |
| `debug` 命令组 | 中 | 保留给开发者，不面向普通 agent |
| `storage` 命令组 | 低到中 | 若 `session cookies` 和 `evaluate` 足够，可删除或降级 internal |
| workspace 内部类型/handler | 中 | 需要工程迁移，不建议立即机械删除 |
| 历史文档 | 低 | 移入 `docs/archive/` 或加明确过时标识 |

## 建议决策

### 短期：隐藏和归档，而不是立即删代码

1. README 只展示 v2/runtime/session API。
2. `bk --help` 中 legacy 只保留一句迁移说明，不扩展解释。
3. 历史文档移入 `docs/archive/`，并在 archive README 写明“仅历史参考，不代表当前架构”。
4. `docs/bk-browser/references/commands.md` 继续只推荐 v2 命令，legacy 列表保留一行即可。

### 中期：做 Phase 3 legacy removal

删除普通 agent 不该使用的 v1 对外命令：

- `ws`, `new`, `ls`, `rm`
- `tab`
- `goto`, `info`, `eval`, `shot`
- 旧 `click/type/fill/select/scroll/hover/drag/focus/upload/keys`
- `find/search/html/url/title/console/options/pdf/fetch`

删除前必须确认 v2 覆盖：

- `act fill`
- `act select`
- `act scroll`
- `act hover`
- `act drag`
- `act upload`
- `act dialog`
- `tabs/open/close --target`
- `session cookies`
- `status`

### 长期：只保留明确分层的接口

最终外部接口应分三类：

1. **Agent primary API**：`connect/open/snapshot/act/navigate/wait/evaluate/screenshot/tabs/close/session/status`
2. **Developer/internal API**：`browser`, `daemon`, `debug`，默认从主 README 隐藏
3. **cdpkit layer**：底层 CDP 能力，不在 browserkit 上层重复实现

## 删除前检查清单

执行 Phase 3 删除前，应逐项确认：

- 没有发布文档继续推荐 v1 命令。
- `docs/bk-browser` skill 不依赖 v1 命令完成主工作流。
- 项目内测试覆盖 v2 等价命令。
- 真实 Chrome 验证覆盖 attach existing browser、default session、isolated session。
- `bk status` 不再依赖 workspace 外部概念，或已明确改成 runtime/session 视角。
- `~/.bk/state.json` 对旧 workspace 数据有迁移或兼容策略。
- 删除 `BK_WS` 后错误信息指向 `BK_SESSION` / `--session`。

## 总结

v1 legacy 当前还有过渡价值，但不应继续作为产品形态存在。最合适的判断是：

- **作为兼容层**：短期可以保留。
- **作为文档和主入口**：应尽快隐藏或归档。
- **作为长期 API**：不值得保留。
- **作为内部实现**：只有在迁移成本高于收益时才暂时保留，并且要明确标注为 legacy/internal。

推荐路线是：先清文档和主入口，再补齐 v2 action 覆盖，最后执行 Phase 3 删除普通 agent 可见的 v1 命令。
