# tmux-recover

`tmux-recover` 是一个用 Rust 实现的 tmux 会话快照、恢复和持续保存工具。
它不依赖 tmux-resurrect 或 tmux-continuum，也不修改 `status-right`。

核心特性：

- 通过单个持久 tmux control-mode 连接抓取完整 server 状态；
- 保存 session、linked/grouped window、pane、cwd、title、layout、active 和 zoom；
- 结构变化由 tmux hooks 触发，cwd/title 由低频轮询发现；
- 每个 socket 独立存储和加锁，多个 tmux server 不会互相覆盖；
- JSON schema 区分空字符串、`null` 和缺失值，并支持非 UTF-8 Unix 路径；
- 快照文件和 `current.json` 指针均原子写入，失败时保留上一份有效快照；
- 独立的进程 checkpoint sidecar 跟踪 pane 当前运行的程序，不污染历史快照；
- 恢复前执行 preflight，恢复过程中保留旧 session，失败后回滚并写报告；
- 导入 tmux-resurrect v3/v4，并修复可确定的 v4 空 pane title 字段错位。

当前要求 tmux 3.7 或更新版本以及 Rust 1.85 或更新版本。支持 Linux 和
macOS；Linux 额外支持从 `/proc` 采集可选的进程重启元数据。

## 安装

```sh
cargo install --path . --locked
```

也可以安装到 `~/.local/bin`：

```sh
./scripts/install.sh
```

### TPM

```tmux
set -g @plugin 'gle/tmux-recover'

# 可选；默认 C-s 保存、C-r 安全恢复。
set -g @tmux-recover-save-key 'C-s'
set -g @tmux-recover-restore-key 'C-r'

run '~/.tmux/plugins/tpm/tpm'
```

TPM 加载脚本会为当前 tmux socket 启动一个后台 daemon。每个 server 都有
自己的 daemon 和锁；重复启动会立即退出。若 binary 不在 `PATH`，在启动
tmux 前设置 `TMUX_RECOVER_BIN`，或构建仓库内的 `target/release/tmux-recover`。

daemon 不依赖 status bar，也不要求存在普通 attached client。TPM 日志位于
`${XDG_STATE_HOME:-~/.local/state}/tmux-recover/tpm.log`。

## CLI

```sh
# 当前 TMUX socket，或默认 socket
tmux-recover save
tmux-recover list
tmux-recover show current --json
tmux-recover validate current

# 明确指定另一个 server
tmux-recover save --socket /tmp/tmux-1000/other

# 前台运行持续保存，适合 systemd/launchd
tmux-recover daemon --socket /tmp/tmux-1000/default
```

### 恢复

先执行 dry-run：

```sh
tmux-recover restore 20260801T212922 --dry-run
```

默认只允许替换一个无显式启动命令的 1 session / 1 window / 1 pane
bootstrap。已有工作状态必须明确确认：

```sh
tmux-recover restore SNAPSHOT --dry-run --replace
tmux-recover restore SNAPSHOT --replace --yes
```

cwd 不存在时 preflight 默认失败，不会静默使用 `$HOME`。需要回退时显式指定：

```sh
tmux-recover restore SNAPSHOT --dry-run --cwd-fallback HOME
tmux-recover restore SNAPSHOT --cwd-fallback /known/safe/path
```

进程默认不恢复。只有显式传入 `--restore-processes`，且 native snapshot 中的
进程可信、可执行文件 basename 位于 allowlist 时才会启动。导入的 resurrect
命令永远不会执行。

#### 进程 checkpoint sidecar

结构去重让历史保持精简，但也意味着快照只记录布局最后一次变化时正在运行的
进程。在已有 pane 里启动 `nvim` 不会产生新快照，`--restore-processes` 因此
可能恢复成一小时前的 shell。

daemon 用一个独立的 `process-current.json` 补上这个缺口：它与
`current.json` 并列，每次原子覆盖而不追加历史，只记录每个 pane 的
`pane_id`、`current_command` 和 `restart`。结构未变时最多每
`autosave.process_checkpoint_interval`（默认 300 秒）刷新一次，且仅在
`process_hash` 真正变化时才写；结构提交则立即刷新。

