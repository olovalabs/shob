# rpi Rust 扩展开发最佳实践

本文是 rpi 创建 Rust `cdylib` 扩展的模型可读规范。扩展是 Rust 原生动态库，
通过稳定 C ABI（`rpi-plugin-sdk`）注册工具、事件处理器、资源发现、Provider
与渲染器，宿主（`rpi` CLI）负责加载与生命周期。创建完整 Agent 项目的默认结构见
`docs` 的 `agent` 主题；本指南聚焦扩展本身的 ABI 与生命周期。

## 1. 扩展模型总览

| 项目 | Rust 原生扩展 |
| --- | --- |
| 安装 | `rpi install crate-name`（预编译协议）；`--build` / `--path`（源码） |
| 入口 | `rpi_plugin_register` C ABI 符号 |
| 开发反馈 | `rpi dev` watch + 热重载；`rpi dev-local` 隔离调试 |
| 静态资源 | `resources_discover` 或随项目放入 `.rpi` |
| 运行环境 | 本机动态库，当前用户权限 |
| 适用场景 | 本地工具、系统集成、性能敏感或纯 Rust 项目 |

推荐用 `rpi extension update [name]` 更新扩展：省略名称时更新全部项目链接扩展和
全局 registry/prebuilt 扩展，指定名称时只更新匹配项。更新保留原来的 Cargo 构建
后端或显式 GitHub 来源；全局通过 `--path` 安装的开发扩展不参与更新。
旧命令 `rpi package update [name]` 仍兼容；项目链接管理命令仍为
`rpi package link`、`rpi package list`、`rpi package unlink`，不改名。

扩展应遵循能力检测：只调用宿主明确提供的 capability，检查 `PluginApi`
对应注册槽位是否为 `Some`。缺少能力时返回清晰错误并优雅降级，不要 panic、
空指针或依赖 `undefined` 语义。

### 先选复用层

写会被 `rpi` 直接加载的 Rust `cdylib` 插件时，ABI 入口和工具/事件注册使用
`rpi-plugin-sdk`。不要把 `rpi-extensions`、`rpi-cli` 或 harness 私有模块链接进
插件；它们属于宿主侧。

