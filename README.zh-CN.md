# herdr-agent-watcher

[English](README.md) · **简体中文** · [日本語](README.ja.md)

面向 [Herdr](https://herdr.dev) 的编码 agent 可观测性插件：实时侧边栏卡片、生命周期通知，
以及一个零配置的 Claude Code 指标桥接。

![Agent Watcher 侧边栏](docs/sidebar.png)

*一个面板里五个会话、四种 agent —— 运行中、已完成、空闲。展开的 Claude 卡片还带着
CONTEXT、CACHE 和 COST：这三个数字 Claude Code 只经由状态行上报，别无他途，把它们送到
这里的正是那个桥接。*

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
| `bind-sidebar-key` | 绑定一个打开侧边栏的快捷键 | [侧边栏](#侧边栏) |
| `unbind-sidebar-key` | 移除该绑定 | [侧边栏](#侧边栏) |
| `enable-claude-bridge` | 把指标桥接装进 Claude 自己的配置 | [Claude 指标桥接](#claude-指标桥接) |
| `disable-claude-bridge` | 把配置文件还原到启用前的状态 | [Claude 指标桥接](#claude-指标桥接) |
| `doctor` | 指出指标为什么缺失，以及该做什么 | [Doctor](#doctor) |
| `kimi-consent-on` | 允许 Kimi 用量查询 | [Kimi 用量上报同意](#kimi-用量上报同意) |
| `kimi-consent-off` | 撤销 —— 无需重启即刻生效 | [Kimi 用量上报同意](#kimi-用量上报同意) |
| `kimi-consent-status` | 查看当前设置 | [Kimi 用量上报同意](#kimi-用量上报同意) |

## 配置

大部分设置从 sidebar 里改更省事：打开它，按 `x`，设置面板会实时编辑同一个文件 —— 每改一项
都能立刻在卡片上看到效果，而且只会写入你动过的键。需要写注释、需要面板没有暴露的键，或者
要把配置纳入版本管理时，再直接编辑文件。

每个键都是可选的，每个无效值都会回退到默认值，因此一个错误只会影响一项设置，而不会影响整个插件。

| 键 | 取值 | 默认 | 作用 |
| --- | --- | --- | --- |
| `daemon.interval_ms` | 正整数 | `1000` | 对账间隔。启动时读取，改完需 `restart-daemon`。`AGENT_WATCHER_INTERVAL_MS` 优先级更高 |
| `appearance.theme` | `inherit`、`lumon` | `inherit` | `inherit` 沿用终端配色；`lumon` 自带一套 |
| `appearance.agent_mark` | `dot`、`initial`、`symbol` | `dot` | 卡片上 agent 的标记 |
| `cards.auto_expand` | `none`、`all` | `none` | 卡片默认展开 |
| `cards.tool_calls` | `bars`、`jar` | `bars` | 上下文用量条的画法 |
| `cards.trace_lines` | `1`–`20` | `5` | 展开后显示几条记录。超范围是夹到边界，不是拒绝 |
| `list.sort` | `position`、`smart`、`group` | `position` | 卡片排序：Herdr 的布局顺序、按紧急程度、或按 agent 分组。默认用 `position`，因为只有它不会在你眼皮下移动 |
| `list.hide_idle` | `true`、`false` | `false` | 隐藏空闲 agent，同按 `z` |
| `list.scope` | `all`、`workspace` | `all` | `workspace` 需要 `HERDR_WORKSPACE_ID`，没有则退回 `all` |
| `keys.open_sidebar` | Herdr 按键串 | `prefix+a` | `bind-sidebar-key` 写入的键。须在绑定前设好 |
| `agent.<id>.color` | `#rrggbb` | 内置 | 覆盖某个 agent 的颜色 |
| `agent.<id>.label` | 任意字符串 | 内置 | 覆盖它在卡片上的名字 |
| `agent.<id>.symbol` | 任意字符串 | 内置 | `agent_mark = "symbol"` 时用的标记 |

设置放在**插件自己的** `config.toml` 里，不是 Herdr 的那个。Herdr 会忽略它不认识的表，
所以把 `[daemon]` 写进 `~/.config/herdr/config.toml` 不会有任何效果，只会让
`herdr config check` 报告一个未知小节。

插件的配置目录可以用这条命令打印出来：

```sh
herdr plugin list
```

默认是 `${XDG_CONFIG_HOME:-~/.config}/herdr/plugins/config/herdr-agent-watcher/`。
如果 `config.toml` 不存在，自行创建。

```toml
[daemon]
interval_ms = 5000

[list]
scope = "workspace"
sort  = "position"
```

运行 [`doctor`](#doctor) 可查看某项设置是否被拒绝，以及实际改用了什么值。

## 侧边栏

```sh
herdr plugin action invoke open-sidebar --plugin herdr-agent-watcher
```

每次调用都会**有意**新开一个分屏。在你绑定之前它没有快捷键 —— 绑定步骤见下文；默认是
`prefix+a`，配合 Herdr 自身的默认前缀就是先按 `ctrl+b` 再按 `a`。
卡片显示 agent 状态、agent/模型、标题、上下文用量、
缓存命中率、成本、工具调用数，以及最近三条工具调用记录。`j`/`k` 或 PageUp/PageDown 滚动，
`o`/`↵` 展开，`z` 隐藏空闲 agent，`x` 打开菜单，`?` 列出所有按键，`q`/`Esc` 或
Ctrl-C 关闭。

`x` 打开菜单，`?` 列出所有按键。菜单通向设置面板和 doctor 面板；`s` 和 `d` 也能到达，
但只在已经打开某个面板时有效 —— 在卡片列表上它们不做任何事，所以 `x` 是唯一入口。
`Esc` 在任何位置都只退一级：面板退回菜单，菜单则关闭。

循环切换设置时，你正在查看的内容会立即变化 —— `l`/`→` 向前、`h`/`←` 向后，`o`/`↵` 同样
向前 —— 按下 `s` 之前不会写入任何内容。

`interval_ms` 是例外：它属于 daemon，因此改动在 daemon 重启前不会生效。改过它之后要离开
面板时会先询问 —— 立即重启，或改回原值 —— 不作答就无法离开。

doctor 面板会在不离开 sidebar 的情况下显示 `doctor` 打印的内容。按 `r` 重新生成报告；
报告比 pane 高时用 `j`/`k` 滚动。

`scope = "workspace"` 正是「为每个 workspace 各开一个 sidebar」这件事有意义的原因。daemon
尚未确定 workspace 的 pane 会显示出来而不是被隐藏；scope 回退到 `all` 时，sidebar 会在
footer 上方说明原因。

要用快捷键打开它：

```sh
herdr plugin action invoke bind-sidebar-key --plugin herdr-agent-watcher
```

这会把绑定写入 **Herdr 的**配置；如果这个键已经被占用，操作会拒绝并指出占用它的项目。
想换一个键，运行它之前先设好 `keys.open_sidebar`。

`unbind-sidebar-key` 会移除该绑定。**卸载插件前先运行它**，否则绑定会比它指向的 action
存留得更久 —— Herdr 卸载插件时不会运行任何命令。

sidebar 打开时若 daemon 不可用，面板会说明并等待按键。若是在 sidebar 打开期间断开 ——
通常正是设置面板刚刚要求的那次重启 —— 卡片会留在屏幕上，提示会显示已等待的秒数，sidebar
会自行重新订阅；整个过程中按键照常响应。超过一分钟仍未恢复才会停止重试并等待按键。它的
state socket
（`$HERDR_PLUGIN_STATE_DIR/herdr-agent-watcher-state.sock`，`WIRE_VERSION = 2`）
属于插件内部实现，不是公开 API。

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

同样的报告在 sidebar 里一个按键就能看到：`x`，然后选 doctor 那一行，`r` 重新生成。需要在
Herdr pane 之外查看、或者写进脚本时，再用命令行。

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

## 疑难排查

**Claude 卡片上 CONTEXT、CACHE、COST 显示为 `—`。** 这三项只经由状态行送达。跑一次
[`doctor`](#doctor) —— 它会区分三种原因并给出各自的处方：

- *桥接没启用* —— `enable-claude-bridge`；
- *herdr reports no agent session for this pane* —— 这个 pane 里的会话被换掉了，而 session
  与绑定对不上的状态行写入会被拒绝。关掉再重开这个 pane；
- *no metrics yet* —— 没出问题；这个 pane 自你启用桥接以来还没渲染过状态行。给它发一条
  提示词。

还有第四种、临时的情况：会话正在跑子 agent，此时状态行描述的是子 agent，卡片会显示它的模型名、
用量从零开始。这种会在主会话下一次 turn 时自行恢复。

**某个 pane 根本没有卡片。** 对于 daemon 启动之前就已打开的 pane，Herdr 不会报告
`agent_session`，因此无法绑定。关掉再重开这个 pane。

**改了设置却没有任何变化。** `[daemon]` 和 `[list]` 属于**插件的** `config.toml`，不是 Herdr
的那个 —— 写错地方时 [`doctor`](#doctor) 会指出来并给出搬运命令。直接打开正确的文件：

```sh
$EDITOR "$(herdr plugin config-dir herdr-agent-watcher)/config.toml"
```

`[list]` 在 sidebar 打开时读取一次，所以已经开着的那个仍保持它启动时的设置。关掉再开一次。

**`doctor` 全部通过，指标却仍然缺失。** 它会如实这么说，而不是报绿：托管设置、workspace 信任
状态和启动参数在这里都看不到，而它们中的任何一个都可能压过状态行。

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

- [x] 从插件自己的 `config.toml` 读配置，取代 `AGENT_WATCHER_INTERVAL_MS`，
      这样配置就不再取决于 Herdr server 是从哪个环境启动的
- [ ] 回收失效的 bridge 目录。判据用**存活性** —— pane id 不在 Herdr 的 pane 列表里
      **且**没有进程持有它 —— 绝不用 unbind（rebind 是常态），也绝不用 mtime
      （会误伤长时间空闲但仍开着的 pane）
- [x] 每个 release 发布预编译二进制，让安装不再需要 `cargo`。`[[build]]` 现在跑
      `scripts/fetch-or-build.sh`：下载匹配平台的产物、校验 SHA256，任何一步失败就改为编译
- [x] sidebar 内的设置面板与 doctor 面板，让最常用的配置和最常用的诊断都不必离开这个 pane
- [x] 扛住设置面板自己下达的 daemon 重启：卡片留在屏上、按秒计数、自行重新订阅，而不是停在
      一个等待按键的死胡同
- [ ] 告诉 sidebar 它已经过期。升级插件后，已经开着的 sidebar 会继续跑旧二进制，界面上没有
      任何提示 —— 0.1.4 测试时我们就被它误导过。让 state socket 的 hello 带上版本，不一致
      时在底部钉一条提示
- [ ] doctor 面板里可操作的修复建议 —— `↵` 复制到剪贴板而不是直接执行，因为这些修复要改的是
      本插件之外的文件
- [ ] sidebar 的 `:` 命令模式
- [ ] 修掉 flaky 的 `pane_without_cwd_uses_herdrs_cwd_for_that_pane` 测试

## 本地开发

```sh
cargo build --release
herdr plugin link "$PWD"
herdr plugin action invoke restart-daemon --plugin herdr-agent-watcher
```

`plugin link` 按设计跳过构建步骤，工作目录由你自己构建。

## 验证

以下都基于上面的源码检出与构建。

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