sidecar 描述的是"现在"，所以只有全部条件成立时恢复才会使用它：传入了
`--restore-processes`、恢复目标是本 socket store 的 `current`（历史 ID 和
`--from-imports` 都不行）、sidecar 自身 schema 与 hash 校验通过、
`base_snapshot_id` 与 `structural_hash` 与该快照一致、socket 与 server 代次
匹配、覆盖的 pane 集合与快照完全相同。任一条件不成立就回退到快照自带的
`restart` 元数据并在计划中给出 warning，session/window/pane 的恢复不受影响。
因此恢复历史 ID 永远不会把当前进程套到过去的布局上。

一旦 sidecar 通过校验，它对覆盖的**每个** pane 都是权威来源。`restart: null`
表示该 pane 当前没有可恢复的前台进程（进程已退出，或 `/proc` 读取失败），
此时会压制快照中更旧的 `restart`，而不是回退去复活那个陈旧命令。`trusted`
与 allowlist 检查始终生效。

dry-run 和 `--json` 会输出 `process_metadata_source`（`disabled`/`snapshot`/
`checkpoint`）与 checkpoint 捕获时间，便于审计这次 best-effort 进程恢复实际
用了哪份元数据、有多旧：

```text
  process restarts: 1 (from checkpoint)
  checkpoint age:   5s (captured 2026-08-07T12:00:29.105801375+00:00)
```

### resurrect 导入

```sh
tmux-recover import-resurrect \
  ~/.local/share/tmux/resurrect/tmux_resurrect_20260801T212922.txt

tmux-recover list --imports
tmux-recover show --imports current --json
tmux-recover restore --from-imports current --dry-run --cwd-fallback HOME
```

导入快照保存在独立的 `imports` store。v4 空 title 错位只有在字段签名明确时
才会修复；修复状态和丢失信息会写入 snapshot diagnostics。

## 自动恢复

自动恢复默认关闭。在配置中启用：

```toml
[restore]
auto = true
auto_bootstrap_max_age_seconds = 30
```

daemon 仅在 server 足够新、拓扑严格为 1/1/1、pane 没有显式启动命令且当前
进程等于 server `default-shell` 时自动恢复。任何 preflight 或恢复失败都会
停止 daemon 启动，不会先把 bootstrap 写成新的 current。

## 配置与数据

默认配置路径：

- Linux: `${XDG_CONFIG_HOME:-~/.config}/tmux-recover/config.toml`
- macOS: `~/Library/Application Support/dev.tmux-recover.tmux-recover/config.toml`

完整配置见 [config.example.toml](config.example.toml)。时间间隔单位均为秒。

Linux 默认数据目录是
`${XDG_DATA_HOME:-~/.local/share}/tmux-recover`。每个 socket 的目录包含：

```text
sockets/<socket-key>/
  snapshots/*.json[.zst]
  current.json
  process-current.json
  pins/
  restores/*.json
  daemon.lock
imports/
```

保留策略默认保留最新 100 份、30 天内每小时一份、180 天内每天一份；pin 和
current 不会被清理。`storage.zstd = true` 可启用 zstd。

systemd user service 模板位于
[contrib/systemd/tmux-recover@.service](contrib/systemd/tmux-recover@.service)。实例名必须是
socket 路径经 `systemd-escape` 转义后的结果：

```sh
instance="$(systemd-escape '/tmp/tmux-1000/default')"
systemctl --user enable --now "tmux-recover@${instance}.service"
```

TPM 是 Linux/macOS 上推荐的 server 生命周期集成方式。

## 设计与限制

- [架构与恢复事务](docs/architecture.md)
- [schema 与原子存储](docs/snapshot-format.md)
- v1 不保存 scrollback 或 pane contents。
- pane title 可能在恢复后被 shell 或程序通过 OSC 再次合法更新。
- macOS 当前保存结构状态，但不采集进程重启元数据。
- resurrect 的未转义 Tab/换行可能无法无损消歧；导入器会标记而不是猜测为
  可执行命令。

## 开发

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
```

tmux 3.7+ 可用时，测试会启动隔离的真实 server，覆盖特殊 cwd、空 title、
transactional restore、hook/poll autosave 和安全自动恢复。
