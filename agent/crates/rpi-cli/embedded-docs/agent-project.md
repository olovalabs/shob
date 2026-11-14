# rpi Agent 项目结构创建指南

本指南是模型可读的默认规范。当任务是用 rpi SDK"创建一个自己的 Agent"、
"把 Agent 结构搭起来"或"给现有项目加 rpi 入口"时，按本文组织项目。默认推荐单 crate：
内嵌 Agent 与全局 `rpi` 扩展共用同一套工具（见 §1），其余形态按需裁剪。

不要先写工具再想结构。先确定形态，再落目录，最后接入口。

## 1. 先选形态

| 形态 | 启动入口 | 直接依赖 | 适用场景 |
| --- | --- | --- | --- |
| 库内嵌 Agent | 你的 `[[bin]]` / 服务进程 | `rpi-agent` + `rpi-ai`（需要内置工具再加 `rpi-tools`） | 在自己的进程里跑 loop，完全掌控事件、工具和输出 |
| CLI 项目 | 全局 `rpi` 宿主 | 无（资源与 cdylib） | 想要现成的 TUI、`-p`/`--mode json`、会话和 skills |
| Rust 扩展 | 宿主加载 `.rpi/extensions/*.dll` | `rpi-plugin-sdk` | 给全局 `rpi` 加工具/Provider/事件，不改宿主 |

一个项目**可以同时提供"内嵌 Agent"和"cdylib 扩展"两份适配器，共享同一套工具实现**。
这也是本指南默认推荐的形态：本地用内嵌 Agent 调试，线上/交互用全局 `rpi` 加载
同一个扩展。只有当需求足够单一时才只做其中一种。

如果扩展还承担 Web/ACP 或其他 native RPC 适配器，需要会话、工作区、事件重放或
durable 状态时，可以复用 crates.io 的 `rpi-extcore = "0.1"`。它是普通 Rust 服务库，
不替代 `rpi-plugin-sdk` 的 `cdylib` ABI，也不自行提供 HTTP/ACP 前端；普通工具插件
保持 `rpi-plugin-sdk` 单一依赖即可。

## 2. 推荐目录结构

```text
my-agent/
├── Cargo.toml
├── AGENTS.md                  # 项目级兜底指令（裸 rpi 也会读）
├── config/
│   └── agent.json             # 项目 Agent 配置（system prompt / skill dirs / artifacts）
├── prompts/
│   ├── system.md              # 主系统提示词
│   └── append-system.md       # 追加系统提示词
├── skills/
│   └── <skill-name>/SKILL.md  # 领域技能，运行时按描述/触发词动态激活
├── src/
│   ├── main.rs                # 内嵌 Agent / 服务入口
│   ├── lib.rs                 # 模块导出（+ 可选 cdylib ABI 适配）
│   ├── config.rs              # 运行配置（环境变量 / 文件）
│   └── tools/
│       ├── mod.rs             # 唯一工具登记表（single source of truth）
│       ├── core.rs            # run_tool 单一入口 + 领域逻辑
│       └── content.rs         # AgentTool 适配器（内嵌 Agent 用）
├── .rpi/
│   ├── settings.json          # skillDirs / promptDirs / extensionDirs / defaultTools
│   └── extensions/            # 编译后的 cdylib（生成物，不提交）
└── tests/
    └── behavior.rs
```

约定：

- `.rpi/` 是 rpi 的项目资源目录；**同名资源以项目内定义优先**
  （项目优先于全局，`.rpi` 优先于 `.pi`，package 最后）。
- `prompts/`、`skills/`、`config/` 是**可审查的普通源码目录**，不要藏进 `.rpi`
  或依赖扩展运行期临时生成的路径。
- `target/`、`.rpi/extensions/`、`artifacts/` 是生成物，写入 `.gitignore`。
- 系统提示词把生成约束写成固定文本，不要依赖模型在每次会话里重新推断。

## 3. Cargo.toml

内嵌 Agent + cdylib 同源时，一个 package 同时声明 `[[bin]]` 和 `[lib]`：

