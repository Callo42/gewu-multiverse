# gewu-multiverse

[English](README.md) · [简体中文](README.zh-CN.md)

**传统的 Agent Harness 定义一次性任务；`gewu-multiverse` 定义长期运转的角色。**

`gewu-multiverse` 在你自己的 Linux 机器上提供一个**团队私有的内部 GitHub（内置仅监听本机的 Forgejo）**，配上一支**完全由你自定义职责的 AI 角色团队**——全部打包在单个轻量级二进制中。

每个角色拥有独立的 Forgejo 账号（`gewu-<role>`）、独立的 Landlock 隔离工作区，以及专属的职责说明（`instruction.md`）、`skills/`、外部 MCP 与角色级密钥。角色之间通过代码仓库、Issue、Pull Request 和 Review 去中心化异步协作；而与人类真实世界的一切交互，都在各自隔离工作区内通过编写与运行代码完成。

```text
  ┌─────────────────────────────────────────────────────────────────────┐
  │  交互界面 (gewu-multiverse) ── 通过 headless.sock 自动附着 ──▶       │
  │  运维控制 (--control/--status)   （按 q 仅断开界面，后台服务不停）     │
  └────────────────────────────────────────┬────────────────────────────┘
                                           ▼
  ┌─────────────────────────────────────────────────────────────────────┐
  │                 gewu-multiverse --headless (外层守护进程)            │
  │                                                                     │
  │   ┌─────────────────────────────────────────────────────────────┐   │
  │   │  内置 Forgejo 17.0.0（仅 127.0.0.1，唯一共享事实中枢）        │   │
  │   │  team/* 仓库 · Issue · PR · Review · 本机 HMAC Webhook      │   │
  │   └──────────────────────────────┬──────────────────────────────┘   │
  │                                  │ 唤醒（event / hybrid / periodic） │
  │         ┌────────────────────────┼────────────────────────┐         │
  │         ▼                        ▼                        ▼         │
  │  ┌──────────────┐        ┌──────────────┐        ┌──────────────┐   │
  │  │ 角色: pm     │        │ 角色: coder  │        │ 角色: reviewer│   │
  │  │ (Landlock)   │        │ (Landlock)   │        │ (Landlock)   │   │
  │  │ workspace/   │        │ workspace/   │        │ workspace/   │   │
  │  │ skills/ mcp  │        │ skills/ mcp  │        │ skills/ mcp  │   │
  │  │ HarnessPool  │        │ HarnessPool  │        │ HarnessPool  │   │
  │  └──────────────┘        └──────────────┘        └──────────────┘   │
  └─────────────────────────────────────────────────────────────────────┘

  每次唤醒都在该角色自己的沙盒内跑完一轮无状态闭环：
    [唤醒触发] ─▶ [机械重构 <environment-snapshot>] ─▶ [新建临时 Session 执行]
              ─▶ [客观动作账本判定结果]             ─▶ [销毁临时 Session]
```

## 为什么是“定义角色”？

传统单 Agent CLI 围绕“人推着一个长对话去完成一次任务”设计。对话越滚越长，上下文逐渐失焦，职责边界模糊，多角色协作退化成人工复制粘贴。

`gewu-multiverse` 换了一种组织方式：

