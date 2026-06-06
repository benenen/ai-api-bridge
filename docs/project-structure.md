# ai-api-bridge 项目结构文档

## 概述

`ai-api-bridge` 是一个本地运行的 AI API 协议翻译代理，用 Rust 编写。它接受 Codex（OpenAI Responses API）和 Claude Code（Anthropic Messages API）的入站请求，将其翻译为上游提供商的 OpenAI Chat Completions 调用，再将流式响应翻译回客户端的格式。桥接器支持多提供商、基于别名的模型路由、主动/被动故障转移，以及通过 Lua 脚本实现的配额和可用性探测。

## 目录结构

```
ai-api-bridge/
├── bridge.toml              # 运行时配置（不入 Git）
├── bridge.example.toml      # 配置模板（入 Git）
├── Cargo.toml               # Rust 依赖与包元数据
├── Cargo.lock               # 锁定的依赖版本
├── CLAUDE.md / AGENTS.md    # AI 助手指令（符号链接）
├── Cross.toml               # 交叉编译配置
├── Dockerfile               # Docker 构建
├── LICENSE                  # 许可证
├── README.md                # 项目说明
├── bridge.db                # SQLite 数据库文件（不入 Git）
│
├── src/                     # 源代码
│   ├── main.rs              # 入口：解析 CLI、加载配置、启动服务
│   ├── lib.rs               # crate 模块声明
│   ├── config.rs            # TOML 配置解析与 Provider/Route 数据结构
│   ├── server.rs            # axum HTTP 服务、路由注册、请求处理管道
│   ├── router.rs            # 模型别名 → 候选提供商的解析与排序
│   ├── upstream.rs          # reqwest 上游 HTTP 客户端
│   ├── watcher.rs           # 后台探测任务管理（spawn/reconcile）
│   ├── probe.rs             # 单次探测：Lua 脚本执行或连通性 ping
│   ├── store.rs             # SQLite 数据访问层（providers/routes/status/usage_events CRUD）
│   ├── usage.rs             # 用量累加器：按类型(billing/count/token)的滚动窗口 + 估算
│   ├── admin.rs             # 管理 API + 嵌入的管理页面
│   ├── error.rs             # 统一错误类型与 HTTP 状态码映射
│   ├── canonical.rs         # 提供商无关的请求/流事件中间表示
│   ├── sse.rs               # SSE 流解码器
│   └── wire/                # 各协议格式的解析与序列化
│       ├── mod.rs           # CanonicalEmitter trait + SseFrame 类型
│       ├── chat.rs          # OpenAI Chat Completions（出站格式）
│       ├── responses.rs     # OpenAI Responses API（入站格式）
│       └── anthropic.rs     # Anthropic Messages API（入站格式）
│
├── web/
│   └── admin.html           # 嵌入的管理控制台（单文件离线可用）
│
├── probes/                  # 内置 Lua 探测脚本
│   ├── opencode-zen.lua     # OpenCode Zen 可用性探测（/models 端点）
│   └── generic-credits.lua  # 通用配额探测模板（/credits 端点）
│
├── migrations/              # SQLite 迁移
│   ├── 0001_init.sql        # 初始表：providers + routes
│   ├── 0002_provider_status.sql  # 增加探测配置字段 + provider_status 表
│   ├── 0003_usage.sql       # usage_events 表 + providers.cost_windows 列
│   ├── 0004_model_prices.sql # providers.model_prices 列（token×单价估算）
│   └── 0005_usage_types.sql # providers.usage 列；usage_events: cost→amount + usage_type
│
├── docs/                    # 文档
│   ├── configuration.md     # 完整配置参考
│   ├── project-structure.md # 本文件
│   └── superpowers/         # 设计规格与实现计划
│       ├── specs/
│       └── plans/
│
└── tests/                   # 集成测试
```

## 模块详解

### `main.rs` — 应用启动

处理 CLI 参数（`--config`、`--listen`、`--db`、`--reseed`），加载 TOML 配置，打开 SQLite 数据库，执行首次种子填充或 `--reseed` 重建，从 DB 加载 provider/route 快照，应用环境变量覆盖，启动后台 watcher，然后绑定并启动 axum HTTP 服务。

```
CLI 参数 → Config::load() → store::open() → store::seed/load → watcher::spawn() → axum::serve()
```

### `config.rs` — 配置数据结构

定义 `Config`、`Provider`、`Route`、`RouteTarget`、`WireName`、`UsageSpec` 等核心结构体：

