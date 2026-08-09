# tmux-recover

[English](README.md)

`tmux-recover` 是一个用 Rust 实现的 tmux 会话快照、恢复和持续保存工具。它不
依赖 tmux-resurrect 或 tmux-continuum，也不修改 `status-right`。

## 功能

- 通过单个持久 tmux control-mode 连接抓取完整 server 状态；
- 保存 session、linked/grouped window、pane、cwd、title、layout、active 和 zoom；
- 结构变化由 tmux hooks 触发，cwd、title 和进程变化由低频轮询发现；
- 使用 canonical connection path 计算 socket identity，使 symlink、相对路径以及
  macOS 的 `/var` 与 `/private/var` 别名共享同一个 store 与锁，即使 tmux 回报原始拼写；
- 在 daemon 和 CLI 之间串行化 capture 与跨文件发布，避免延迟的旧 capture 把
  `current` 回退到新 save 之前；
- JSON 区分空字符串、`null` 和缺失值，并保留非 UTF-8 Unix 路径；
- 恢复前执行 preflight，可逆阶段保留旧 session，并写入持久 restore report；
- 使用独立 checkpoint sidecar 跟踪 pane 当前运行的程序；
- 导入 tmux-resurrect v3/v4，但永远不信任导入的命令文本。

要求 tmux 3.7+ 和 Rust 1.85+。支持 Linux 与 macOS；Linux 通过 `/proc` 采集
可选的进程重启元数据。

## 安装

```sh
cargo install --path . --locked
```

安装到 `~/.local/bin`：

```sh
./scripts/install.sh
```

### TPM

```tmux
set -g @plugin 'gle/tmux-recover'

# 可选；默认 C-s 保存，C-r 安全恢复。
set -g @tmux-recover-save-key 'C-s'
set -g @tmux-recover-restore-key 'C-r'

run '~/.tmux/plugins/tpm/tpm'
```

TPM 入口会为当前 canonical tmux socket 启动一个后台 daemon。重复启动会因
daemon 单实例锁而退出。binary 不在 `PATH` 时，请在启动 tmux 前设置
`TMUX_RECOVER_BIN`。

TPM 日志位于 `${XDG_STATE_HOME:-~/.local/state}/tmux-recover/tpm.log`。

## CLI

```sh
# 当前 TMUX socket，或默认 socket。
tmux-recover save
tmux-recover list
tmux-recover show current --json
tmux-recover validate current

# 显式指定 server；socket 别名会被 canonicalize。
tmux-recover save --socket /tmp/tmux-1000/other

# 前台 daemon，适合 systemd/launchd。
tmux-recover daemon --socket /tmp/tmux-1000/default
```

无 label 的 `save` 会对未变化结构去重并输出 `unchanged`。`--label` 总会写入一条
历史；结构未变化时，`--pin` 会 pin 当前已存快照，而不是复制一份。

### 恢复

先执行 dry-run：

```sh
tmux-recover restore 20260801T212922 --dry-run
```

默认只允许替换一个无显式启动命令的 1 session / 1 window / 1 pane bootstrap。
替换真实工作状态需要明确 review 和确认：

```sh
tmux-recover restore SNAPSHOT --dry-run --replace
tmux-recover restore SNAPSHOT --replace --yes
```

cwd 缺失时 preflight 默认失败；fallback 必须显式指定：

```sh
tmux-recover restore SNAPSHOT --dry-run --cwd-fallback HOME
tmux-recover restore SNAPSHOT --cwd-fallback /known/safe/path
```

preflight 会在重命名任何 session 前校验 snapshot identity、状态图所有权、非负
window index、可重建的连续 pane index、cwd 可用性和 layout checksum。dead pane
当前会被明确拒绝，因为把它恢复成 live shell 会静默改变原状态。

恢复分为可逆阶段和 commit cleanup 阶段。commit 前失败会删除新建 session，恢复
备份名称和普通客户端附着。新状态完整建立且客户端完成切换后，删除旧备份是不可逆
操作；因此 cleanup 失败会保留新状态，并在 restore report 中记录 warning，而不会
执行可能同时丢失新旧状态的 rollback。

每次非 dry-run 恢复都会记录一份恢复前 safety snapshot。safety snapshot 使用独立、
有上限的保留策略，在 `list` 中以 `!` 标记；用户 pin 使用 `+` 标记，并一直保留到
显式 unpin。旧版本以普通 pin 保存的 safety snapshot 会继续保持 pin，可通过
`tmux-recover unpin SNAPSHOT` 释放。

### 进程恢复

进程默认不恢复。只有显式传入 `--restore-processes`，且 native restart metadata
为 trusted、可执行文件 basename 位于 `restore.process_allowlist` 时才会启动。
导入的 resurrect 命令文本永远不会执行。