```toml
[package]
name = "my-agent"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "agent"
path = "src/main.rs"

[lib]
name = "my_agent"
path = "src/lib.rs"
crate-type = ["cdylib", "rlib"]   # cdylib 给宿主；rlib 便于集成测试

[dependencies]
# 下面用 `<rpi>` 代指当前 rpi 发布版本，请替换为实际版本号（如 `0.1.x`），
# 并保证所有 `rpi-*` 依赖同版本；第三方依赖按需增删。
rpi-agent = "<rpi>"
rpi-ai = { version = "<rpi>", features = ["providers"] }
rpi-tools = "<rpi>"
rpi-plugin-sdk = "<rpi>"          # 只有 cdylib 适配需要
tokio = { version = "1", features = ["full"] }
async-trait = "0.1"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
schemars = "0.8"
thiserror = "1"

[dev-dependencies]
# 用 faux provider 做离线测试，不依赖网络或 API key
```

边界规则：

- **扩展只依赖 `rpi-plugin-sdk`，绝不依赖 `rpi-cli`、`rpi-extensions`、`rpi-harness`
  内部模块**——宿主链接插件，不是插件链接宿主，否则 ABI 和版本会耦合。
- 内嵌 Agent 只依赖 `rpi-agent`/`rpi-ai`/`rpi-tools`，与扩展 ABI 解耦。
- 需要会话、压缩、skills、上下文文件时再加 `rpi-harness`。

## 4. 工具单一登记点

工具只声明一次，两个宿主适配器从同一份声明派生各自的 schema，避免双实现漂移。

`src/tools/mod.rs`：

```rust
pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub props: serde_json::Value,        // JSON Schema properties
    pub required: &'static [&'static str],
}

pub static TOOLS: std::sync::LazyLock<Vec<ToolSpec>> = std::sync::LazyLock::new(|| {
    vec![ToolSpec {
        name: "lookup",
        description: "查找项目中的符号定义。",
        props: serde_json::json!({
            "symbol": {"type": "string", "description": "要查找的符号名"}
        }),
        required: &["symbol"],
    }]
});

/// 所有宿主共用的唯一执行入口。
pub fn run_tool(name: &str, params: &serde_json::Value)
    -> Result<serde_json::Value, ToolError>
{
    match name {
        "lookup" => lookup::run(params),
        other => Err(ToolError::new(format!("unknown tool `{other}`"))),
    }
}
```

`src/tools/core.rs` 放共享逻辑；`src/tools/content.rs` 只做编排（把 `run_tool`
包成 `rpi_agent::AgentTool`）；cdylib 侧只做 ABI 编排。三者都不重复实现领域逻辑。

为每个工具加一个断言 schema 在两个宿主之间一致的单元测试（见 §11）。

## 5. 内嵌 Agent

最小闭环：构造 provider/model → `AgentBuilder` → 订阅事件 → `prompt`。

```rust
use std::sync::Arc;
use rpi_agent::AgentBuilder;
use rpi_ai::Provider;

let model = provider.default_model().clone();
let agent = AgentBuilder::new()
    .model(model)
    .system_prompt(compose_system_prompt())     // prompts/system.md + append
    .tools(create_agent_tools())                // 从 TOOLS 派生
    .stream_fn(make_stream_fn(provider))        // Provider::stream_simple 的同步封装
    .build()?;

let mut events = agent.subscribe();
agent.prompt(user_input).await?;

while let Some(event) = events.recv().await {
    if matches!(event, rpi_agent::AgentEvent::AgentEnd { .. }) {
        break;
    }
    // 转发 MessageUpdate / ToolExecutionStart / ToolResult 给你的 UI 或日志
}
```

要点：

- **先 `subscribe` 再 `prompt`**，否则广播缓冲里会漏掉早期事件。
- 自定义工具实现 `rpi_agent::AgentTool`：`schema()` 返回 `rpi_ai::types::Tool`，
  `execute(tool_call_id, params, signal, on_update)` 解析 JSON、检查 `signal`、
  返回 `AgentToolResult`。把参数解析失败映射成结构化错误，不要 panic。
- 用 faux provider + `InMemoryExecutionEnv` 做离线测试：无需网络、API key 或真实文件系统。
- 需要 CLI 级别的会话/压缩/skills 时，用 `rpi-harness` 而不是自己重造。

## 6. cdylib ABI 适配（给全局 `rpi` 用）

宿主只加载统一 C ABI 符号 `rpi_plugin_register`。扩展使用 `export_plugin!`，
通过同一个 `PluginApi` 注册能力并调用 `api.declare` 声明优先级/平台。
SDK 在注册前检查 `abi_version` 和 `struct_size`；不兼容时返回非零状态，宿主报告加载失败。