- **Config**: `listen`、`database`、`default_provider`、`auth_token`、`cost_tracking`（用量统计总开关，默认 false）、`providers`（HashMap）、`routes`（Vec）
- **Provider**: `wire`（协议）、`base_url`、`api_key`、`model_prefix`、`max_tokens_field`、`extra_headers`、`probe_script`、`probe_enabled`、`probe_interval_secs`、`quota_min`、`usage`（Vec\<UsageSpec\>）+ 旧 `cost_windows`/`model_prices`（`normalize_usage()` 折算为 billing）
- **UsageSpec**: 带 `#[serde(tag="usage_type")]` 的枚举 = `billing`{windows, model_prices} | `count`{windows} | `token`{windows}；`UsageKind` 提供 unit/as_str/parse，`UsageWindow` = {label, window_secs, limit}
- **Route**: `alias`（客户端模型名）→ `provider` + `model`（上游模型 ID）+ `fallback` 候选列表
- **WireName**: `openai-chat` | `openai-responses` | `anthropic-messages`

Provider 的 api_key 可以通过环境变量 `BRIDGE_PROVIDERS_<NAME>_API_KEY` 注入，避免明文写入配置文件。

首次运行后，provider 和 route 由 SQLite 管理（配置文件仅作种子），`listen`/`default_provider`/`auth_token` 仍从 TOML 读取。

### `server.rs` — HTTP 服务与请求管道

用 axum 注册所有端点，路由处理三种入站协议：

| 端点 | 协议 | 用途 |
|---|---|---|
| `POST /v1/responses` | OpenAI Responses | Codex 客户端 |
| `POST /v1/messages` | Anthropic Messages | Claude Code 客户端 |
| `POST /v1/chat/completions` | OpenAI Chat Completions | 透传模式 |
| `GET /v1/models` | — | 列出配置的别名 |
| `GET /v1/providers` | — | 提供商状态（watcher + 用量） |
| `GET /health` | — | 健康检查 |
| `/` `/admin` | HTML | 管理控制台 |
| `/admin/api/providers` `/admin/api/routes` | REST | 提供商 / 路由 CRUD |
| `GET/POST /admin/api/usage` | REST | 用量统计开关 |

**流式请求处理管道**（Responses / Messages）：

```
解析入站请求 → CanonicalRequest
     → router::resolve_candidates() → 候选链
     → upstream::post_stream() → Chat Completions SSE 字节流
     → SseDecoder → JSON chunks
     → chat::ChatStreamParser → CanonicalEvent 流
     → CanonicalEmitter（ResponsesEmitter / AnthropicEmitter）→ 客户端格式 SSE
```

**非流式请求处理管道**：

```
解析入站请求 → CanonicalRequest
     → router::resolve_candidates() → 候选链
     → upstream::post_json() → Chat Completions JSON
     → chat::completion_to_events() → CanonicalEvent 列表
     → CanonicalEmitter.final_response() / .final_message() → 客户端格式 JSON
```

**Chat Completions（`/v1/chat/completions`）**是透传的：入站和出站都是同样的 Chat Completions 格式，直接转发字节流/JOSN，不做协议翻译。

`reload_from_db()` 在管理 API 写入后被调用：重新从 DB 加载 provider/route，交换配置快照，重启 watcher 任务，清理已删除 provider 的状态。

### `router.rs` — 模型路由

将客户端请求的模型名解析为有序的候选提供商链：

1. 精确匹配 `[[routes]]` 中的 alias → 使用 route 的 provider + model + fallback 列表
2. 无匹配 → 使用 `default_provider`，应用 `model_prefix`（如果模型名不含 `/`）
3. 以上都不满足 → `400 unknown model`

候选链按健康状态重新排序：可用提供商排在最前，不可用或配额耗尽的排在末尾（作为最后手段）。

`is_usable()` 判断逻辑：
- 无状态（未监控）→ 假定可用
- `available = false` → 不可用
- `quota_remaining < quota_min` → 不可用

### `upstream.rs` — 上游 HTTP 客户端

封装 reqwest，提供两个上游调用方法：

- `post_stream()`: 返回字节流（SSE），仅设置 headers 超时（30s），不限制 body 传输时间
- `post_json()`: 返回 JSON，全请求超时 60s

自动附加 `Authorization: Bearer` 和 `extra_headers`。

### `watcher.rs` — 后台探测管理器

为每个 `probe_enabled()` 的 provider 创建一个 `tokio::spawn` 后台任务：

- 启动时立即探测一次
- 之后按 `probe_interval_secs`（默认 300s）周期性探测
- 结果写入内存状态 map（供 router 读取）+ SQLite 持久化

`reconcile()` 用于管理写入后重载：中止所有当前探测任务，重新 spawn。

### `probe.rs` — 单次探测执行

支持两种模式：

