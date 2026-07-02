# browserkit 接管真实 Chrome + 能力增强 —— 任务总结与进度

> 本文件为阶段性总结。记录:总目标、已完成、未完成、以及本轮会话踩过的坑/关键决策。

## 一、总目标
让 `bk`(CLI + 后台 daemon,构建在 cdpkit 之上)能**接管用户日常在用、已登录的真实 Chrome**做 RPA(复用登录态、在用户可见窗口操作),并补齐一批面向 LLM/RPA 的能力。底层 CDP 库为 cdpkit-rs。

## 二、已完成(本会话 9 个提交,`cargo test --lib` 357 + bin 17 全绿)
| 提交 | 内容 |
|---|---|
| bc1d55b | 接管已有 Chrome(`browser connect/discover`、`ws attach`、`ws new --attached`)、升级 **cdpkit 0.3.0**、daemon 生命周期加固 |
| a6fd80d | 持续跟踪标签(setAutoAttach)+ Tab.managed 安全模型 + 单实例 + select/dropdown 修复 |
| 3564c76 | `page state` 加 page_text/page_info、复合 `page wait`、scroll 精细化、`type --clear` |
| 8d3e0de | `find_elements` + `page wait` 的 networkidle |
| e515e29 | 文件上传 `bk upload`(DOM.setFileInputFiles) |
| eb2dcee | JS 对话框拦截(监听 + 用户控制 + per-workspace 策略) |
| 47fe52e | tab 短别名 `t1/t2`(单调不复用,resolve 支持 别名/tid/前缀) |
| dbbba34 | 批量填表 `bk fill --set` |
| 642beaa | **role-ref 稳定寻址**:`--ref`=backendNodeId,抗 DOM 重排,与 `--index` 并存 |

能力清单:connect/discover/attach、attached 持续跟踪、select(按 value/文本 + available_options)、dropdown_options、page state(交互元素 + page_text + page_info + ref)、复合 wait(time/selector/text/text-gone/url/load-state/networkidle/fn)、scroll(方向/像素/到顶底/到元素)、type(--clear)、fill(批量,按类型)、find_elements、upload、dialog(list/accept/dismiss/policy)、tab 别名、--ref 稳定寻址、错误信息取全(exception.description)。

## 三、未完成 / 待办
1. **真机验收**:上述新命令尚需在真实 Chrome 上人工跑一遍(agent 不碰用户 Chrome)。已提供 Node 自检测试服务 + 命令清单(见临时目录)。
2. **push 到远端**:9 个提交均未 push。
3. **交 cdpkit-rs**:给 `CDP::connect` 增加连接超时(browserkit 现用 `tokio::time::timeout` 兜底)。需求报告已在会话中给出。
4. **SSRF 导航防护**:经评估**主动跳过**(对内网 RPA 低价值且易误拦),需要时再做且默认关闭。
5. **多 agent 隔离**:已搁置(attached 共享登录态的单一 context,与完全隔离不可兼得)。
6. 报告里其余次要项(role-ref 已做)、batch 之外的 snapshot/AX-tree 表示等未做。

## 四、本会话关键决策 / 踩坑(重要,务必留意)
- **测试安全铁律**:任何 agent **不得在用户真机启动 / 杀 / 连接 Chrome**(曾因 `taskkill /IM chrome` 与误连误杀用户正在用的浏览器)。真实浏览器验收一律用户手动做;自动化只跑 `cargo test --lib` 等。
- **提交规范**:提交信息**一律英文 + Conventional Commits**,**绝不含 "Claude"/任何 AI 署名**;`.claude/` 已 gitignore;默认只提交源码+构建配置,**不提交 docs/**(除非明说);只在用户明确要求时提交。
- **记忆目录**:真实在 `.claude/agent-memory/browserkit/`(CLAUDE.md 写的 `.claude/memory/` 不准)。
- **Chrome 136 toggle 接管要点**:`chrome://inspect/#remote-debugging` 开关(一次性);Chrome 把动态端口+ws path 写 `DevToolsActivePort`;**toggle 模式下 HTTP `/json/*` 被禁用(404)→ 必须用 `ws://127.0.0.1:<port><wspath>` 直连**,不能靠 `/json/version` 发现;别硬编码 9222。
- **JSON.parse 系统性 bug**:`serde_json::to_string` 产出的已是合法 JS 字面量,旧代码再套 `JSON.parse(...)` 导致解析裸词报错(select 等);已全仓修正(storage import 保留正好一层)。
- **Tab.managed 安全模型**:bk 只回收自己建的(managed=true → CloseTarget;用户的 → 仅 detach);unmanaged 浏览器永不被杀(child=None)。取代了早期 had_attached 兜底。
- **unmanaged 浏览器=运行时态**:不持久化、daemon 重启不自动重连(避免无端弹授权窗/触碰用户浏览器);managed 才恢复且后台非阻塞。
- **daemon 单实例**:用 OS 文件锁(fs2)保证唯一实例,进程崩溃 OS 自动释放;修复了"关停不退出→孤儿堆积→锁住 bk.exe";`daemon stop` 等进程真正退出再返回;`stop/status` 不自动拉起。
- **setAutoAttach 语义**:它只自动 attach "related" target;**用户手动新开的顶层标签靠 `Target.targetCreated`**;采用 **type-only 纳管**(凡 `type==page` 含 `chrome://newtab` 都纳管,非 page 按 type 排除);"快照替换"模型。
- **role-ref 选型**:经三轮调研(browser-use 两次 + openclaw)一致 → 用 **backendNodeId(方案 B)**,纯 CDP、零页面副作用;openclaw 注入 data 属性只是为 Playwright 定位,我们不需要。
- **dialog**:per-session 订阅 `Page.javascriptDialogOpening`,在来源 session 上 `handleJavaScriptDialog`;真实 profile `has_browser_handler=true` 时 Chrome 会同时弹原生 UI、先 handle 先生效;默认 manual 策略(记录+等用户),可配 accept/dismiss。
- **cdpkit 0.3.0**:unified Sender API(`cmd.send(&cdp)` / `let s=cdp.session(id); cmd.send(&s)`;自定义 method 用 `send_cmd`;事件 `Event::subscribe`)。
- **PowerShell 引号坑**:`bk eval` 传 JS 时别用 `\"`;JS 内只用单引号、外层 PS 双引号包,且避免 `$`;复杂 JS 存文件 `Get-Content -Raw` 传入。

## 五、当前仓库状态
- 分支 main,本会话 9 提交于 `a5b4686` 之上;工作树干净(仅 docs/ 未跟踪,按约定不提交)。
- 未 push。