如果插件项目还需要 Web/ACP/远程适配器共享的会话、工作区、RPC、事件重放和运行
状态逻辑，可以复用 crates.io 的 [`rpi-extcore`](https://crates.io/crates/rpi-extcore)：

```toml
[dependencies]
rpi-plugin-sdk = "0.8"
rpi-extcore = "0.1" # 需要 native RPC / session / workspace 服务时再添加
```

`rpi-extcore` 是普通 Rust 库，不是 `cdylib` 插件入口，也不替代
`rpi-plugin-sdk`。它为适配器提供 `service::Service`、`host::HostSession`、事件订阅、
会话/工作区操作和 `durable::DurableCoordinator` 等可复用模块；运行时需要一个兼容的
`rpi --mode rpc` 可执行文件。它不会自行启动服务端、管理凭据或提供 HTTP/ACP 前端，
也不会创建第二份 transcript。需要它的项目可以这样初始化共享服务：

```rust
use rpi_extcore::service::{self, BackendConfig, Service};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let core = Service::new(BackendConfig::default())?;
    let projects = service::list_projects(&core).await;
    assert!(projects["projects"].is_array());
    core.shutdown().await;
    Ok(())
}
```

如果只需要给 `rpi` 增加一个工具，保持 `rpi-plugin-sdk` 单一依赖并从
`examples/plugin-stub` 开始；只有确实需要协议适配器或共享 native RPC 服务时才加入
`rpi-extcore`。注意它与插件 ABI 的版本策略独立：插件 SDK 跟随宿主 ABI，`rpi-extcore`
按自己的 `0.1` minor line 选择兼容 patch 版本。

## 2. 推荐结构

```text
my-rpi-extension/
├── Cargo.toml
├── README.md
├── src/
│   ├── lib.rs
│   ├── tool.rs
│   └── error.rs
└── tests/
    └── behavior.rs
```

### Cargo.toml

```toml
[package]
name = "my-rpi-extension"
version = "0.1.0"
edition = "2021"
license = "MIT"
repository = "https://github.com/example/my-rpi-extension"

[lib]
crate-type = ["cdylib", "rlib"]

[dependencies]
rpi-plugin-sdk = "0.8"
serde_json = "1"
```

`cdylib` 是宿主加载的产物；同时保留 `rlib` 便于 Rust 集成测试。扩展边界只依赖
`rpi-plugin-sdk`，不要依赖 `rpi-cli`、`rpi-extensions` 或 harness 内部模块，
否则插件会和宿主实现耦合。

## 3. ABI 入口

宿主只加载统一 C ABI 符号 `rpi_plugin_register`。扩展使用 `export_plugin!`，
通过同一个 `PluginApi` 注册能力并调用 `api.declare` 声明优先级/平台。
SDK 在注册前检查 `abi_version` 和 `struct_size`；旧宿主缺少的尾部可选槽位补成
`None`，插件可据此降级。版本不兼容或必需字段不完整时返回非零状态，宿主报告加载失败。

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
清晰错误；不要依赖空指针或 panic 表达不支持。宿主对未知 runtime action ID
返回结构化错误，不会进入分发。runtime action 使用 SDK 的 `RuntimeActionId`
定义（含 `GetCliFlag`、`UiDialog`），不要自行假定数字范围。

### 独立子任务

`GetExecutionContext`（23）返回当前模型身份、thinking、活动工具、cwd 和会话 ID，
不包含凭据、认证头或 endpoint。`ChildAgent`（24）使用 `op` 分派：

- `start`：`task`（必填，最多 20,000 bytes）、`role`、`maxTurns`（1–32，默认 8）、
  `timeoutSeconds`（10–1800，默认 300），以及可选的父活动工具子集 `tools`。
  立即返回 `childId`、独立 `sessionId` 和 `status: running`。
- `poll`：传入 `childId`，消费 `deltas` 数组并取得 `running/done/failed` 状态。
  `done` 后还要检查 `result.status`、`result.text` 和 `result.turnLimited`。
- `cancel`：设置取消信号；`dispose`：取消并释放 handle。所有终态都必须 dispose。

子任务继承有效 provider 私有绑定、系统提示词/资源、项目 cwd 和权限 hook，
不重新加载默认配置，也不进入父 transcript。权限审批和用户问题使用父 UI，
无交互消费者时不会默认允许。`delegate_task` 从子工具注册表中删除。
插件工具仍属于父宿主，`__rpi.sessionId` 仍是父扩展状态空间，不能当成 child ID。
handle 按插件 owner 隔离；父结束/取消、会话切换、关闭或插件 reload 会清理。
每宿主最多保留 16 个任务，文本增量和最终报告共享 4 MiB、最多 8192 个增量。
这不是 OS 安全沙箱；只读任务应缩小工具范围并使用权限规则。

从工作线程调用同步 runtime action，避免在单线程 Tokio executor 中自锁。
ChildAgent 还必须绑定本次工具调用的 `__rpi.runtimeHandle` / `runtimeKey`：
用 opaque handle 作为 action 的 user_data，并在参数中传入 `__rpiRuntimeKey`；
不得跨调用缓存或输出给模型。仅注册级、失效和 child-originated 调用会被拒绝
（包括别名工具），不能经根宿主绕过工具子集。正确的父 capability 在取消后仅允许
poll/cancel/dispose 回收已有任务，不允许 start；排队中的 start 也继承原调用取消。
同步工具回调由宿主自动绑定。后台工具、provider 和审批 driver 自己保活 DLL 和参数；
超过两秒前台清理宽限期仍占用活动名额，直到实际退出。
宿主 runtime binding 弱持有父 harness；候选 reload 回退不会留下可解引用的失效 bridge 指针。
旧宿主不支持新 action 时明确提示升级，不能静默退回可能换模型的子进程。
完整协议与真实 DLL 测试见仓库 `docs/extensions/subagents.md`。

### 运行期间追加上下文

`AppendEntry` 的 `{"customType": "...", "data": {...}}` 形式只保存扩展数据；
默认不进入模型上下文。`{"message": <AgentMessage>}` 形式追加会话消息，但不
触发新一轮模型回复。运行期间消息先进入持久化队列，等当前轮的全部工具结果
提交后再写入历史并展示，下一次模型请求才能读取。

返回的 `entryId` 是稳定的保留 ID；排队期间查询该 entry 可能为空。工具内部
可以等待 `AppendEntry` 返回，它不会等待工具或整个运行结束。取消和会话恢复
不会丢弃已经接受的排队消息。

### SendMessage 的投递选项

`SendMessage` 接受原有 `{"message": <AgentMessage>}` / `{"text": "..."}`，
也接受 Pi 的自定义消息形状：`customType`、`content`、`display`、`details`。
`content` 可以是字符串或文本/图片块数组；缺失或 null 会归一化为空数组。

选项可以放在 `options` 对象中，也可以放在参数顶层；两种形式不要混用：

```json
{
  "message": {"customType": "goal-state", "content": "目标状态已更新", "display": true},
  "options": {"triggerTurn": false}
}
```

| 选项 | 行为 |
| --- | --- |
| 不传 `triggerTurn` | 空闲时只追加上下文；运行时默认进入 steering 队列 |
| `triggerTurn: true` | 空闲时发起模型运行；运行时按 `deliverAs` 排队 |
| `triggerTurn: false` | 只更新上下文，运行时等当前工具批次结束后提交；不主动追加模型回复 |
| `deliverAs: "steer"` | 默认模式，在当前工具批次结束后注入 |
| `deliverAs: "followUp"` | 当前任务完成后注入，再继续处理 |
| `deliverAs: "nextTurn"` | 留到下一次显式发起的运行，在新输入之前加入；优先于 `triggerTurn` |

返回值区分 `status: "appended"`、`status: "queued"` 和原有运行结果状态。
追加和排队返回稳定的 `entryId`；排队消息提交前查询 entry 可能为空。
排队结果中的 `deliverAs` 表示投递方式；静默上下文更新返回 `queue: "context"`。
steering/follow-up 不会重入正在执行的工具，`nextTurn` 也不会被自动恢复运行提前消费。

默认空闲发送现在与 Pi 一致，不再自动请求模型。原来依赖默认发送立即回复的
调用，应显式传 `triggerTurn: true`。

### 工具提示词摘要与规则（与 Pi 一致）

工具 schema 决定模型是否能调用工具；系统提示词只选择性展示摘要，两者独立。
先注册工具，再通过可选 `api.register_tool_prompt` 注册显式元数据：

```rust
if let Some(register_prompt) = api.register_tool_prompt {
    let status = register_prompt(
        rpi_plugin_sdk::StbStringRef::from_str("my_tool"),
        rpi_plugin_sdk::StbStringRef::from_str(
            r#"{"promptSnippet":"工具的简短摘要","promptGuidelines":["该工具的具体使用规则"]}"#,
        ),
    );
    if status != 0 { return status; }
}
```

- 只能为当前插件已经注册的工具设置元数据。返回值：`0` 成功、`1` 未知工具、
  `2` 元数据不是合法对象或字段类型错误、`-1` 不在注册阶段；参数字符串由宿主立即复制。
- `promptSnippet` 可省略；trim 后非空且工具已启用，才出现在提示词工具列表。
  无摘要的工具仍通过 schema 正常可用，不自动把 description 当摘要。
- `promptGuidelines` 可省略；只收集启用工具的规则，trim、去空白，并按启用工具顺序
  与其他工具及核心规则去重。有规则不要求有摘要；宿主不再为外部 todo 工具硬编码规则。
- 元数据随工具定义参与 first-wins 合并及 reload。同名扩展替换内置工具时，不继承
  原内置摘要；卸载或停用工具后，不残留其摘要和规则。空对象清除元数据。
- registrar 只追加到 ABI vtable 尾部，不改变 `StableToolSchema` 或 ABI 版本。
  旧插件仍可加载；旧宿主槽位为 `None` 时跳过元数据注册，不能因此跳过工具注册。
- 自定义 `SYSTEM.md` / `--system-prompt` 仍替换默认提示词；上述自动贡献只用于默认提示词。

## 4. 工具生命周期：execute → poll → cancel → destroy

工具导出四个 `extern "C"` 函数，宿主按固定协议驱动：

- `execute` 解析宿主传入的 JSON（owned `StbString`，用 `free_with` 释放），
  创建每次调用独立的 handle（`Box::into_raw`）。
- `poll` 必须非阻塞；未完成返回 `Pending`，完成返回 `Done` 或 `Err`。
  `partial_cb` 在 poll 内同步调用，宿主在等待终态期间实时转发 partial。
  宿主对 Pending 短暂退避，取消后等待 driver 结束不空转；迟到的成功不覆盖取消。
- `cancel` 只设置线程安全取消标记，必须幂等，不能释放 handle。
- `destroy` 由宿主最终调用一次并释放 handle（空指针 no-op）。

所有权协议：

- 宿主产生的 `StbString`（execute 的 params、事件 payload）由插件通过宿主的
  `free_string` 释放。
- 插件产生的 `StbString`（done 结果、partial 进度、resources_discover 的 out）
  由宿主通过插件注册时提供的 `plugin_free_string` 释放。
- 每个 `extern "C"` 边界都不能 unwind；插件内部捕获 panic 并转换为错误结果。

完整、可运行的 ABI 模板位于 `examples/plugin-stub/src/lib.rs`。创建新工具时先
复制生命周期骨架，再替换参数 schema 和领域逻辑，不要重新设计所有权协议。

### 注入到工具参数里的宿主上下文

宿主在把参数交给插件**之前**，会往这次调用的 JSON 参数里加一个保留键
`__rpi`（`rpi_plugin_sdk::HOST_CONTEXT_KEY`）：

```json
{ "action": "add", "text": "…",
  "__rpi": { "cwd": "D:\\Projects\\pi-rust",
             "sessionId": "01adb026-7230-708b-855b-91eab6b056b8" } }
```

- **模型看不到、也写不了。** 注入发生在模型参数解析之后、`execute` 之前，不
  会进 `ToolExecutionStart`/`Update` 事件，也不会进会话日志；模型若碰巧传了同
  名键，宿主的值覆盖它。
- **两个字段都可能是缺失的。** 拿不到工作目录时没有 `cwd`；ephemeral 会话以及
  早于该注入的旧宿主没有 `sessionId`；整个键也可能不存在。
- **所以 tool 的参数类型必须容忍未知的顶层字段。** 用 `serde_json::Value`
  取值天然满足；给参数定义 `#[serde(deny_unknown_fields)]` 的结构体会直接失败。
- 内置工具不走这条路——它们从 harness 直接拿到 `ExecutionToolContext`。

需要"每个会话一份状态"的插件就用 `sessionId` 做键（`rpi-todo` 存
`.rpi/todos/<sessionId>.json`，一条会话一个文件），取不到时退回项目级并**在
回复里说明**，不要编一个共享的伪会话——那正是它要解决的问题。

## 5. 事件处理器、资源发现与 Provider

### 事件处理器

通过 `api.register_event_handler` 按 `EventTag`（36 类）订阅：

- **会话生命周期**：`SessionStart`、`SessionBeforeSwitch`、`SessionBeforeFork`、
  `SessionBeforeCompact`、`SessionCompact`、`SessionShutdown` 等。
- **Agent 循环**：`BeforeAgentStart`、`AgentStart`、`AgentEnd`、`AgentSettled`、
  `TurnStart`、`TurnEnd`、`MessageStart`/`MessageUpdate`/`MessageEnd`、
  `ToolExecutionStart`/`Update`/`End`、`ToolCall`、`ToolResult`、`UserBash`、`Input`。
- **rpi 特有 / UI**：`BeforeTuiStart`（TUI 初始化前）、`UiPromptStart`/`UiPromptEnd`。

处理器收到 `StablePluginEvent`，不得释放事件里的字符串（宿主 fan-out 后统一释放）。
返回 `0` 成功；返回 `EVENT_HANDLER_ABORT` 表示 veto。

### 生命周期 veto

`BeforeTuiStart` 是带 veto 语义的 rpi 专有钩子：第一个返回 `EVENT_HANDLER_ABORT`
的处理器中止启动，CLI 以退出码 `3` 退出（用于"进入 TUI 前必须先建立外部连接"）。
`SessionStart`/`SessionShutdown` 的 veto 只是 advisory。分发带超时，慢处理器不会
无限阻塞主流程。

### 资源发现

`api.register_resources_discover` 注册 handler，宿主在 `startup`/`reload` 时
fan-out 到所有已注册 handler，合并返回的 `{skillPaths, promptPaths, themePaths}`。
单个 handler 报错不中止 fan-out。`out` 是插件拥有的 `StbString`，宿主通过插件
注册时给的 `plugin_free_string` 回收。

### 运行时改写系统提示词

`api.register_before_agent_start` 注册系统提示词变换器，对应 pi 的
`before_agent_start` handler 返回 `systemPrompt`（成为该次运行的
`forceSystemPrompt`）。这是插件**运行时**改变系统提示词的唯一正规入口：

```rust
// event_json 里带 prompt / imageCount / sessionId / host，以及**当前** systemPrompt。
extern "C" fn on_before_agent_start(
    event_json: StbStringRef,
    out: *mut StbString,
    user_data: *mut c_void,
) -> i32 {
    rpi_plugin_sdk::guard_or(1, || {
        let env: serde_json::Value =
            serde_json::from_str(unsafe { event_json.as_str() }).unwrap_or_default();
        let current = env["systemPrompt"].as_str().unwrap_or("");
        // 返回值是**替换**而非追加：想追加就读当前值再拼回去（pi 同样要求）。
        let next = format!("{current}\n\n本仓库优先使用 `just test` 而非 `cargo test`。");
        unsafe { *out = StbString::from_string(json!({ "systemPrompt": next }).to_string()); }
        0
    })
}
```

语义要点（与 pi 对齐）：

- **替换，不是追加。** 返回的文本成为提示词。想追加必须自己读 `eventJson` 里的
  `systemPrompt` 再拼接——宿主不会帮你保留原文。
- **按注册顺序串联。** 后一个 handler 收到的是前一个 handler 返回的结果。
- **单个 handler 失败不致命。** 返回非零或 panic 时，提示词保持前一个 handler 的
  结果，fan-out 继续（宿主记日志 + 跳过）。
- **只影响该次运行。** 宿主每轮重新从会话状态推导提示词，插件的结果不会被持久化。
- **每个用户 prompt 只算一次。** 一次 prompt 可能触发多轮 provider 调用（工具循环），
  变换器只在第一轮前运行并缓存结果，避免同一运行中途漂移。
- **返回空对象/空串 = 不变。** 只想观察不想改的插件可以什么都不返回；显式返回空
  字符串则是“清空提示词”，会被采纳。

`out` 与其他 out-param handler 一样由插件拥有、宿主经 `plugin_free_string` 回收。
该槽位在结构体末尾新增且可为 `None`：面向旧宿主的插件应检测后再注册（见 §3
能力检测），宿主则在没有插件注册时完全不进入这条路径。

### 改写工具结果

`rpi_plugin_sdk::register_tool_result(api, handler, user_data)` 注册工具结果变换器，
对应 pi 的 `tool_result` handler 返回 `content` / `details`。工具成功返回后、结果进入
会话之前，宿主把 `{"toolCallId", "toolName", "input", "content", "details"}` 交给
handler（`content` 与插件工具返回值同形：`text` 块和带 base64 `data`、`mimeType` 的
`image` 块）；handler 在 `out` 里只写要替换的字段，例如把截图重新编码后返回新的
`content`：

```rust
extern "C" fn on_tool_result(
    event_json: StbStringRef,
    out: *mut StbString,
    _user_data: *mut c_void,
) -> i32 {
    rpi_plugin_sdk::guard_or(1, || {
        let env: serde_json::Value =
            serde_json::from_str(unsafe { event_json.as_str() }).unwrap_or_default();
        let Some(content) = shrink_images(&env["content"]) else {
            return 0; // 不写 out = 不变
        };
        unsafe { *out = StbString::from_string(json!({ "content": content }).to_string()); }
        0
    })
}
```

- **按注册顺序串联**，后一个 handler 看到前一个的结果；返回非零或 panic 时保持原样，
  fan-out 继续。
- **只改返回的字段。** `terminate`、usage 等仍是工具自己的；失败的调用不经过变换器。
- **在阻塞线程上运行**，重新编码图片这类慢活不会卡住运行时；结果按变换后的样子写进
  会话，以后每轮原样重发，提示词缓存不受影响。
- 没有插件注册时宿主不包装任何工具。旧宿主的 `struct_size` 够不到这个槽，
  `register_tool_result` 返回 `-1`。

### Provider 与渲染器

推理 provider 的目录、认证、执行绑定与验收要求见本文末尾的「Provider 接入规范」。

`api.register_provider` 注入自定义 LLM Provider（`provider_id`/`base_url`/
`api_style` + `request_fn`）。`register_message_renderer`/`register_markdown_transformer`/
`register_entry_renderer` 注册消息渲染能力。这些槽位都可能为 `None`，使用前必须检测。

`api.register_streaming_provider` 注入**流式** Provider。注册时同样提供 provider ID、
端点、API style 和字符串释放回调；流式 request fn 另有逐块回调 `emit`、
`emit_user_data`、可选控制面 `control` 和当前宿主恒为 `None` 的 `completion`。
插件在请求还没结束时调用 `emit`，把增量推给宿主：

```json
{"type":"text_delta","contentIndex":0,"delta":"hello "}
{"type":"thinking_delta","contentIndex":1,"delta":"weighing"}
{"type":"toolcall_delta","contentIndex":2,"delta":"{\"path\""}
```

宿主即时把它们映射成 `AssistantMessageEvent`（`TextDelta`/`ThinkingDelta`/
`ToolCallDelta` 等），所以 TUI 在模型还在生成时就开始渲染，而不是等整段返回。
可用的 `type`：`start`、`text_start`/`text_delta`/`text_end`、
`thinking_start`/`thinking_delta`/`thinking_end`、
`toolcall_start`/`toolcall_delta`/`toolcall_end`、`usage`；未知 `type` 忽略（向前兼容）。

`toolcall_end` 可以带一个可选的 `recorded` 字段，表示这次工具调用**已经在远端执行过**
（例如远程 agent 自己跑了工具，再把调用和结果一起流回来）。宿主把 `recorded` 当作已完成的
工具结果，**绝不会在本地再次执行这个调用**，包括审批钩子和工具 `execute` 在内都不会被触发。
`recorded` 的形状与 Cursor replay worker 发出的一致：
`{"version":1,"isError":false,"result":{"content":[{"type":"text","text":"..."}],"details":{},"terminate":true}}`。
`result.content` 只允许 `text` 和 `image` 块；`result.details` 与 `result.terminate` 可省略。
`terminate: true` 会像实时工具结果一样结束这一批工具调用。`{"version":1}` 这类残缺记录
（崩溃前写入）、版本不是 `1` 的记录、字段为 `null`、含未知顶层字段或形状不合法的记录，都会被
宿主当作错误工具结果返回，同样不执行。只有在调用确实已经发生
过时才能带 `recorded`；不带该字段的调用行为与以前完全一致。

**先检查宿主能力。** 不认识 `recorded` 的旧宿主会忽略这个字段，在本地执行调用；C ABI
没有变化也挡不住这一点。请求信封顶层的 `capabilities` 对象说明宿主支持什么：
`capabilities.recordedToolCalls` 是宿主能解码的最高 `recorded` 版本，宣告 `n` 表示能解码
`1` 到 `n` 的每个版本。插件只有在该值 `>= 1`（且不小于自己发送的 `version`）时才可以发送
`recorded`。没有这个键就是旧宿主，插件不得发送 `recorded`，应让这一轮以错误结束，而不是把
已经执行过的调用当普通调用交给宿主。

用 SDK 桥接（`export_provider!`、`provider::drive`、`provider::drive_with_context`）写的
provider 由桥接执行这条检查：宿主没有宣告 `recordedToolCalls >= 1` 时，含 `recorded` 调用、
本可执行的最终消息会变成 `Error` 终态（文本为 `host does not support recorded tool calls`，
不带内容，保留用量）。工厂在桥接调用期间可以用 `provider::host_capabilities()` 读取本次请求的
`capabilities`（旧宿主为 `null`），再交给它构造的 provider。它只在请求线程上有值，其他线程
（包括 `drive_with_context` 下 provider 自己 spawn 的任务）都读到 `null`，所以要在工厂里读取并保存。
桥接自己的检查直接读请求，不受影响。桥接会把 `recorded` 同时放进
`toolcall_end` 块和 `out`。

**`recorded` 与最终 `out`。** 宿主执行的是 `out` 里的调用，所以终态之前会用 `out` 核对每个在
`toolcall_end` 上带过 `recorded` 的调用：按 `id` 找到 `out` 中的同一调用，并要求 `name` 与
`arguments` 相同（按 JSON 值比较：对象键顺序无关，但 `1` 与 `1.0` 不同）。该调用在 `out`
中没有 `recorded` 键时，宿主补上流中的记录（显式 `null` 也照样补上，仍是错误结果、不执行）；
两边记录相同则不变。以下任一情况都会让这一轮以 `Error` 终态结束，本地不执行任何调用：`out`
中没有该 `id`、同一 `id` 出现多次、`name` 或 `arguments` 不同、记录不同，或带 `recorded` 的
`toolcall_end` 没有 `id`。错误文本是固定的，不含调用 `id`，以免被当作可重试的瞬时错误。只在 `out` 中带
`recorded` 同样有效。插件返回非零、`out` 为空或无法解析、请求被取消时，终态本来就是
`Error`/`Aborted`，不会执行任何调用。建议在 `toolcall_end` 和 `out` 中带同一份 `recorded`。

约定：

- **`out` 仍是最终消息。** 回调只送增量；插件返回时写入 `out` 的完整
  `AssistantMessage` 才是终态 `Done`/`Error` 的载荷。宿主会在终态前补一个 `Start`，
  所以即使插件一个块都没发，事件流也合法。
- **回调的所有权与工具 partial 一致。** 每个 chunk 都是插件分配的 `StbString`，宿主
  用插件注册时给的 `plugin_free_string` 回收；`emit`/`emit_user_data` 只在本次请求
  期间有效，插件不得保存。

该槽位同样追加在结构体末尾且可为 `None`：面向旧宿主时应降级到 `register_provider`
（一次性）而不是拒绝加载。同一 `provider_id` 先一次性、再流式注册时，流式会原地替换
该记录（流式是一次性的超集）。可从 `examples/plugin-stub/src/lib.rs` 的
`on_provider_stream_request` 复制完整可运行的骨架。

#### 取消：同一个方法，插件自行决定

长时间生成需要能被中断。**没有单独的「可取消」注册**：`register_streaming_provider`
只有一个方法、一个签名，request fn 永远会收到一个 `control` 参数（宿主提供的同步
控制面）。要不要轮询它，由插件自己决定。

```rust
// 每个 SSE frame 之间（真实 provider 通常在这里阻塞在 socket read 上）：
if (control.is_cancelled)(control.user_data, ProviderChunkKind::Content as u32) != 0 {
    return 1; // 调用方已经走了，停止生成
}
```

- `is_cancelled(user_data, chunk_kind)`：非阻塞轮询。传 `Content`（0）表示
  “模型输出”，传 `Structural`（1）表示 `start`/`*_end`/`usage` 这类收尾帧。
  取消后宿主确认结构帧、拒收内容帧；已发出终态后的收尾帧会被丢弃。
  取消终态保留已转发的内容、模型标识和用量。插件持有 DLL 引用直到请求返回，
  后续写入 `out` 的成功结果不会覆盖取消终态。
- `wait_cancelled_ms(user_data, timeout_ms)`：阻塞版，供无法轮询的循环使用；
  已取消时会立即返回，不会睡满超时。

宿主的取消信号来自 `options.signal`（和内置 provider 同一个 `CancellationToken`，
用户中断时由 agent loop 取消）。取消发生后宿主会：latch 控制面、停止转发内容 delta、
**立即**发出 `Error { reason: Aborted }` 终态并结束事件流——不再等一次没人要的完整
生成。`emit` 对内容帧返回非 0 也算同一个信号，插件可据此退出。

轮询 `control` 是**可选**的，两种写法都合法：

- **不轮询**（fire-and-forget）：插件照常把整段产出推完。取消时宿主仍会停止转发内容
  帧并立刻结束该轮，插件只是多花了它已经产出的那部分 token。
- **轮询**：插件在最早一个可检查点退出，省掉剩余生成。

`control` 可能是 `null`（面向不支持控制面的宿主）；此时当作「不可取消」，不要拒绝加载。

`completion` 参数在**当前宿主上恒为 `None`**：本宿主不允许插件在 request fn 返回后
继续回调（那会让回调指针比调用活得久）。插件必须在 request fn 内收尾并写 `out`；
ABI 里保留该参数是为了将来宿主能扩展，而不必再改签名。

插件只需按 流式 → 一次性 逐级降级：

```rust
if let Some(register) = api.register_streaming_provider { /* 流式，可选轮询 control */ }
else { /* register_provider，一次性 */ }
```

可从 `examples/plugin-stub/src/lib.rs` 的 `on_provider_stream_request`（不轮询）和
`on_cancellable_provider_stream_request`（轮询）各复制一份骨架。

## 6. Rust 工具设计最佳实践

1. 把领域逻辑写成普通 Rust 函数，FFI 函数只负责转换、生命周期和错误映射。
2. 对普通逻辑写 `rlib` 单元测试；对 ABI 注册、加载和卸载写至少一个宿主 smoke test。
3. Schema 和实现使用同一字段命名，拒绝未知字段；对路径、URL、命令、数量和输出大小设置上限。
4. `poll` 不执行长时间阻塞任务。耗时工作放入线程/任务，handle 保存状态；取消标记必须能让任务在有限时间内结束。
5. 全局可变状态使用明确同步，避免把 session 或 tool call 状态存进无保护的 `static mut`。
6. runtime action、provider、renderer、event handler 和 resources handler 都是可选能力；注册前检查 vtable slot。
7. 插件注册失败返回非零；单个工具执行失败返回结构化错误，不要让整个宿主退出。
8. README 记录工具 schema、权限边界、持久化文件、网络访问、支持平台和卸载方式。

## 7. 使用 rpi dev 开发

在扩展 crate 目录运行：

```bash
rpi dev
```

rpi 会通过 Cargo metadata 识别当前 `cdylib`，首次编译后把版本化副本放在工作区
`.rpi/extensions/.dev/`，启动正常 TUI，并监控 `src/`、`build.rs`、package/workspace
`Cargo.toml` 和 `Cargo.lock`。源文件变化后会自动编译并走现有 reload 流程；编译失败
会保留当前已加载版本。

多扩展 workspace 必须明确选择 package：

```bash
rpi dev --package rpi-todo
rpi dev -P rpi-webfetch
```

其他模式：

```bash
rpi dev --release
rpi dev --no-watch
rpi dev-local
rpi dev --package rpi-todo -- --model gateway/model
```

调试单个扩展时使用 `rpi dev-local`（等价于 `rpi dev --local-only`）。它只加载当前
编译的扩展、当前项目目录的 skill/prompt，以及该扩展发现的 skill/prompt；不会扫描
全局扩展、已安装 Pi 包或其他项目资源，避免同名命令和无关命令干扰调试。内置命令
（包括 `/reload`）仍然可用。没有 Cargo cdylib 时优雅降级为 skills-only 模式。

TUI 中执行 `/reload` 会强制重新运行 Cargo build，再加载新阶段产物。Windows 上
已加载 DLL 无法原地覆盖，因此不要手工把 Cargo target DLL 复制到固定文件名；
`rpi dev` 的版本化 staging 会安全地完成切换，并在会话退出、loader 释放后清理。

## 8. 资源与配置约定

项目级资源放在 `.rpi/`。同名资源优先级是项目内定义优先
高于 `.pi`，项目高于全局，package 最后：

```text
.rpi/
├── SYSTEM.md
├── APPEND_SYSTEM.md
├── settings.json
├── skills/
├── prompts/
├── themes/
└── extensions/
```

动态 `resources_discover` 适合根据运行环境决定路径；固定资源优先使用 manifest 或
约定目录，便于安装器、欢迎页和 `/context` 在扩展代码运行前发现它们。

### 自定义加载路径

项目级 `.rpi/settings.json` 可以声明额外资源路径；无需其他兼容
回退。路径相对于项目根目录，配置路径会排在约定目录前面：

```json
{
  "skillDirs": ["./team-skills"],
  "promptDirs": ["./prompts/shared"],
  "extensionDirs": ["./target/debug"],
  "packages": ["./packages/review-tools"]
}
```

`skills`、`prompts`、`extensions` 也可分别作为三个路径数组的简写。全局
`~/.rpi/agent/settings.json` 支持相同的 `skillDirs`、`promptDirs`、`extensionDirs`，
其中相对路径相对于全局 agent 目录。项目配置优先于全局配置；`.rpi` 资源和设置排在
`.pi` 之前。

单次运行可以显式追加路径：

```bash
rpi --skill ./extra-skills
rpi --prompt-template ./extra-prompts
rpi --extension ./target/debug/my_extension.dll
rpi --extensions-dir ./target/debug
```

`--skill`、`--prompt-template`、`--extension` 和 `--extensions-dir` 都可以重复使用。
额外的 Rust 扩展目录也可通过 `RPI_EXTENSIONS_DIR` 配置；Windows 使用分号分隔，
Linux/macOS 使用冒号分隔。命令行和环境变量适合本机开发、CI 与排错；团队共享配置
使用项目 `.rpi/settings.json`。

## 9. 发布检查清单

- `cargo fmt --check`、`cargo clippy --all-targets`、`cargo test` 通过。
- `crate-type` 包含 `cdylib`，使用 `export_plugin!` 导出
  `rpi_plugin_register`。
- 依赖已发布的 `rpi-plugin-sdk` 兼容版本，不链接宿主私有 crate。
- `rpi dev --no-watch` 能编译、加载并注册预期工具。
- `rpi install <crate> --build --force` 的干净安装路径通过。
- Linux/macOS/Windows 构建由 CI 验证，README 标明支持矩阵。

## 10. 模型执行规则

模型收到"创建 rpi Rust 插件、扩展工具"任务时：

1. 先调用 `docs` 查询 `authoring`（扩展开发）或 `debugging`（调试）。
2. 阅读当前仓库现有 manifest、SDK 版本和相邻扩展约定，不猜 API。
3. 优先复制 `examples/plugin-stub` 的最小骨架。
4. 实现领域逻辑与边界测试，再连接 loader/ABI。
5. 使用 `rpi dev --no-watch` 与 `rpi install <crate> --build --force` 做真实 smoke test。
6. 不声称未验证的 Pi capability 已完全兼容；明确记录降级行为。

## Provider 接入规范

统一生产包为 `rpi-providers`，一个安装入口包含 DeepSeek、OpenRouter、llama.cpp 和 Proxy。
新增供应商优先在包内添加模块；只有根模块导出一次 `export_plugin!`，各模块共用注册桥接。
provider ID、目录与凭据继续独立。单 provider 教学样例才使用 `export_provider!`。

完整规范：[Provider 接入规范](https://github.com/bigfish1913/pi-rust/blob/feature-stream-porider/docs/extensions/provider-integration.md)。创建推理扩展时优先复制
正式 provider package 示例见 [rpi-providers](https://github.com/pi-rust/rpi-package/tree/feature-1014/packages/rpi-providers)，通用工具扩展再使用 `plugin-stub`。当前接入约定如下。

### 方式与目录

只改已支持协议的端点、模型和 key 时，优先使用 `models.json`；需要可安装目录、
认证策略或供应商逻辑时，使用独立 package。新供应商不添加为 CLI 的默认依赖。
Provider 负责推理，auth 负责凭据和登录流程，两者独立注册。

通过 `declare` 的 `providers` 数组声明 `id`、完整 `rpi_ai::Model` 数组 `models`、
可选 `apiKeyEnv` 和默认 `true` 的 `requiresAuth`。用 `Model::new` 构造后序列化，
不能使用 models.json 的简写对象。provider ID、每个模型的 `provider`、实际注册 ID
必须一致；model ID 是发送给服务端的名称。匿名服务必须显式设置 `requiresAuth: false`，
协议客户端也必须允许匿名，不能只修改目录声明。

目录只在同一插件成功注册对应 provider 后生效，同名冲突时保留第一份实现及其目录。
注册和目录发现不要求凭据；宿主在选择模型前加载扩展、合并 models.json 覆盖。
`--list-models` 可无凭据显示目录；`--no-extensions` 同时禁用实现和声明。

### Factory 与私有执行绑定

统一包内复用 `crate::registration::register_provider!`；单 provider 教学包可使用
`rpi_plugin_sdk::export_provider!`，需显式开启 SDK 默认关闭的 `provider` feature。模块提供
`fn(&Model, &SimpleStreamOptions) -> Arc<dyn Provider>`。OpenAI-compatible 接入可复用
`rpi_plugin_sdk::provider::completions(model, options, anonymous)`，无需复制 SSE 解析器。
该 helper 返回 `Result`；通过 `ContextFactory` 传播客户端配置错误，或使用
`context.completions(model, options, anonymous)` 复用客户端。
宿主私有 `ProviderFactory` 统一选择扩展 / 内置协议，不是插件需要实现的接口。
运行时依赖使用 `rpi-ai` 与开启 `provider` feature 的 `rpi-plugin-sdk`，不依赖 `rpi-cli`；宿主加载器
可以作为测试依赖。

公开目录、当前模型及配置后 `Provider::models()` 返回的描述不携带请求头。
宿主私有 `ExecutionBinding` 保存实际端点、模型请求头及默认认证，执行前按
provider / API / model ID 恢复；修改公开描述不能重定向绑定的凭据。
`base_url` 仍公开用于展示，每次请求显式传入的 options 仍按既有规则生效。
插件必须使用请求传入的 model / options，不能改用硬编码端点或进程环境变量中的 key。

### 认证与重载

默认凭据由宿主按 provider ID 解析：选中 provider 的 `--api-key` → `auth.json`
有效 key / OAuth → `models.json` → 声明的 `apiKeyEnv` → 显式允许的匿名。
协议名不决定认证，不继承 `OPENAI_API_KEY`；空白、未解析引用和无法刷新的过期 token 不算可用。
目录不得含凭据；请求参数、token 与私密请求头不得进入日志、普通事件或错误文本。
OAuth 供应商流程另外通过 `register_oauth_provider` 接入，宿主负责通用命令与存储。

加载匹配 OAuth 扩展且存储项含 refresh token 时，宿主可选择过期登录对应的 provider，
并在每次请求前自动刷新已过期或 60 秒内到期的 token，先保存再使用；刷新失败不发送旧 token。
`auth check` 只检查本地未过期凭据，不访问服务端或刷新 token，可能在请求仍可自动刷新时报告缺少凭据；
退出码为就绪 0、缺少凭据 1、配置错误 2。
`/provider <id>` 与列表选择会即时切换当前会话并保存默认模型；缺少认证或切换失败时
保留原会话与默认值。`/model` 选择具体模型；`/provider remove` 删除配置，
卸载 package 使用 `rpi package unlink`。API-key logout 不会清除环境变量或 models.json。

涉及 provider package 的 `/reload` 会重新绑定 runtime、凭据、端点与公开目录；
当前 package 消失或无法重新解析时返回错误并保留旧会话。仅内置 provider 的会话仍使用
既有资源重载逻辑，不承诺重新解析所有 provider 配置；完整重载尚非原子代次切换。

### 流式与验收

增量必须在生成期间发出，最终 `out` 仍提供完整消息，宿主只产生一个 Done / Error。
取消要及时停止请求并保留部分输出；同步 request fn 返回后禁止继续调用 `emit`。
使用桥接库处理 ABI 字符串释放、取消与 runtime 生命周期，手写 ABI 遵守前述协议。

验收必须包含真实动态库加载、本地 SSE 在首个正文增量到达后才继续发送的门控测试、
UTF-8 跨块、usage、唯一终态、响应未结束时取消与 HTTP 401；认证隔离、端点 / 请求头
覆盖、公开目录无认证头、命令与重载也要验证。工具和 OAuth 只声明已完成验证的能力。
真实 API 测试默认 ignored，显式运行时从环境注入 key 并限制超时、重试和用量。
正式统一包位于独立的 [rpi-package 项目](https://github.com/pi-rust/rpi-package/tree/feature-1014/packages/rpi-providers)，
保留 `deepseek` ID，声明 `DEEPSEEK_API_KEY`，并通过 `requiresReasoningContent` 支持工具续轮。

在 rpi-package 仓库运行正式 package 的离线验证：

```sh
cd /path/to/rpi-package
cargo build -p rpi-providers
cargo test -p rpi-providers --test streaming
cargo test -p rpi-providers --test host
```

在 pi-rust 仓库运行 CLI 集成测试时设置 `RPI_TEST_PROVIDER_LIBRARY` 和
`RPI_TEST_PROVIDER_PACKAGE`，再执行：

```sh
cargo test -p rpi-cli --test provider_packages
```

新增 package 还必须构建并测试自身动态库，不能仅凭宿主测试通过就声称接入完成。
