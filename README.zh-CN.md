# herdr-agent-watcher

[English](README.md) · **简体中文** · [日本語](README.ja.md)

面向 [Herdr](https://herdr.dev) 的编码 agent 可观测性插件：实时侧边栏卡片、生命周期通知，
以及一个零配置的 Claude Code 指标桥接。

![Agent Watcher 侧边栏](docs/sidebar.png)

一屏四个 agent。展开的 Claude 卡片上的 CONTEXT、CACHE、COST 这三项，是 Claude Code 只通过
状态栏上报的指标 —— 也正是下面那个桥接存在的理由。

这个想法最初长在 [Vimeflow](https://github.com/winoooops/vimeflow) 里面：观测编码 agent 只是
那个 Electron 应用中的一层。

与 [herdr-agent-title-sync](https://github.com/winoooops/herdr-agent-title-sync) 配套使用 ——
后者让 Herdr 的 pane 标题跟上每个 agent 正在做的事。

**默认只做本地观测。** 唯一会离开你机器的是 Kimi 的用量查询，它在你主动开启前一直是关的 ——
见 [Kimi 用量上报同意](#kimi-用量上报同意)。

## 安装

```sh
herdr plugin install winoooops/herdr-agent-watcher
```

需要 Herdr 0.8.0+。安装时会下载 macOS 和 Linux（x86_64 与 arm64）的预编译二进制并校验
SHA256。如果没有匹配你平台的产物、或下载环节出任何问题，就改为从源码编译 —— 那条路需要
Rust 1.88+，工具链缺失时 Herdr 只报错，不会替你安装。

往一个**已在运行**的 Herdr server 里安装时不会自动启动 daemon，需要手动跑一次：

```sh
herdr plugin action invoke restart-daemon --plugin herdr-agent-watcher
```

Herdr 原生 UI 不依赖侧边栏也能工作：它会收到生命周期通知和 pane 元数据 token ——
`agent_watcher_state`、`agent_watcher_phase`、`agent_watcher_model`、
`agent_watcher_context_pct`、`agent_watcher_attention`、`agent_watcher_title`。
**这些名字是与 Herdr 的集成接口，刻意保持稳定。**

## 验证

扫描所有已支持的活跃 agent pane，且不暴露 pane 或 session ID：

```sh
./tests/verify-live-agents.sh
./tests/verify-sidebar-state.sh
```

Tier A 跑在确定性的 fake Herdr socket 上，始终启用：

```sh
cargo test --test e2e_fake_herdr
```

Tier B 会用隔离的 HOME/XDG 目录启动已安装的 Herdr 二进制，默认被忽略：

```sh
cargo test --test e2e_real_herdr -- --ignored
```

普通测试全跑：`cargo test`。

## 可用命令

所有 action 的调用方式相同：

```sh
herdr plugin action invoke <id> --plugin herdr-agent-watcher
```

输出进插件日志 —— 用
`herdr plugin log list --plugin herdr-agent-watcher --limit 1` 看最近一次。

| Action | 做什么 | 详见 |
| --- | --- | --- |
| `restart-daemon` | 启动或重启 daemon | |
| `stop-daemon` | 停止 daemon | |
| `open-sidebar` | 在新分屏里打开实时侧边栏 | [侧边栏](#侧边栏) |
| `enable-claude-bridge` | 把指标桥接装进 Claude 自己的配置 | [Claude 指标桥接](#claude-指标桥接) |
| `disable-claude-bridge` | 把配置文件还原到启用前的状态 | [Claude 指标桥接](#claude-指标桥接) |
| `doctor` | 指出指标为什么缺失，以及该做什么 | [Doctor](#doctor) |
| `kimi-consent-on` | 允许 Kimi 用量查询 | [Kimi 用量上报同意](#kimi-用量上报同意) |
| `kimi-consent-off` | 撤销 —— 无需重启即刻生效 | [Kimi 用量上报同意](#kimi-用量上报同意) |
| `kimi-consent-status` | 查看当前设置 | [Kimi 用量上报同意](#kimi-用量上报同意) |

## 配置

`AGENT_WATCHER_INTERVAL_MS` 是 daemon 与 Herdr 协调（reconciliation）的间隔，单位毫秒，
必须为正数，默认 `1000`。以设成 5 秒为例：

```sh
echo 'export AGENT_WATCHER_INTERVAL_MS=5000' >> ~/.zshrc
```

然后开一个全新的 Herdr 会话 —— daemon 继承的是 server 的环境，单独跑 `restart-daemon` 拿不到
新值。

## 侧边栏

```sh
herdr plugin action invoke open-sidebar --plugin herdr-agent-watcher
```

每次调用都会**有意**新开一个分屏。卡片显示 agent 状态、agent/模型、标题、上下文用量、
缓存命中率、成本、工具调用数，以及最近三条工具调用记录。`j`/`k` 或 PageUp/PageDown 滚动，
`o`/`↵` 展开，`z` 隐藏空闲 agent，`q`、Escape 或 Ctrl-C 关闭。

daemon 不可用或断开时，面板会显示该状态并等待按键后再关闭。侧边栏的 state socket 属于插件
内部实现：`$HERDR_PLUGIN_STATE_DIR/herdr-agent-watcher-state.sock`，其换行分隔的 JSON 协议
当前为 `WIRE_VERSION = 2`，**不是**公开的集成 API。

停止 daemon：

```sh
herdr plugin action invoke stop-daemon --plugin herdr-agent-watcher
```

## 已支持的 agent

| Agent | 指标来自哪里 | 桥接 | 状态 |
| --- | --- | --- | --- |
| Claude Code（`claude`、`claude-code`）| **只有**它的状态栏 | **必需** —— 一条 `enable-claude-bridge` | ✅ |
| Codex CLI（`codex`）| rollout transcript | 不需要 | ✅ |
| Kimi Code（`kimi`）| transcript，外加可选的用量 API | 不需要 | ✅ |
| OpenCode（`opencode`）| 自带的 bridge 插件 | 首次绑定时自动装好 | ✅ |

只有 Claude 需要你主动启用桥接，原因只有一个：**它发出的 hook 事件不携带任何用量数据**，
状态栏是唯一通道。OpenCode 的 bridge 是本插件替你安装的；Codex 和 Kimi 什么都不需要。

新增 agent：在 `src/agents/` 下实现 `AgentAdapter`，并在 `src/daemon/run.rs` 里注册到
`AgentRegistry`。agent 特有的解析逻辑放在 adapter 里；Herdr socket 的细节留在 `HerdrPort`
后面。

## Claude 指标桥接

Claude Code 的 CONTEXT、CACHE、COST **只**通过状态栏上报，所以它需要一个另外三个 agent
都不需要的桥接。

```sh
herdr plugin action invoke enable-claude-bridge --plugin herdr-agent-watcher
```

它会改 Claude 自己的用户配置（并打印改的是哪个文件），并把你原有的状态栏串接在桥接之后，
让它照常运行。不用改 `PATH`、不用开新 shell：**之后每个 pane 里的每个 Claude 都会接入**，
包括已经在跑的会话 —— 它们在下一次状态栏渲染时接上。

```sh
herdr plugin action invoke disable-claude-bridge --plugin herdr-agent-watcher
```

会还原配置文件，你原本没有 `statusLine` 的话就把这个键整个删掉。**启用之后你自己改过的
statusLine 不会被动** —— 桥接只收回仍然属于它自己的东西。

## Doctor

卡片显示 `— bridge not connected (README)` 时用它；或者任何时候指标缺失、你想知道为什么。

```sh
herdr plugin action invoke doctor --plugin herdr-agent-watcher
herdr plugin log list --plugin herdr-agent-watcher --limit 1
```

doctor 会指出原因并给出修复方式。**唯一它不能替你修的**是某个项目自己定义了 `statusLine`，
那优先级高于用户层 —— 它会生成一段可直接粘贴到该项目 `.claude/settings.local.json` 的配置，
让项目的状态栏和你的指标都保住。

它在信息不全时绝不报绿：受管配置从这里读不到，所以当所有检查都通过、指标却仍然缺失时，
它会明说这一点。

## Kimi 用量上报同意

Kimi 的用量查询会把配置的 API key 发送到它的 `/usages` 端点，因此**默认关闭**，必须显式开启。
使用 Herdr 插件动作 `kimi-consent-on`、`kimi-consent-off`、`kimi-consent-status`；撤销后正在
运行的 daemon 无需重启即可生效。

## OpenCode 桥接

首次绑定 OpenCode 时会在 OpenCode 的插件目录里安装或更新自带的 bridge。
`AGENT_WATCHER_OPENCODE_PLUGINS_DIR` 和 `AGENT_WATCHER_OPENCODE_BRIDGE_DIR` 可覆盖安装目录
和事件目录。该 bridge 插件保留 `agent-watcher-opencode-bridge` 这个文件名：它位于移植过来的
sidecar 树中，而那棵树是冻结的。

## 设计说明

[`DESIGN.md`](DESIGN.md) 记录了这个插件为什么长成这样：为什么只有 Claude 需要桥接、
为什么装进 Claude 自己的配置而不是拦截 `PATH`、为什么由 daemon 拥有写入目标、
以及一个坏掉的桥接**必须**退化成什么样子。

## 已知限制

- **daemon 启动前就已打开的 pane 没有卡片。** Herdr 不为它上报 `agent_session`，因此无法绑定。
  关掉再重开该 pane 即可。
- **在 workspace 之间移动过的 pane 保留已作废的 id。** 进程的 pane id 在 exec 时就固定了，
  daemon 认不出它。doctor 会把这个情况显示出来而不是静默失败，但该会话必须重建。
- **同一时间只支持一个 Herdr 会话。** daemon 的锁和 socket 都在用户全局的
  `$HERDR_PLUGIN_STATE_DIR` 下，第二个 Herdr 会话会顶掉第一个 daemon。
- **`attention.jsonl` 无界。** 只追加，有 192KB 的单条 payload 上限，但没有总量上限、也没有轮转。
- **一个会话的第一个 turn 可能不产生完成通知**，且五个 hook 里有四个原样存储 payload ——
  所以保证是「prompt 不落盘」，**不是**「没有任何用户文本落盘」。两者都在冻结的
  `src/agent/**` 树里。

## 后续工作

- [ ] 从插件自己的 `config.toml` 读配置，取代 `AGENT_WATCHER_INTERVAL_MS`，
      这样配置就不再取决于 Herdr server 是从哪个环境启动的
- [ ] 回收失效的 bridge 目录。判据用**存活性** —— pane id 不在 Herdr 的 pane 列表里
      **且**没有进程持有它 —— 绝不用 unbind（rebind 是常态），也绝不用 mtime
      （会误伤长时间空闲但仍开着的 pane）
- [x] 每个 release 发布预编译二进制，让安装不再需要 `cargo`。`[[build]]` 现在跑
      `scripts/fetch-or-build.sh`：下载匹配平台的产物、校验 SHA256，任何一步失败就改为编译
- [ ] sidebar 的 `:` 命令模式，以及整页 doctor 视图
- [ ] 修掉 flaky 的 `pane_without_cwd_uses_herdrs_cwd_for_that_pane` 测试

## 本地开发

```sh
cargo build --release
herdr plugin link "$PWD"
herdr plugin action invoke restart-daemon --plugin herdr-agent-watcher
```

`plugin link` 按设计跳过构建步骤，工作目录由你自己构建。