- **唯一的共享事实中枢：** 内置 Forgejo（仅监听 `127.0.0.1`）承载全部代码仓库、Issue、分支、PR、评审与操作审计，角色间不私下传递脆弱的隐式对话历史。
- **单轮无状态完备闭包：** 跨轮不延续聊天记录。每次唤醒时，Supervisor 先机械拼接一份极简 `<environment-snapshot>`（本次唤醒原因、本地 Git 状态、指派给自己的开放 Issue、自己的开放 PR，以及该角色按需勾选的团队视图或沙盒内 `hydrate_cmd` 输出），在**全新临时 Session** 中执行 `instruction.md`，完全依据**客观动作账本**（MCP 写入、Git 推送与原生终态事件，中英文 Prompt 一视同仁）判定本轮结果，随后销毁该 Session。
- **双层后台 Server 架构：**
  - **外层守护进程（`--headless`）：** 原生适配 `systemd` 与无人值守服务器。当该目录已有守护进程运行时，直接执行 `gewu-multiverse` 会通过私有 Unix Socket **自动附着**为实时控制台（按 `q` 仅退出界面、不中断 Forgejo 与运行中的角色；按 `Q` 可显式关闭守护进程）。
  - **内层每角色独立热驻 `HarnessRuntime`：** 严格**每角色在自己的 Landlock 沙盒内独占一套常驻运行时**（绝不跨角色共享；每角色的运行时 Socket、MCP 桥 Socket 与本地 Git 代理均受独立 256 位 `0600` 随机 Cookie/口令认证，且仅存放于该角色只读目录内），空闲超过 `runtime_idle_secs`（默认 `300s`）自动回收释放内存。内置 4 款经实测锁定的纯开源 Harness：**DeepSeek Harness（`dsh` `0.2.2-rc.5`，全新安装默认）**、**Pi（`1.6.1`）**、**Codex（`0.172.0`）**、**OpenCode（`2.4.2`）**；旧版 `built-in` 引擎冻结保留用于老配置无感兼容。
- **可切换唤醒模式（`periodic` | `event` | `hybrid`）：** 全新安装默认 `hybrid`（内置 Forgejo 本机 HMAC-SHA256 Webhook，带投递去重、防抖、冷却限频与防回声 + 300 秒兜底巡检），也支持纯 `event` 事件驱动或 `periodic` 定时轮询。**处于 `desired = "stopped"` 的角色在任何模式下都绝对静默：0 次唤醒、0 次模型调用。**
- **内部 GitHub 分层权限：** 默认仅授予最小执行权（查派给自己的 Issue、读 Issue/PR、评论、经认证本地 Git 代理推分支——沙盒内无原始 Forgejo Token、提/改自己的 PR）。按角色需要单独开通更高团队权限：
  - `triage`：列举/新建/更新/派单/打标签/关闭/重开 Issue；
  - `review`：列举 PR、流式限长读取 Diff、提交正式评审（**Supervisor 层核验当前 Token 身份并硬拦截自审**）；
  - `maintain`：合并已过审的他人 PR（**Supervisor 层硬拦截自合，并强制校验匹配当前 PR head SHA 且未失效/未驳回的他人 `APPROVED` 评审、绑定 `head_commit_id` 合并且全分页零未解 `REQUEST_CHANGES`**）、在 `team/` 下新建私有仓库。删除仓库与组织最高管理员权限始终只归人类 `owner`。
- **严格本机隔离下的真实世界连接：** 自动启用的内核 **Landlock** 将每个角色的可写范围严格锁在自身 `workspace/` 与受保护父目录下的精确叶子运行目录（`home`、`tmp`、`xdg` 及当前 Harness 专属叶子目录）内，屏蔽 `$HOME`、全局凭据、守护进程控制 Cookie、Forgejo 数据、兄弟角色工作区、`/run/user/*` 与 `/dev/shm`，并在内核支持 Landlock ABI v6+ 时自动启用 `Scope::Signal` 与 `Scope::AbstractUnixSocket`。需要对接外部真实世界（云 API、工单、飞书/文档、监控流水线等）时，只需给该角色配置专属 `env`、`0600` 外部 API 密钥（`credentials.toml` 的 `[agent_env_secrets.<role>]`，日志与回放自动脱敏）、`skills/` 和 `mcp.json`。

## 配置一支角色团队（形态 A）

团队总拓扑写在 `~/.config/gewu-multiverse/multiverse.toml`，每个角色的装备放在 `~/.config/gewu-multiverse/agents/<role>/`：