1. **Lua 脚本模式**（`probe_script` 已设置）：在阻塞线程池中执行 Lua 脚本，脚本通过注入的辅助函数执行 HTTP 请求并返回 `{ ok, remaining, used, limit, note }`。`io` 和 `os` 模块被禁用，防止脚本访问文件系统/进程。
2. **连通性 ping 模式**（无脚本但 `probe_enabled = true`）：对 `base_url` 发 GET 请求，任何 HTTP 响应即为可用。

注入的 Lua 辅助函数：
- `ctx` — 全局表，包含 provider 配置
- `http{ url, method, headers, body }` → `{ status, body }` — 发起 HTTP 请求
- `json_decode(str)` → table — JSON 反序列化
- `json_encode(table)` → str — JSON 序列化

内置 Lua 探测脚本见 `probes/` 目录。

### `store.rs` — SQLite 数据访问层

管理 providers、routes、provider_status 三张表的 CRUD：

| 表 | 说明 |
|---|---|
| `providers` | Provider 配置（…, quota_min, cost_windows/model_prices 旧字段, **usage** JSON=Vec\<UsageSpec\>） |
| `routes` | 模型路由（alias PK, provider FK→providers, model, fallback JSON） |
| `provider_status` | watcher 运行时状态（provider FK→providers, available, quota_remaining/used/limit, last_checked, last_ok, error, note） |
| `usage_events` | 用量事件日志（provider, ts, usage_type, amount）——驱动滚动窗口，重启可恢复 |

关键操作：
- `open()` — 打开/创建 DB + 运行迁移
- `seed_from_config()` — 首次运行时从 TOML 种子填充
- `load_into_config()` — 从 DB 加载到内存配置（`row_to_provider` 会 `normalize_usage` 折算旧字段）
- `insert_provider/update_provider/delete_provider` — 管理 CRUD
- `write_status()` / `load_statuses()` — watcher 状态读写
- `insert_usage_event/load_usage_events/prune_usage_events` — 用量事件读写 + 保留期清理

provider 删除时通过外键级联删除关联的 route 和 status 行。

### `usage.rs` — 用量累加器

按**类型**统计每个 provider 的用量并喂给故障转移（受顶层 `cost_tracking` 开关控制，默认关）：

- `UsageMeter` 内存日志按 `(provider, UsageKind)` 分桶；`record/windows/exhausted` 均带类型参数。
- 窗口数学单位无关：`spent = Σ amount`、`remaining = limit − spent`、`remaining ≤ 0` 即耗尽。
- `amount_for(kind,…)`：`billing` = 真实 cost 或 token×单价估算；`count` = 1；`token` = prompt+completion。
- `parse_cost / tokens_from_usage / effective_cost` — 从响应里取 cost / token / 估算计费金额。
- 内存为读取（状态+failover）的权威源，事件持久化到 `usage_events`（保留约 31 天）。

