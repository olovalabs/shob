# rpi Rust 扩展与 Agent 调试指引

本文覆盖两条主线：**如何编写 Rust `cdylib` 扩展**（统一 C ABI、工具生命周期、
事件处理器、资源发现、Provider/渲染器），以及**如何调试 agent 与扩展**（事件日志、
开发宿主、系统提示词检查、常见失败定位）。创建完整 Agent 项目的默认结构见 `docs` 的 `agent` 主题。

## 1. Rust 扩展快速开始

最小可用扩展是一个 Rust `cdylib` crate，只依赖 `rpi-plugin-sdk`：

```bash
cargo new my-rpi-extension
cd my-rpi-extension
# 在 Cargo.toml 中把 crate-type 设为 ["cdylib", "rlib"]
```

```toml
[package]
name = "my-rpi-extension"
version = "0.1.0"
edition = "2021"
license = "MIT"

[lib]
crate-type = ["cdylib", "rlib"]

[dependencies]
rpi-plugin-sdk = "0.8"
serde_json = "1"
```

**扩展边界规则**：`cdylib` 只依赖 `rpi-plugin-sdk`。绝不链接 `rpi-cli`、
`rpi-extensions` 或 harness 内部 crate——插件由宿主动态加载，宿主链接插件，
插件不能反向链接宿主。保留 `rlib` 是为了能在同一 crate 内写 Rust 单元测试。

仓库自带的 `examples/plugin-stub` 是完整可运行的模板，包含 echo 工具、事件
处理器和资源发现三种能力，是复制骨架的首选来源。

## 2. 统一 ABI 入口

宿主只加载统一 C ABI 符号 `rpi_plugin_register`。扩展使用 `export_plugin!`，
通过同一个 `PluginApi` 注册能力并调用 `api.declare` 声明优先级/平台。
SDK 在注册前检查 `abi_version` 和 `struct_size`；不兼容时返回非零状态，宿主报告加载失败。

```rust
rpi_plugin_sdk::export_plugin!(|api| {
    if let Some(declare) = api.declare {
        let status = declare(rpi_plugin_sdk::StbStringRef::from_str(
            r#"{"priority":60,"platforms":["linux"]}"#,
        ));
        if status != 0 {
            return status;
        }
    }
    let Some(register_tool) = api.register_tool else {
        return -1;
    };
    // 构建 StableToolSchema 和 execute/poll/cancel/destroy 函数表，
    // 然后调用 register_tool。完整实现参考 examples/plugin-stub。
    let _ = register_tool;
    0
});
```

`declare` 的 JSON 支持 `priority`（数字，越大越先）和 `platforms`（字符串数组，
如 `["linux","windows","macos"]`）；宿主在分发事件处理器时跳过不匹配的平台。

### 能力检测

每个注册槽位都是 `Option`。注册前检查 vtable slot 是否为 `Some`，缺失时返回
清晰错误并优雅降级，不要 panic、空指针或依赖 `undefined` 语义。宿主对未知
runtime action ID 返回结构化错误，不会进入分发。

## 3. 工具生命周期：execute → poll → cancel → destroy

每个工具导出四个 `extern "C"` 函数，宿主按固定协议驱动：

- `execute(tool_call_id, params, free_params) -> StepHandle`：解析宿主传入的 JSON
  （owned `StbString`，用 `free_with(free_params)` 释放），创建本次调用独立的
  handle（通常是 `Box::into_raw(Box::new(State))`）。
- `poll(handle, partial_cb, user_data) -> StepResult`：**必须非阻塞**。未完成返回
  `StepResult::pending(progress)`；完成返回 `StepResult::done(result)`；
  失败返回 `StepResult::err(message)`。`partial_cb` 在 poll 内同步调用。
- `cancel(handle)`：设置线程安全取消标记。**必须幂等，不能释放 handle**。
- `destroy(handle)`：宿主最终调用一次，`Box::from_raw` 释放 handle。空指针要 no-op。