```text
~/.config/gewu-multiverse/
├── multiverse.toml          # 团队总拓扑、唤醒模式、上下文视图与角色权限
├── credentials.toml         # 0600 权限：模型密钥、Forgejo Token 与角色外部 API 密钥
└── agents/
    ├── pm/
    │   ├── instruction.md   # 角色职责与操作规范
    │   ├── skills/          # 角色专属 Skills
    │   ├── mcp.json         # 可选：角色专属外部 MCP Server 配置
    │   └── workspace/       # 角色私有的 Git 克隆目录（Landlock 可写边界）
    ├── coder/
    ├── reviewer/
    └── ops/
```

`multiverse.toml` 示例：

```toml
terms_acknowledged = true
terms_version = 1
default_harness = "dsh"
wake_mode = "hybrid"          # periodic | event | hybrid
default_context = "self"      # none | self
runtime_idle_secs = 300

[endpoint]
base_url = "https://api.example.com/v1"
protocol = "chat-completions"
model = "your-model-id"
max_turns = 24

[[agents]]
name = "pm"
desired = "started"
harness = "dsh"
wake_mode = "hybrid"
[agents.context]
mode = "self"
views = ["unassigned_issues", "open_pulls"]
[agents.permissions]
triage = true

[[agents]]
name = "coder"
desired = "started"
harness = "codex"
wake_mode = "event"           # 收到派单或评审意见后即时唤醒

[[agents]]
name = "reviewer"
desired = "started"
harness = "dsh"
wake_mode = "event"
[agents.context]
mode = "self"
views = ["open_pulls"]
[agents.permissions]
review = true
maintain = true               # 可合入已过审他人 PR；硬拦截自审与自合

[[agents]]
name = "ops"
desired = "started"
harness = "pi"
wake_mode = "hybrid"
wake_interval_secs = 600
[agents.context]
mode = "self"
hydrate_cmd = "python3 scripts/check_alerts.py --summary"  # 在同一 Landlock 沙盒内执行
[agents.env]
SERVICE_ENV = "staging"       # 配合 credentials.toml 中 0600 的 [agent_env_secrets.ops]
```

支持语义化角色名（`pm`、`coder`、`reviewer`、`ops`），同时 100% 兼容既有的 `agent-1`、`agent-2` 命名；所有新增字段均为可选。

## 安装

一行安装，无需 Rust 环境（Linux `x86_64`，glibc 2.39+）：

```sh
curl -fsSL https://raw.githubusercontent.com/Callo42/gewu-multiverse/main/install.sh | sh

# 离线 / 隔离环境直接从预先下载的发布包安装：
sh install.sh --tarball ./gewu-multiverse-*-x86_64-linux.tar.gz [--checksum ./file.sha256] --harness none

# 默认安装 DeepSeek Harness (dsh)；其他官方引擎按需指定：
sh install.sh --harness dsh,codex
sh install.sh --harness pi
sh install.sh --harness none        # 不安装底层引擎；全新安装新建角色仍默认指向 DeepSeek Harness
```

发布包内置预编译的 `gewu-multiverse`（安装到 `~/.local/bin/gewu-multiverse`）以及未经修改、经上游 GPG 校验的 `forgejo-17.0.0-linux-amd64`（安装到 `~/.local/share/gewu-multiverse/`，无需二次下载 Forgejo）。除非指定 `--harness none`，安装脚本会自动拉取对应版本的开源引擎并逐项核验 SHA-256 / npm 锁文件摘要。DeepSeek Harness（`dsh 0.2.2-rc.5`）需要 Node.js 24.9+ 与 npm 11.16.0+，安装时强制使用 `npm ci --ignore-scripts` 并仅对经审查的 3 个原生辅助包（`@deepseek-ai/dsh-subprocess-local`、`koffi`、`node-pty`）执行显式 `npm rebuild`。

源码安装（Rust 1.85+）：

```sh
cargo install --git https://github.com/Callo42/gewu-multiverse
```

## 快速开始

### 交互式终端看板

```sh
gewu-multiverse
```