```rust
rpi_plugin_sdk::export_plugin!(|api| {
    let Some(register_tool) = api.register_tool else {
        return -1;                         // 能力检测：槽位缺失就明确失败
    };
    // 从同一份 TOOLS 声明构造 StableToolSchema 与
    // execute/poll/cancel/destroy 函数表，然后 register_tool(...)。
    let _ = register_tool;
    0
});
```

工具生命周期固定为 `execute → poll → cancel → destroy`：`execute` 分配每次调用的
handle，`poll` 必须非阻塞并响应取消标记，`cancel` 只置位不释放，`destroy` 由宿主
调用一次并释放。任何 `extern "C"` 边界都不能 unwind。

**先复制 `examples/plugin-stub/src/lib.rs` 的骨架**，再替换 schema 和领域逻辑；
不要重新设计所有权协议。完整的 ABI、能力检测、事件处理、资源发现与发布检查见
`docs` 的 `authoring` 主题。

## 7. 项目资源：prompts、skills、AGENTS.md

- `prompts/system.md`：主系统提示词，放职责、流程、质量门禁（如"每个内容项目先
  `read` 对应 skill 的 `SKILL.md`"）。运行期通过 `--system-prompt` 或配置注入。
- `prompts/append-system.md`：追加段，用于环境/项目特定约束。
- `skills/<name>/SKILL.md`：领域技能。模型从启动时 `<available_skills>` 的
  `name`/`description` 动态选择，**不要维护硬编码的主题→技能映射**。
  发现（`loaded`）不等于使用；真正的激活证据是先完整 `read` 该 `SKILL.md`。
  frontmatter 另支持 `disable-model-invocation: true`：该 skill 不进入
  `<available_skills>`，只能由用户经 `/skills <name>` 显式调用。
- `AGENTS.md`：项目根兜底指令，裸 `rpi` 也会读取。与 `prompts/system.md` 的
  核心约束保持一致，改一个就同步另一个。
- 系统提示词与 skills 在**每次会话启动时**加载快照；改完要重启会话，已有会话不会热替换。

## 8. `config/agent.json`（项目约定）

`.rpi/settings.json` 是 rpi 宿主的原生配置。`config/agent.json` 是**项目自己的**
约定文件，供内嵌 Agent 启动器或包装脚本读取，让"系统提示词 / 技能目录 / 产物目录 /
默认 provider"集中在一处：

```json
{
  "system_prompt": "prompts/system.md",
  "append_system_prompt": "prompts/append-system.md",
  "skills_dir": "skills",
  "skill_dirs": ["skills"],
  "extension_dirs": [".rpi/extensions"],
  "disable_context_files": true,
  "artifacts_dir": "artifacts",
  "provider": "<default-provider>"
}
```

以上取值按项目实际使用的提供者与目录填写，不要照抄示例；`config/agent.json`
不是 rpi 原生配置，只是本项目启动器的约定。同时用 `.rpi/settings.json` 让宿主
原生发现资源：

```json
{
  "skillDirs": ["./skills"],
  "defaultTools": ["read", "bash", "edit", "write", "docs"]
}
```

`disable_context_files: true` 可避免包装启动器被上级仓库的 `AGENTS.md`/`CLAUDE.md`
污染；需要时再用 `--no-context-files` 显式关闭。

## 9. 启动与开发循环

两套入口，脚本只负责固定 cwd、编译和转发参数。脚本语言按平台选择即可
（Windows `.ps1`/`.cmd`，Unix `.sh`，或统一用 Taskfile/just/make 等 runner），
两套入口的职责保持一致：

- **内嵌 Agent**：一个启动脚本 → `cargo run --bin agent`（本项目进程，直连 SDK）。
- **全局宿主**：一个构建脚本编译 cdylib 并复制到 `.rpi/extensions`，再由一个启动
  脚本拉起全局 `rpi`。脚本应：
  1. 固定工作目录到项目根，避免加载到别的项目的 skills；
  2. 让宿主从 `.rpi/extensions` **原生扫描**，不要重复传同一个 `--extensions-dir`；
  3. 把 `prompts/system.md`、`prompts/append-system.md` 显式传给宿主；
  4. 只在没有更高优先级设置时选默认 provider/model，打印来源但**不打印 API key**；
  5. fail-closed：检测到 `--no-tools`、`--no-skills`、`--no-extensions` 或
     `--tools` 白名单缺少 `read` 时先失败——这些会阻止加载内容扩展或读取 skill。