所有权协议要点：

- 宿主产生的 `StbString`（execute 的 params、事件 payload）由**插件**通过宿主
  提供的 `free_string` 释放。
- 插件产生的 `StbString`（done 结果、partial 进度、resources_discover 的 out）
  由**宿主**通过插件注册时提供的 `plugin_free_string` 释放。
- 每个 `extern "C"` 边界都不能 unwind：插件内部 `catch_unwind` 并把 panic 转换为
  结构化错误结果。

```rust
extern "C" fn my_execute(
    _tool_call_id: StbStringRef,
    params: StbString,
    free_params: Option<FreeStringFn>,
) -> StepHandle {
    let text = params.to_string_lossy();
    params.free_with(free_params);
    let state = Box::new(MyState { input: text, done: false });
    Box::into_raw(state) as StepHandle
}

extern "C" fn my_poll(
    handle: StepHandle,
    _partial_cb: Option<ToolPartialCb>,
    _user_data: *mut c_void,
) -> StepResult {
    let state = unsafe { &mut *(handle as *mut MyState) };
    if !state.done {
        state.done = true;
        return StepResult::pending(StbString::empty());
    }
    StepResult::done(StbString::from_string(
        r#"{"content":[{"type":"text","text":"ok"}]}"#.to_string(),
    ))
}

extern "C" fn my_cancel(_handle: StepHandle) {}

extern "C" fn my_destroy(handle: StepHandle) {
    if !handle.is_null() {
        unsafe { drop(Box::from_raw(handle as *mut MyState)) };
    }
}
```

完整模板见 `examples/plugin-stub/src/lib.rs`。创建新工具时复制生命周期骨架，
只替换参数 schema 和领域逻辑，不要重新设计所有权协议。

## 4. 事件处理器、资源发现与 Provider

### 事件处理器（36 个事件类别）

通过 `api.register_event_handler` 按 `EventTag` 订阅。事件分三大类：

- **会话生命周期**：`SessionStart`、`SessionBeforeSwitch`、`SessionBeforeFork`、
  `SessionBeforeCompact`、`SessionCompact`、`SessionShutdown` 等。
- **Agent 循环**：`BeforeAgentStart`、`AgentStart`、`AgentEnd`、`AgentSettled`、
  `TurnStart`、`TurnEnd`、`MessageStart`/`MessageUpdate`/`MessageEnd`、
  `ToolExecutionStart`/`Update`/`End`、`ToolCall`、`ToolResult`、`UserBash`、`Input`。
- **rpi 特有 / UI**：`BeforeTuiStart`（TUI 初始化前）、`UiPromptStart`/`UiPromptEnd`
  （交互式提示阻塞 agent 时）。

处理器收到 `StablePluginEvent`，**不得释放**事件里的字符串（宿主 fan-out 后统一
释放）。返回 `0` 成功；返回 `EVENT_HANDLER_ABORT` 表示 veto。

### 生命周期 veto（P1）

`BeforeTuiStart` 是一个带 veto 语义的 rpi 专有钩子：第一个返回
`EVENT_HANDLER_ABORT` 的处理器会**中止启动**，CLI 以退出码 `3` 退出并打印原因
（用于"进入 TUI 前必须先建立外部连接"这类场景）。`SessionStart` / `SessionShutdown`
的 veto 只是 advisory：headless 模式打印警告，shutdown 时忽略。事件分发带超时，
慢处理器不会无限阻塞主流程。

### 资源发现

`api.register_resources_discover` 注册一个 handler，宿主在 `startup` / `reload`
时 fan-out 到所有已注册 handler，合并返回的 `{skillPaths, promptPaths, themePaths}`。
单个 handler 报错不会中止 fan-out。`out` 是插件拥有的 `StbString`，宿主通过插件
注册时给的 `plugin_free_string` 回收。

### Provider 与渲染器