1. **首次启动：** 阅读并确认本机隔离提示（`y`）、填写默认模型地址与密钥（`t` 测试连通）、创建初始角色，选中角色按 `⏎` → `e` 编写 `instruction.md`。按 `s` 启停角色，按 `p` 切换或安装 Harness，按 `f` 查看本机 Forgejo 地址与管理员口令重置，按 `?` 查看全部快捷键。
2. **自动附着后台守护进程：** 若同一数据目录已有 `gewu-multiverse --headless` 在运行，直接启动 `gewu-multiverse` 会自动附着为实时终端看板。按 `q` 仅断开界面、Forgejo 与角色继续运行；按 `Q` 可安全停止后台守护进程。
3. **脚本演示模式（不启动真实 Forgejo）：** `gewu-multiverse --demo`

### 无头守护与服务器自动化（`systemd` / CI / 下游集成）

```sh
# 1. 非交互配置（所有密钥仅从文件或 stdin 读取，严禁经命令行参数传入）：
gewu-multiverse --configure \
  --accept-terms \
  --add-role coder \
  --agent coder \
  --program dsh \
  --wake-mode hybrid \
  --context-mode self \
  --base-url https://api.example.com/v1 \
  --model your-model-id \
  --api-key-file /run/secrets/model.key \
  --instruction-file ./coder.md

# 可选：为 reviewer 开通评审权限并注入角色级外部密钥：
gewu-multiverse --configure \
  --add-role reviewer \
  --agent reviewer \
  --allow-review \
  --context-view open_pulls \
  --env REVIEW_STRICT=1 \
  --secret-env-file WEBHOOK_TOKEN=/run/secrets/external.token

# 2a. 启动标准前台守护进程（适配 systemd）：
gewu-multiverse --configure --start-all
gewu-multiverse --headless

# 2b. 热备零调用待命模式（保持 Forgejo 与角色账号常热，但停止状态的角色零模型调用）：
gewu-multiverse --configure --stop-all
gewu-multiverse --headless --allow-idle

# 3. 在线控制运行中的守护进程（无需重启 Forgejo，修改 instruction.md 自动生效）：
gewu-multiverse --control --start-agent coder --stop-on-no-work
gewu-multiverse --control --stop-agent coder
gewu-multiverse --control --stop-all --json

# 4. 只读健康检查（不抢占守护进程文件锁）：
gewu-multiverse --status
gewu-multiverse --status --json
```

可通过 `--config-dir <dir>`、`--state-dir <dir>`、`--data-dir <dir>` 自由重定向所有目录。

## `v0.2.0` 升级与向后兼容承诺

从 `v0.1.x`（包括下游 `matrixorg` 无头部署）平滑升级至 `v0.2.0` **零破坏**：

- 既有未声明 `default_harness`、`wake_mode`、`default_context` 的老 `multiverse.toml` 自动沿用原有引擎（含 `built-in`）、`wake_mode = "periodic"`、`context = "none"` 与最小 Forgejo 权限，绝不静默切换。
- `--headless --allow-idle`、`--control`、`--configure`、`--status --json` 及 `headless-status.json` 既有接口与字段 100% 兼容。
- `desired = "stopped"` 在所有唤醒模式下严格保持 **0 次唤醒、0 次模型调用**。
- 新默认值（`dsh`、`wake_mode = "hybrid"`、`default_context = "self"`）仅作用于**全新安装**或显式配置开启的场景。

## 安全边界与明确不做之事