恢复程序运行在固定的 `/bin/sh` supervisor 中。程序启动前重置 SIGINT/SIGQUIT，
程序退出后进入目标 tmux server 捕获的 `default-shell`，避免 `C-c` 连同 pane 一起
杀掉。已知限制：`C-z` 可能暂停程序而 supervisor 继续等待，使 pane 卡住。

#### 进程 checkpoint sidecar

结构去重意味着历史快照可能早于 pane 当前运行的程序。`process-current.json` 在不
追加历史的情况下补齐这部分状态，记录 `pane_id`、`current_command` 和 `restart`，
按独立 interval 刷新，并仅在相关进程状态变化时原子覆盖。

sidecar 只会用于同一 socket store 的 `current` + `--restore-processes`，并要求
snapshot ID、structural hash、socket identity、server generation 和完整 pane 集合
全部匹配。否则恢复会回退到 snapshot 自带元数据并报告 warning。sidecar 中的
`restart: null` 是权威状态，会压制该 pane 更旧的 snapshot restart metadata。

dry-run 和 JSON plan 会输出 `process_metadata_source` 与 checkpoint 捕获时间。

### tmux-resurrect 导入

```sh
tmux-recover import-resurrect \
  ~/.local/share/tmux/resurrect/tmux_resurrect_20260801T212922.txt

tmux-recover list --imports
tmux-recover show --imports current --json
tmux-recover restore --from-imports current --dry-run --cwd-fallback HOME
```

导入快照保存在独立 store 中，并且永远不做结构去重。导入器识别 v3/v4，只有在字段
签名明确时才修复已知的 v4 空 title 错位；repaired、ambiguous 和 lossy 行都会写入
diagnostics。

## 自动恢复

自动恢复默认关闭：

```toml
[restore]
auto = true
auto_bootstrap_max_age_seconds = 30
```

daemon 仅自动恢复足够新的 1/1/1 bootstrap，并要求 pane 正在运行 server 的
`default-shell` 且没有显式 start command。preflight 失败不会修改 server，daemon
会继续监控。

## 配置与数据

默认配置路径：

- Linux: `${XDG_CONFIG_HOME:-~/.config}/tmux-recover/config.toml`
- macOS: `~/Library/Application Support/dev.tmux-recover.tmux-recover/config.toml`

完整配置见 [config.example.toml](config.example.toml)。重要的 daemon 与存储配置：

```toml
[autosave]
hook_slot = 901
process_checkpoint_interval = 300

[retention]
safety_snapshots = 10
```

daemon 使用 tmux 原子的 set-if-absent option update 安装持久 `wait-for` event hook。
之前 daemon 留下的相同 hook 会被复用；`hook_slot` 中的其他命令会保持不变，并使启动
失败。hook 可跨 control connection 重连和 daemon 重启继续使用，因此 shutdown 不再
执行存在竞态的 check-and-remove。应使用专用 slot。

Linux 默认数据目录是 `${XDG_DATA_HOME:-~/.local/share}/tmux-recover`：

```text
sockets/<socket-key>/
  snapshots/*.json[.zst]
  current.json
  process-current.json
  pins/
  safety/
  restores/*.json
  daemon.lock
  mutation.lock
imports/
```

默认保留最新 100 份、30 天内每小时一份、180 天内每天一份，以及最新 10 份 safety
snapshot。current 和用户 pin 不会被清理。`storage.zstd = true` 可启用压缩快照。

每个历史文件名必须严格为 `<snapshot.id>.json` 或 `<snapshot.id>.json.zst`。直接读取
和 pin 会拒绝不一致文件；`list` 会跳过，retention 会记录 warning 并保留原文件。

systemd user service 模板位于
[contrib/systemd/tmux-recover@.service](contrib/systemd/tmux-recover@.service)。实例名必须
是 socket 路径经 `systemd-escape` 转义后的结果：

```sh
instance="$(systemd-escape '/tmp/tmux-1000/default')"
systemctl --user enable --now "tmux-recover@${instance}.service"
```

Linux/macOS 上仍推荐使用 TPM 集成 server 生命周期。

## 设计与限制

- [架构与恢复事务](docs/architecture.md)
- [snapshot schema 与原子存储](docs/snapshot-format.md)
- v1 不保存 scrollback 或 pane contents；
- dead pane 会被捕获，但恢复会在修改 server 前拒绝；
- pane title 可能被 shell 或程序通过 OSC 合法更新；
- macOS 当前保存结构状态，但不采集 restart metadata；
- resurrect 中未转义的 Tab/换行可能无法无损消歧，导入器会记录 diagnostics，而不是
  猜测为可执行命令。

## 开发

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
```

所有 tmux-backed 测试均使用独立临时 `tmux -S` socket，不会修改当前 ambient server。