`api.register_provider` 注入自定义 LLM Provider（`provider_id` / `base_url` /
`api_style` + `request_fn`）。`register_message_renderer` /
`register_markdown_transformer` / `register_entry_renderer` 注册消息渲染能力，
宿主接受 `{text, lines}` 组件信封。这些槽位都可能为 `None`，使用前必须检测。

## 5. 用 `rpi dev` 开发

在扩展 crate 目录运行：

```bash
rpi dev
```

rpi 通过 Cargo metadata 识别当前 `cdylib`，首次编译后把版本化副本放到工作区
`.rpi/extensions/.dev/`，启动正常 TUI，并监控 `src/`、`build.rs`、package/workspace
`Cargo.toml` 和 `Cargo.lock`。源码变化自动重编译并热重载；编译失败保留当前已加载
版本。TUI 里 `/reload` 会强制重新构建再加载。

多扩展 workspace 必须明确选择 package：

```bash
rpi dev --package rpi-todo
rpi dev -P rpi-webfetch
rpi dev --release
rpi dev --no-watch
rpi dev --package rpi-todo -- --model gateway/model
```

调试单个扩展用 `rpi dev-local`（等价于 `rpi dev --local-only`）：只加载当前编译
的扩展和该扩展发现的 skill/prompt，不扫描全局扩展、已安装 Pi 包或其他项目资源，
避免同名命令干扰。没有 cdylib 时优雅降级为 skills-only 模式。Windows 上已加载
DLL 无法原地覆盖，因此不要手工复制 Cargo target DLL；版本化 staging 会安全切换。

## 6. Agent 调试：事件日志

### 开启事件日志

```bash
export RPI_EVENT_LOG=1
rpi            # 任意运行都会记录扩展事件处理器调用
```

日志默认写到 `~/.rpi/logs/events.jsonl`（`RPI_CODING_AGENT_DIR` 存在时写到其
父目录的 `logs/` 下；`RPI_EVENT_LOG_PATH` 可显式覆盖）。关闭时零磁盘写入、零开销。

查看日志位置和实时跟踪：

```bash
rpi events path        # 打印事件日志路径
rpi events tail        # 打印已有内容并持续跟踪（Ctrl+C 停止）
```

每一行 JSON 包含 `ts_ms`（Unix 毫秒）、`event`（如 `BeforeTuiStart` /
`MessageEnd`）、`plugin`（扩展显示名）、`result`（`Continue` / `Error` / `Abort` /
`Timeout` / `Panic` / `JoinFailed`）、`duration_ms` 和可选 `detail`。用它回答
"我的扩展为什么没运行"这类问题。

### 请求、上下文与帧写入诊断

排查“模型请求很慢”“上下文构建耗时”或“流式进度写入阻塞”时，可临时开启
stderr 诊断。两个开关都只在值存在且不等于字符串 `0` 时启用，不会把日志写入会话
文件，也不会改变正常的持久化格式：

```bash
RPI_AGENT_DIAGNOSTICS=1 RPI_FRAME_DIAGNOSTICS=1 rpi -p "复现问题" 2>rpi-diagnostics.log
```

PowerShell：

```powershell
$env:RPI_AGENT_DIAGNOSTICS="1"
$env:RPI_FRAME_DIAGNOSTICS="1"
rpi -p "复现问题" 2> rpi-diagnostics.log
```

- `RPI_AGENT_DIAGNOSTICS` 输出上下文构建、assistant 请求转换/建流、工具执行和
  session entry/record/batch append 的纳秒耗时；每次模型请求以
  `RPI_AGENT_DIAGNOSTICS` 开头，每次工具以 `RPI_TOOL_DIAGNOSTICS` 开头。
- `RPI_FRAME_DIAGNOSTICS` 只对持久化 session 的 assistant frame writer 生效，运行
  结束清理时输出 `RPI_FRAME_DIAGNOSTICS` 汇总。重点看 `batch_records`、
  `append_records_calls`、`flush_requests`、`writer_failures` 和各类 flush barrier。