类型与配置数据结构（`UsageKind / UsageWindow / UsageSpec`）定义在 `config.rs`，详见
[`configuration.md` 的用量章节](configuration.md#usage-tracking-cost--count--token-windows)。

### `admin.rs` — 管理 API

提供 RESTful 的管理接口 + 嵌入的管理页面 HTML：

- `GET /admin/api/providers` — 列出所有 provider（api_key 脱敏）
- `POST /admin/api/providers` — 创建 provider
- `PUT /admin/api/providers/:name` — 更新 provider
- `DELETE /admin/api/providers/:name` — 删除 provider
- `GET/POST /admin/api/routes`、`PUT/DELETE /admin/api/routes/:alias` — 路由 CRUD
- `GET/POST /admin/api/usage` — 用量统计开关（运行时切换；持久默认值是 `cost_tracking`）

provider 列表会附带按类型的 `usage` 视图（`server::usage_view`：billing/count/token + 各窗口
spent/remaining/reset），供管理页按类型渲染进度条。所有写入操作完成后自动调用 `reload_from_db()`
使变更热生效（无需重启）。鉴权复用 bridge 的 `auth_token`。

### `canonical.rs` — 中间表示

定义提供商无关的内部数据结构，使所有协议翻译都通过这个中间层，避免格式间直接互转：

- `CanonicalRequest` — 包含 model、system prompt、messages、tools、tool_choice、temperature、max_output_tokens、reasoning_effort、stream 等
- `Message` — User / Assistant（含 reasoning_content）/ Tool
- `CanonicalEvent` — Created、ReasoningDelta、TextDelta、ToolCallStart/ArgsDelta/Done、Usage、Completed、Error

### `sse.rs` — SSE 解码器

在原始字节层面解析 SSE（Server-Sent Events）流，仅在完整事件块边界解码 UTF-8，确保多字节字符不会因 TCP 分块而被截断。支持 `\n\n` 和 `\r\n\r\n` 两种分隔符，识别 `[DONE]` 终止标记。

### `wire/` — 协议格式模块

各协议的解析器（入站 → CanonicalRequest）和序列化器（CanonicalEvent → 出站 SSE/JSON）：

| 文件 | 方向 | 说明 |
|---|---|---|
| `chat.rs` | 出站 | 构建 Chat Completions 请求体；解析上游 Chat Completions SSE 流为 CanonicalEvent |
| `responses.rs` | 入站 | 解析 Responses API 请求；ResponsesEmitter 将 CanonicalEvent 序列化为 Responses SSE |
| `anthropic.rs` | 入站 | 解析 Messages API 请求（含 thinking blocks）；AnthropicEmitter 将 CanonicalEvent 序列化为 Messages SSE |

**DeepSeek thinking 模式支持**：`reasoning_content` 在 Chat Completions 中通过 `reasoning_content` delta 字段传递，在 Anthropic Messages 中映射为 `thinking` block，在 Responses 中映射为 `reasoning` output item。桥接器确保 reasoning 在每轮对话中正确回传给模型。

## 数据流全景

```
┌──────────┐  ┌───────────┐  ┌──────────┐
│  Codex   │  │Claude Code│  │  cURL/... │
│ (Resp.)  │  │ (Msg.)    │  │ (Chat)    │
└────┬─────┘  └─────┬─────┘  └────┬─────┘
     │              │             │
     ▼              ▼             ▼
 /v1/responses  /v1/messages  /v1/chat/completions
     │              │             │
     ▼              ▼             │
 parse_request  parse_request     │ (透传)
     │              │             │
     ▼              ▼             │
 CanonicalRequest◄─┘             │
     │                            │
     ▼                            │
 router::resolve_candidates()     │
     │                            │
     ▼                            │
 upstream::post_stream/post_json──┘
     │
     ▼
 Chat Completions SSE/JSON (上游)
     │
     ▼
 chat::completion_to_events / ChatStreamParser
     │
     ▼
 CanonicalEvent 流
     │
     ├──► ResponsesEmitter ──► /v1/responses SSE
     └──► AnthropicEmitter ──► /v1/messages SSE
```

**Watcher 侧通道**（与请求路径并行）：

```
watcher::watch_provider() [周期性 ticker]
    │
    ▼
probe::run_probe()  ←── probes/*.lua (Lua脚本) 或 HTTP ping
    │
    ├──► status: StatusMap (内存, 供router读取)
    └──► store::write_status() → provider_status表 (SQLite持久化)
```

## 故障转移机制

两层故障转移协同工作：

**主动层（Watcher 状态 + 用量窗口）**：
- 定期探测将 provider 标记为 available/unavailable
- `quota_remaining < quota_min` → 标记为耗尽
- 用量统计开启时，任一用量窗口（任意单位）`remaining ≤ 0` → 视为耗尽（撞 429 之前主动切走）
- Router 将可用 provider 排前，不可用/耗尽的排后

**被动层（请求时重试）**（与用量统计无关，始终生效）：
- 按候选链顺序尝试上游
- 可重试的错误：连接失败/超时、5xx、429（限流）、401/402（认证/配额）
- 不可重试：其他 4xx（错误的请求换 provider 也修不好）
- 流式请求在 headers 返回阶段即可判断成败（body 字节尚未发送给客户端）
- 失败时将 provider 立即标记为 unavailable，并触发一次异步重新探测

## 配置管理

### 配置来源分层

1. **bridge.toml** — `listen`、`default_provider`、`auth_token`、`database`（始终从此读取）；`[providers]` 和 `[[routes]]`（仅在首次运行时种子填充 SQLite）
2. **SQLite DB** — 首次运行后，provider 和 route 于此管理（可热更新）
3. **环境变量** — `BRIDGE_PROVIDERS_<NAME>_API_KEY` 覆盖对应 provider 的 api_key，避免明文存储
4. **管理 API** — 提供 Web UI 和 REST API 在线 CRUD provider/route，变更即时生效无需重启

## 技术栈

| 组件 | 依赖 |
|---|---|
| Web 框架 | axum 0.7 |
| 异步运行时 | tokio |
| HTTP 客户端 | reqwest 0.12 |
| 数据库 | SQLite（sqlx 0.8） |
| 配置格式 | TOML（toml 0.8） |
| Lua 引擎 | mlua 0.10（Lua 5.4，内置编译） |
| 序列化 | serde / serde_json |
| 流处理 | async-stream、futures-util |
| 错误处理 | thiserror、anyhow |
| 日志 | tracing |
| CLI | clap 4 |