- **不是容器或虚拟机：** 角色以当前本机用户运行，默认自动启用内核 **Landlock**（启动前经子进程真实读写探测验证后才宣告生效），完全兼容 `systemd` 的 `NoNewPrivileges=true` 与 `ProtectHome=true`。当宿主机内核支持 Landlock ABI v6+ 时自动启用信号与抽象 Unix Socket 作用域隔离（`Scope::Signal`、`Scope::AbstractUnixSocket`）；在旧版 Landlock 内核上，同 UID 信号隔离不由 Landlock 保证，且 `/proc` 可见性遵循宿主机 `hidepid` 与 Yama `ptrace_scope` 配置（也可通过 `GEWU_CONFINEMENT=bwrap` 或 `unshare` 显式选用命名空间隔离）。
- **文件系统与 IPC 边界：** 除固定的只读系统目录（`/usr`、`/etc`、`/proc`、`/sys` 等）与安全 `/dev/*` 设备节点外，受限进程仅可向自身 `workspace/` 与受保护非可写父目录（`<TMPDIR>/gewu-rt-*`）下的精确叶子目录（`home`、`tmp`、`xdg` 与当前 Harness 运行叶子目录）写入用户数据，Landlock 逐级使用 `O_NOFOLLOW` 打开可写授权路径，杜绝符号链接重定向越狱，无法读取 `~/.config/gewu-multiverse`、`credentials.toml`、`headless.sock.cookie`、Forgejo 数据、`/run/user/*`、`/dev/shm`、宿主机 `$HOME` 或其他角色的工作区（允许范围之外的宿主机私有路径永不打开）；所有 Supervisor IPC（`headless.sock`、`runtime.sock`、`mcp.sock`、本地 `GitProxy`）均强制校验 256 位 `0600` 随机 Cookie 或代理凭据。
- **密钥不落地命令行与日志：** 模型密钥、Forgejo Token 与角色外部密钥（`[agent_env_secrets.<role>]`）均保存在 `0600` 权限的 `credentials.toml` 中；`mcp.json` 禁止在 `command`/`args`/`url` 中引用 `${SECRET}`（仅允许在 `env` 与 HTTP `headers` 中引用）；日志、回放与 `hydrate_cmd` 输出均自动脱敏原始、JSON 转义、URL 编码与 Base64 形式的密钥。
- **Forgejo 仅监听本机：** 严格绑定 `127.0.0.1`，不对外暴露网络端口，不依赖外部云服务。

## 目录一览

| 用途 | 默认路径 |
|---|---|
| 主程序 | `~/.local/bin/gewu-multiverse` |
| 团队与角色配置 | `~/.config/gewu-multiverse/multiverse.toml` |
| 密钥文件（`0600`） | `~/.config/gewu-multiverse/credentials.toml` |
| 角色目录（`instruction.md` / `skills/` / `mcp.json` / `workspace/`） | `~/.config/gewu-multiverse/agents/<role>/` |
| 运行记录、日志与可读回放 | `~/.local/state/gewu-multiverse/<role>/<run>/` |
| 守护进程快照与控制 Socket（`0600` Cookie） | `~/.local/state/gewu-multiverse/headless-status.json`、`headless.sock`、`headless.sock.cookie` |
| 内置 Forgejo 与已安装 Harness | `~/.local/share/gewu-multiverse/{forgejo,harnesses}/` |

完全卸载：

```sh
rm ~/.local/bin/gewu-multiverse
rm -rf ~/.config/gewu-multiverse ~/.local/state/gewu-multiverse ~/.local/share/gewu-multiverse
```

## 深入文档

详细工程规范与行为契约见 `docs/`：

- [docs/native-programs.md](docs/native-programs.md) — 4 款锁定 Harness（`dsh` / `pi` / `codex` / `opencode`）、每角色常驻 `HarnessRuntime` 与安装核验
- [docs/engine.md](docs/engine.md) — 引擎生命周期、唤醒调度、无头守护与崩溃恢复
- [docs/agent-execution.md](docs/agent-execution.md) — Landlock 隔离边界、运行记录与客观动作账本
- [docs/forgejo.md](docs/forgejo.md) — 内置 Forgejo 初始化、分层 MCP 工具集与本机 Webhook
- [docs/board.md](docs/board.md) — 终端看板布局、快捷键与状态标记规范

## 许可证

采用以下任一协议双许可：

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))