- frame writer 默认最多聚合 32 条记录，或等待约 50 ms 后写入；提交 assistant entry
  和清理 stream/run 前后会强制 flush。诊断中的 `*_ns` 是累计耗时，不能直接当作单次
  wall-clock 延迟或吞吐基准。

这类诊断只用于复现和定位，可能包含工具名、调用 ID 与会话规模等元数据；不要把
包含敏感 provider 响应的完整 stderr 直接提交到仓库。没有启用开关时不会输出这些
诊断行。

### 系统提示词与资源检查

```bash
rpi --debug-system-prompt   # 把最终解析出的系统提示词各段打印到 stderr
rpi --list-models           # 列出可用模型（可带模糊搜索词）
rpi --verbose               # 显示启动警告（如被忽略的 flag）
```

`--debug-system-prompt` 会显示 skills、prompts、package 资源和计数，是排查
"资源为什么没出现在欢迎页"的直达路径。

### 凭据与 Provider 调试

```bash
rpi auth check                          # 报告任何可用认证源（不联网）
rpi auth check --provider gateway --json
```

接口报错时保留 provider 返回的诊断文本，先检查 endpoint、认证头和模型 ID。

### 会话导出

```bash
rpi --export session.jsonl      # 把 JSONL 会话导出为 HTML 便于审阅
```

## 7. 远程模式调试

```bash
rpi --server --port 9899        # 无头服务端，启动时打印 token
rpi --connect 127.0.0.1:9899 --token <token>   # 远程 TUI 客户端
```

客户端是纯输入/显示，没有任何本地 provider、工具、扩展或 session 文件；全部 agent
工作发生在服务端。调试技巧：

- 服务端日志（`--verbose`）显示启动警告；事件日志（`RPI_EVENT_LOG=1`）在服务端
  进程开启，用 `rpi events tail` 在同一台机器跟踪。
- 客户端可用 `/state`（当前 model/thinking/工具）、`/model <id>`、`/thinking <level>`、
  `/tools` 检查会话状态。
- 远程模式下 `/tree`、`/fork`、`/switch`、`/export`、`/name`、`/reload` 不可用
  （依赖本地 harness），会得到明确的"不支持"提示。

### 按键诊断（手机 / 远程终端）

手机客户端、IME、SSH 网关对"回车"的编码各不相同，出问题时先看**到达 TUI 的原始
按键事件**，不要猜。设置 `RPI_DEBUG_KEYS` 即可把所有按键事件（含 Enter 的判定结果）
追加到日志：

```powershell
# Windows：在手机 SSH 进来的那个会话里
$env:RPI_DEBUG_KEYS=1; rpi
```

```bash
# Unix / Termux
RPI_DEBUG_KEYS=1 rpi
```

默认写到 `<临时目录>/rpi-keys.log`（Windows 为 `%TEMP%\rpi-keys.log`），也可用
`RPI_DEBUG_KEYS=<路径>` 指定文件。格式：

```text
2026-09-24T12:00:01.234Z --- rpi TUI key trace start pid=4321 TERM=xterm-256color raw_mode=true ---
2026-09-24T12:00:01.235Z code=Char('h') mods=NONE kind=Press
2026-09-24T12:00:01.236Z code=Enter mods=NONE kind=Press
2026-09-24T12:00:01.236Z enter-decision -> newline (paste burst) gap_ms=1 more_queued=false
```

按这两条线索判断：