- Windows 上已加载的 DLL 无法原地覆盖：复制前比较 SHA256，字节未变就跳过；
  被占用时给出"停止运行中的 rpi 后重试"的可操作错误，而不是静默失败。

编辑循环：改 Rust 扩展用 `rpi dev`（watch + 热重载，版本化 staging 到
`.rpi/extensions/.dev`）；只调当前扩展用 `rpi dev-local`（隔离模式）。不要手工把
Cargo target DLL 复制成固定文件名。

## 10. 服务化 Agent（可选）

把 Agent 作为长驻服务时，`main.rs` 用项目已有的 HTTP 框架（如 `axum`、`actix-web`
等，任选其一）暴露接口，需要时再加消息队列消费者（MQTT/Kafka/NATS 等）：

- `main.rs` 只做编排：初始化 `tracing` → 加载 `Config::from_env()` → 建 `AppState`
  → 启动 HTTP server 并接优雅停机。
- 配置全部走环境变量（`.env.example` 记录键名，**不提交真实密钥**）。
- 任务/运行状态放共享 `Arc<TaskStore>`，API handler 保持薄。
- 需要容器时提供多阶段 `Dockerfile`；运行阶段只带二进制和 CA 证书，
  `EXPOSE`/`CMD` 指向 `main.rs` 的入口。

## 11. 测试与发布

- `cargo fmt --check`、`cargo clippy --all-targets`、`cargo test` 全绿。
- 领域逻辑写 `rlib` 单元测试；ABI 注册/加载写至少一个宿主 smoke test。
- 断言**两个宿主的 schema 一致**：同一份 `TOOLS` 派生的 Agent schema 与 ABI JSON
  必须相等，防止漂移。
- `rpi dev --no-watch` 能编译、加载并注册预期工具；`rpi install <crate> --force`
  的干净安装路径通过。
- 提供 `cdylib` 时，`crate-type` 包含 `"cdylib"`，导出
  `rpi_plugin_register`，依赖已发布的 `rpi-plugin-sdk`。
- `README` 写明工具 schema、权限边界、持久化文件、网络访问、支持平台和卸载方式；
  `AGENTS.md`、`config/agent.json`、`prompts/`、`skills/`、`.rpi/settings.json`
  之间的路径保持一致。

## 12. 创建检查清单

1. 选定形态（内嵌 / CLI / cdylib；推荐内嵌 + cdylib 同源）。
2. 建目录：`src/tools/`、`prompts/`、`skills/`、`config/`、`.rpi/`。
3. `Cargo.toml` 声明 `[[bin]]` 与（如需 cdylib）`[lib] crate-type`。
4. 写工具登记表 `TOOLS` + 唯一 `run_tool`，再写两个适配器。
5. 写 `prompts/system.md` 与 `AGENTS.md`，约束保持一致。
6. 写 `config/agent.json` 和 `.rpi/settings.json`。
7. 接内嵌 Agent 入口；需要全局宿主时复制 `examples/plugin-stub` 的 ABI 骨架。
8. 加脚本：编译扩展、启动内嵌 Agent、启动全局 rpi。
9. 加 schema 对齐测试和至少一个端到端 smoke test。
10. 用 `--debug-system-prompt` 确认项目提示词与动态 skill 路由进入最终 composed prompt。

## 13. 模型执行规则

模型收到"创建 rpi Agent 项目 / 搭 Agent 结构"任务时：

1. 先调用 `docs` 查询 `agent`（本指南）；扩展开发查 `authoring`，调试查 `debugging`。
2. 读当前仓库现有 `Cargo.toml`、SDK 版本与相邻项目约定，不猜 API。
3. 先落目录和单一工具登记点，再写适配器，最后接入口。
4. 优先复制仓库内最小骨架（`examples/minimal`、`examples/plugin-stub`）。
5. 用 faux provider 做离线测试，用 `rpi dev --no-watch` 做真实加载 smoke test。
6. 不声称未验证的 ABI/capability 已完全兼容；如实记录降级行为。
