# herdr-agent-watcher

[English](README.md) · [简体中文](README.zh-CN.md) · **日本語**

[Herdr](https://herdr.dev) 向けのコーディングエージェント可観測性プラグイン。ライブな
サイドバーカード、ライフサイクル通知、そして Claude Code 用のゼロ設定メトリクスブリッジ。

![Agent Watcher サイドバー](docs/sidebar.png)

1 つのペインに 4 つのエージェント。展開された Claude カードの CONTEXT、CACHE、COST は、
Claude Code がステータスライン経由でしか報告しない 3 つの指標であり、下記のブリッジはまさに
それを届けるために存在します。

この発想はもともと [Vimeflow](https://github.com/winoooops/vimeflow) の中にありました。
コーディングエージェントの監視は、あの Electron アプリの 1 レイヤーに過ぎませんでした。

[herdr-agent-title-sync](https://github.com/winoooops/herdr-agent-title-sync) と組み合わせて
使えます。あちらは Herdr のペインタイトルを、各エージェントの作業内容に追随させます。

**既定ではローカル観測のみです。** 唯一マシンの外に出るのは Kimi のプラン使用量取得で、
自分で有効にするまでオフのままです — [Kimi 使用量レポートの同意](#kimi-使用量レポートの同意)
を参照。

## インストール

```sh
herdr plugin install winoooops/herdr-agent-watcher
```

Herdr 0.8.0 以上が必要です。インストール時に macOS と Linux（x86_64 / arm64）向けの
ビルド済みバイナリを取得し、SHA256 を検証します。プラットフォームに対応するアセットが無い
場合や、ダウンロードに何か問題があった場合はソースからビルドします。その経路には Rust 1.88
以上が必要で、無い場合 Herdr はエラーを報告するだけで自動インストールはしません。

既に起動している Herdr サーバーにインストールした場合、デーモンは自動起動しません。
一度だけ実行してください:

```sh
herdr plugin action invoke restart-daemon --plugin herdr-agent-watcher
```

Herdr 標準 UI はサイドバー無しでも機能します。ライフサイクル通知と、
`agent_watcher_state`、`agent_watcher_phase`、`agent_watcher_model`、
`agent_watcher_context_pct`、`agent_watcher_attention`、`agent_watcher_title` といった
ペインメタデータトークンを受け取ります。**これらの名前は Herdr との連携面であり、
意図的に安定させています。**

## 検証

ペイン ID やセッション ID を露出せずに、対応する全ライブエージェントペインを走査します:

```sh
./tests/verify-live-agents.sh
./tests/verify-sidebar-state.sh
```

Tier A は決定的な fake Herdr socket に対して実行され、常に有効です:

```sh
cargo test --test e2e_fake_herdr
```

Tier B はインストール済みの Herdr バイナリを隔離した HOME/XDG ディレクトリで起動し、既定では
無視されます:

```sh
cargo test --test e2e_real_herdr -- --ignored
```

通常のテストをすべて実行するには `cargo test`。

## 利用できるコマンド

すべてのアクションは同じ方法で実行します:

```sh
herdr plugin action invoke <id> --plugin herdr-agent-watcher
```

出力はプラグインログに入ります。直近の実行は
`herdr plugin log list --plugin herdr-agent-watcher --limit 1` で確認できます。

| アクション | 何をするか | 詳細 |
| --- | --- | --- |
| `restart-daemon` | デーモンを起動 / 再起動 | |
| `stop-daemon` | デーモンを停止 | |
| `open-sidebar` | 新しい分割ペインでサイドバーを開く | [サイドバー](#サイドバー) |
| `enable-claude-bridge` | メトリクスブリッジを Claude 自身の設定に導入 | [Claude メトリクスブリッジ](#claude-メトリクスブリッジ) |
| `disable-claude-bridge` | 設定ファイルを有効化前の状態に戻す | [Claude メトリクスブリッジ](#claude-メトリクスブリッジ) |
| `doctor` | メトリクスが欠けている理由と対処を示す | [Doctor](#doctor) |
| `kimi-consent-on` | Kimi の使用量取得を許可 | [Kimi 使用量レポートの同意](#kimi-使用量レポートの同意) |
| `kimi-consent-off` | 取り消す — 再起動なしで反映 | [Kimi 使用量レポートの同意](#kimi-使用量レポートの同意) |
| `kimi-consent-status` | 現在の設定を表示 | [Kimi 使用量レポートの同意](#kimi-使用量レポートの同意) |

## 設定

設定は **プラグイン自身の** `config.toml` に置きます。Herdr のものではありません。
Herdr は認識できないテーブルを無視するため、`~/.config/herdr/config.toml` に `[daemon]` を
書いても何も起こらず、`herdr config check` が未知のセクションを報告するだけです。

プラグインの設定ディレクトリは次のコマンドで表示できます:

```sh
herdr plugin list
```

既定では `${XDG_CONFIG_HOME:-~/.config}/herdr/plugins/config/herdr-agent-watcher/` です。
`config.toml` が無ければ作成してください。

すべてのキーは任意で、不正な値はそれぞれ既定値にフォールバックするため、
1 つの誤りで失われるのはその設定だけで、プラグイン全体ではありません。

```toml
[daemon]
interval_ms = 5000     # デーモンが Herdr と調整する間隔。既定値は 1000

[list]
scope = "workspace"    # "all"（既定）は全ペイン、"workspace" はこのワークスペースだけ
```

変更の適用:

```sh
herdr plugin action invoke restart-daemon --plugin herdr-agent-watcher
```

サイドバーは次に開いたときに `[list]` を取り込みます。

`AGENT_WATCHER_INTERVAL_MS` も引き続き使え、ファイルより優先されます。読み取られるのは
シェルではなく **Herdr サーバーの**環境なので、設定するには Herdr の再起動が必要です。
その不便を解消するために、このファイルがあります。

設定が拒否されたか、代わりに何が使われたかは [`doctor`](#doctor) で確認できます。

## サイドバー

```sh
herdr plugin action invoke open-sidebar --plugin herdr-agent-watcher
```

呼び出すたびに**意図的に**新しい分割ペインが開きます。カードにはエージェントの状態、
エージェント/モデル、タイトル、コンテキスト使用量、キャッシュヒット率、コスト、ツール呼び出し
回数、最新 3 件のツールトレースが表示されます。`j`/`k` または PageUp/PageDown でスクロール、
`o`/`↵` で展開、`z` でアイドル状態のエージェントを隠し、`q`、Escape、Ctrl-C で閉じます。

`scope = "workspace"` にすると、各サイドバーは自身のワークスペースにあるペインだけを
表示します。これにより、ワークスペースごとに 1 つずつ開くことに意味が生まれます。
デーモンがまだ配置先を特定していないペインは、隠さず表示します。
`HERDR_WORKSPACE_ID` が無い場合 — Herdr ペイン外でサイドバーを実行した場合 — scope は
`all` にフォールバックし、サイドバーのフッター上部にその理由を表示します。

開く操作をキーに割り当てるには、次をプラグインではなく **Herdr の**設定
（`~/.config/herdr/config.toml`）へ追加します:

```toml
[[keys.command]]
key = "prefix+a"
type = "plugin_action"
command = "open-sidebar"
description = "Open the Agent Watcher sidebar"
```

```sh
herdr config check          # 検証する
herdr server reload-config  # Herdr を再起動せずに反映する
```

prefix の既定は `ctrl+b` なので、この割り当ては `ctrl+b` の次に `a` です。別のキーに
するときは `herdr --default-config` で空いているか確認してください。

デーモンが利用できない、または切断された場合、ペインはその旨を表示してキー入力を待ちます。
state socket（`$HERDR_PLUGIN_STATE_DIR/herdr-agent-watcher-state.sock`、
`WIRE_VERSION = 2`）はプラグイン内部のもので、公開 API ではありません。

デーモンの停止:

```sh
herdr plugin action invoke stop-daemon --plugin herdr-agent-watcher
```

## 対応エージェント

| エージェント | メトリクスの出どころ | ブリッジ | 状態 |
| --- | --- | --- | --- |
| Claude Code（`claude`、`claude-code`）| ステータスライン**のみ** | **必須** — `enable-claude-bridge` 1 回 | ✅ |
| Codex CLI（`codex`）| rollout トランスクリプト | 不要 | ✅ |
| Kimi Code（`kimi`）| トランスクリプトと、任意の使用量 API | 不要 | ✅ |
| OpenCode（`opencode`）| 同梱のブリッジプラグイン | 初回バインド時に自動インストール | ✅ |

自分で有効化が必要なブリッジを持つのは Claude だけです。理由は 1 つ、**Claude が発する
フックイベントは使用量データを一切運ばない**ためで、ステータスラインが唯一の経路です。
OpenCode のブリッジは本プラグインが代わりに入れます。Codex と Kimi には何も要りません。

エージェントを追加するには、`src/agents/` 配下に `AgentAdapter` を実装し、
`src/daemon/run.rs` の `AgentRegistry` に登録します。エージェント固有のパースはアダプタに置き、
Herdr socket の詳細は `HerdrPort` の背後に留めます。

## Claude メトリクスブリッジ

Claude Code は CONTEXT、CACHE、COST をステータスライン経由で**しか**報告しないため、
他の 3 つのエージェントには不要なブリッジが必要です。

```sh
herdr plugin action invoke enable-claude-bridge --plugin herdr-agent-watcher
```

これは Claude 自身のユーザー設定を編集し（どのファイルかを出力します）、既存の
ステータスラインをブリッジの後段につないでそのまま動かします。`PATH` の変更も新しい
シェルも不要で、**どのペインのどの Claude もブリッジされます**。既に実行中のセッションも
含み、それらは次のステータスライン描画時に取り込まれます。

```sh
herdr plugin action invoke disable-claude-bridge --plugin herdr-agent-watcher
```

は設定ファイルを元に戻し、元々 `statusLine` が無ければそのキーごと削除します。**有効化後に
自分で変更した statusLine には触れません** — ブリッジは今も自分のものであるものだけを
戻します。

## Doctor

カードに `— bridge not connected (README)` と出たとき、あるいはメトリクスが欠けていて
理由を知りたいときに実行します。

```sh
herdr plugin action invoke doctor --plugin herdr-agent-watcher
herdr plugin log list --plugin herdr-agent-watcher --limit 1
```

doctor は原因と対処法を示します。**代わりに直せない唯一のケース**は、プロジェクトが独自の
`statusLine` を持つ場合です。これはユーザー層より優先されるため、そのプロジェクトの
`.claude/settings.local.json` に貼り付けるブロックを出力し、プロジェクトのステータスラインと
メトリクスの両方を保ちます。

情報が不完全なまま「正常」とは報告しません。管理された設定はここからは読めないので、
すべてのチェックが通ってもメトリクスが欠けている場合は、そのことを明示します。

## Kimi 使用量レポートの同意

Kimi のプラン使用量取得は、設定された API キーを `/usages` エンドポイントに送信するため、
**既定では無効**で、明示的に有効化するまで動作しません。Herdr プラグインアクション
`kimi-consent-on`、`kimi-consent-off`、`kimi-consent-status` を使用してください。取り消しは
実行中のデーモンに再起動なしで反映されます。

## OpenCode ブリッジ

最初の OpenCode バインド時に、同梱のブリッジを OpenCode のプラグインディレクトリへインストール
または更新します。`AGENT_WATCHER_OPENCODE_PLUGINS_DIR` と
`AGENT_WATCHER_OPENCODE_BRIDGE_DIR` でインストール先とイベントディレクトリを上書きできます。
このブリッジプラグインは `agent-watcher-opencode-bridge` というファイル名のままです。移植された
サイドカーツリー内にあり、そのツリーは凍結されているためです。

## 設計ノート

[`DESIGN.md`](DESIGN.md) は、このプラグインがこの形になっている理由を記録しています。
なぜ Claude だけがブリッジを必要とするのか、なぜ `PATH` を横取りせず Claude 自身の設定に
入れるのか、なぜ書き込み先をデーモンが所有するのか、そして壊れたブリッジが何に劣化
しなければならないのか。

## 既知の制限

- **デーモン起動前から開いていたペインにはカードが出ません。** Herdr がその
  `agent_session` を報告しないため束縛できません。ペインを閉じて開き直してください。
- **ワークスペース間で移動したペインは無効な id を保持し続けます。** プロセスの pane id は
  exec 時に確定するため、デーモンはそれを認識できません。doctor は静かに失敗する代わりに
  これを表示しますが、セッション自体は作り直す必要があります。
- **同時に扱える Herdr セッションは 1 つだけです。** デーモンのロックとソケットはユーザー
  全体で共有される `$HERDR_PLUGIN_STATE_DIR` にあり、2 つ目のセッションは 1 つ目の
  デーモンを置き換えます。
- **`attention.jsonl` は無制限に増えます。** 追記のみで、1 ペイロード 192KB の上限はあり
  ますが、総量の上限もローテーションもありません。
- **セッションの最初のターンで完了通知が出ないことがあります。** また 5 つのフックのうち
  4 つはペイロードをそのまま保存するため、保証は「プロンプトは永続化しない」であって
  「ユーザーのテキストを一切永続化しない」では**ありません**。どちらも凍結された
  `src/agent/**` ツリー内にあります。

## 今後の課題

- [x] `AGENT_WATCHER_INTERVAL_MS` の代わりに、プラグイン自身の `config.toml` から設定を
      読む。設定が「Herdr サーバーがどの環境から起動されたか」に依存しなくなる
- [ ] 不要になったブリッジディレクトリの回収。判定は**生存性**で — pane id が Herdr の
      ペイン一覧に無く、**かつ**どのプロセスも保持していないこと。unbind では絶対に行わず
      （rebind が通常の流れ）、mtime でも行わない（開いたままの長時間アイドルなペインを
      巻き込む）
- [x] リリースごとにビルド済みバイナリを公開し、インストールに `cargo` を不要にする。
      `[[build]]` は `scripts/fetch-or-build.sh` を実行し、プラットフォームに合うアセットを
      取得して SHA256 を検証、失敗時はコンパイルにフォールバックする
- [ ] サイドバーの `:` コマンドモードと、全画面の doctor ビュー
- [ ] flaky な `pane_without_cwd_uses_herdrs_cwd_for_that_pane` テストの修正

## ローカル開発

```sh
cargo build --release
herdr plugin link "$PWD"
herdr plugin action invoke restart-daemon --plugin herdr-agent-watcher
```

`plugin link` は設計上ビルド手順をスキップします。作業ディレクトリのビルドは自分で行って
ください。