| 日志 | 含义 | 处理 |
| --- | --- | --- |
| `code=Char('j') mods=CONTROL`（或 `Char('\n')`） | 客户端把回车发成了 **LF (0x0A)**。crossterm 在 raw 模式下把 LF 当 Ctrl+J，而 Ctrl+J 绑定的是"插入换行"（对齐 pi 的 `tui.input.newLine`） | **已修复**：`Editor::handle_key` 现在把无修饰键的 `Char('\n')`/`Char('\r')` 当作提交，`Ctrl+J`（`Char('j') + CONTROL`）仍保持插入换行 |
| `code=Enter mods=NONE` + `enter-decision -> newline (paste burst)` | 收到的是正常 CR，但客户端把"文字 + 回车"合并成一批发送，粘贴启发式（20ms 突发窗口）误判为粘贴 | **已修复**：粘贴启发式仅保留在 Windows（`cfg!(windows)`）。Unix 上括号粘贴用 `Event::Paste` 处理真实粘贴，普通回车直接提交 |
| `code=Enter mods=NONE` + `enter-decision -> submit` | 输入路径正常，问题不在按键层 | 检查会话/网络层 |
| 完全没有 `code=` 行 | 按键根本没到 TUI | 客户端没连上、窗口未聚焦，或 `RPI_SKIP_STDIN` 被设了 |

## 8. 常见失败定位

| 症状 | 检查项 |
| --- | --- |
| 扩展没加载 | `cdylib` crate-type？平台匹配？用 `--extensions-dir` 指向生成目录；`--no-extensions` 未误开 |
| 工具未注册 | ABI 版本协商失败？vtable slot 是 `None`（宿主未实现）？注册返回非零？ |
| 事件处理器没触发 | `RPI_EVENT_LOG=1` + `rpi events tail` 看是否有 `Abort`/`Panic`/`Timeout` |
| 启动被中止 | `BeforeTuiStart` 有扩展返回 `EVENT_HANDLER_ABORT`，退出码 3 |
| `poll` 卡住 | `poll` 必须非阻塞；耗时工作放线程，handle 保存状态，取消标记限时结束 |
| Windows 上热重载失败 | 已加载 DLL 无法覆盖；用 `rpi dev` 的版本化 staging，不要手工复制 |
| 资源没出现在系统提示词 | `--debug-system-prompt` 查看最终组装结果；确认 skill 路径被发现 |
| 手机/远程终端回车变换行 | `RPI_DEBUG_KEYS=1` 看按键：`Char('j') mods=CONTROL` = 客户端发的是 LF（已被 `Editor` 当提交处理）；`Enter` + `paste burst` = 粘贴启发式误判（已限定 Windows 平台） |
| 粘贴内容不是系统剪贴板（而是 TUI 自己的文案） | 远程/手机客户端把粘贴映射成 `Ctrl+V` 时，`read_clipboard_text` 会读**服务器**剪贴板——可能被拖选复制（copy-on-select）写入了 TUI 文案（如欢迎语 `Skills (…): …`）。已限定该回退只在 Windows 生效；Unix 上真实粘贴走括号粘贴 `Event::Paste`，`Ctrl+V` 回退为编辑器 kill-ring yank |
| panic / 段错误跨 ABI | `extern "C"` 边界不能 unwind；`StbString` 所有权遵守宿主/插件两侧规则 |
| ABI 版本不匹配 | 宿主日志提示 mismatch 并跳过加载；确认 `rpi-plugin-sdk` 版本 |

## 9. 测试与发布

- 领域逻辑写成普通 Rust 函数，FFI 只做转换/生命周期/错误映射；对普通逻辑写
  `rlib` 单元测试。
- 至少一个宿主 smoke test 验证 ABI 注册、加载和卸载（参考 `plugin-stub` 的
  测试模式：宿主读取插件导出的 hit counter 断言行为）。
- `cargo fmt --check`、`cargo clippy --all-targets`、`cargo test` 通过。
- `rpi dev --no-watch` 能编译、加载并注册预期工具。
- `rpi install <crate> --force` 的干净安装路径通过（Linux/macOS/Windows 由 CI 验证）。
- README 记录工具 schema、权限边界、持久化文件、网络访问、支持平台和卸载方式。
