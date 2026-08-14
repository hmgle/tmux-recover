# tmux-recover

[![CI](https://github.com/hmgle/tmux-recover/actions/workflows/ci.yml/badge.svg)](https://github.com/hmgle/tmux-recover/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/hmgle/tmux-recover?sort=semver)](https://github.com/hmgle/tmux-recover/releases/latest)
[![License](https://img.shields.io/github/license/hmgle/tmux-recover)](LICENSE)
[![tmux 3.7+](https://img.shields.io/badge/tmux-3.7%2B-1BB91F)](https://github.com/tmux/tmux/releases)
[![Rust 1.85+](https://img.shields.io/badge/Rust-1.85%2B-DEA584?logo=rust)](Cargo.toml)

[English](README.md)

`tmux-recover` 持续保存 tmux 会话，并在重启、崩溃或 server 意外退出后恢复工作
环境。它会记住 session/window/pane 结构、工作目录、名称、布局、当前选择和 zoom
状态，同时不会接管你的状态栏。

恢复操作有意采用保守策略：替换真实工作前先校验快照并展示计划；真正恢复前创建
安全快照；提交前的步骤失败时自动回滚。

## 为什么选择 tmux-recover？

- **持续历史：**结构变化后快速保存，同时低频检查工作目录、标题等不易触发事件的
  信息；
- **安全恢复：**提供 dry-run 预检、显式替换、回滚、安全快照和持久恢复报告；
- **每个 tmux server 独立：**不同 socket 的历史互不混淆，包括通过软链接或不同
  路径写法访问的同一个 socket；
- **有上限的存储：**保留近期、每小时和每日历史，重要快照可长期 pin；
- **平滑迁移：**可导入 tmux-resurrect v3/v4 文件，但不会执行导入的命令文本；
- **方便自动化：**默认输出适合人阅读，也可用 JSON 做检查或脚本处理。

支持 Linux 和 macOS，要求 tmux 3.7+。源码构建要求 Rust 1.85+。进程重启元数据
目前只在 Linux 上采集且必须显式启用；普通会话恢复在两个平台都可使用。

## 快速开始

### 1. 安装 binary

从 [最新 Release](https://github.com/hmgle/tmux-recover/releases/latest) 下载预编译包：

| 平台 | Release target |
| --- | --- |
| Linux x86_64 | `x86_64-unknown-linux-musl` |
| macOS Intel | `x86_64-apple-darwin` |
| macOS Apple 芯片 | `aarch64-apple-darwin` |

例如 Linux x86_64 可执行：

```sh
target=x86_64-unknown-linux-musl
archive="tmux-recover-$target"
curl -fLO "https://github.com/hmgle/tmux-recover/releases/latest/download/$archive.tar.gz"
curl -fLO "https://github.com/hmgle/tmux-recover/releases/latest/download/$archive.tar.gz.sha256"
if command -v sha256sum >/dev/null 2>&1; then
  sha256sum -c "$archive.tar.gz.sha256"
else
  shasum -a 256 -c "$archive.tar.gz.sha256"
fi
tar -xzf "$archive.tar.gz"
install -d "$HOME/.local/bin"
install -m 0755 "$archive/tmux-recover" "$HOME/.local/bin/tmux-recover"
install -d "$HOME/.local/share/zsh/site-functions"
install -m 0644 "$archive/completions/_tmux-recover" \
  "$HOME/.local/share/zsh/site-functions/_tmux-recover"
```

请确保启动 tmux 的环境能在 `PATH` 中找到 `$HOME/.local/bin`。

也可以用 Cargo 构建当前 `main` 分支：

```sh
cargo install --git https://github.com/hmgle/tmux-recover --locked
```

在源码或 TPM checkout 中运行 `./scripts/install.sh`，会下载并校验最新 Release
binary，再安装到 `${PREFIX:-$HOME/.local}/bin`，普通用户不需要 Rust。

本地维护者如需验证当前 checkout（包括未提交的改动），运行：

```sh
./scripts/install.sh --local
```

Release 压缩包和 `scripts/install.sh` 都包含 zsh 自动补全。请确保安装目录在
`compinit` 运行前已加入 `fpath`，例如在 `.zshrc` 中配置：

```zsh
fpath=("$HOME/.local/share/zsh/site-functions" $fpath)
autoload -Uz compinit
compinit
```

补全内容包括子命令、选项、路径参数，以及所选 native/import 历史中的 snapshot ID。
安装或升级后，请启动新的 shell，或再次运行 `compinit` 刷新 zsh 补全缓存。

### 2. 启用 TPM 集成

在 `.tmux.conf` 中把插件放在 TPM 初始化之前：

```tmux
set -g @plugin 'hmgle/tmux-recover'

# 可选：按下 tmux prefix 后使用的按键。
set -g @tmux-recover-save-key 'C-s'
set -g @tmux-recover-restore-key 'C-r'

run '~/.tmux/plugins/tpm/tpm'
```

按 `prefix` + <kbd>I</kbd> 安装插件，然后重新加载 tmux 配置。某个 socket 首次激活
插件时，启动脚本会在返回前同步写入一份基线快照；初始化不会覆盖已有历史。随后
TPM 会为当前 tmux server 启动一个后台 watcher。`prefix` + <kbd>Ctrl-s</kbd>
立即保存；在空的初始 server 中，`prefix` + <kbd>Ctrl-r</kbd> 恢复最新快照。
如果按键与现有配置冲突，请修改上面的两个 option。

恢复快捷键会把最后结果显示在 tmux 状态栏。如果 server 中已有手动创建的窗口，
提示会直接说明恢复被安全策略拦截；请先选择要恢复的快照，再使用下面的 dry-run
和 `--replace` 命令。

如果 tmux 的 `PATH` 找不到 binary，请在启动 tmux 前导出绝对路径：

```sh
export TMUX_RECOVER_BIN="$HOME/.local/bin/tmux-recover"
```

TPM 不是必需项。可以在自己的 supervisor 下运行 `tmux-recover daemon` 持续保存，
也可以只手动执行 `tmux-recover save`。

## 日常使用

### 保存和查看历史

```sh
# 立即保存；布局未变化时会去重。
tmux-recover save

# 即使布局未变化，也记录并长期保留一个有名称的检查点。
tmux-recover save --label before-upgrade --pin

# 列出当前 tmux server 的历史。
tmux-recover list

# 查看或校验最新快照。
tmux-recover show current
tmux-recover show current --json
tmux-recover validate current

# 输出 JSON，便于筛选或脚本处理。
tmux-recover list --json
```

使用 `--help` 可以查看全部命令以及某个命令的参数说明：

```sh
tmux-recover --help
tmux-recover save --help
tmux-recover restore --help
tmux-recover daemon --help
```

列表前缀中，`*` 表示当前快照，`+` 表示用户 pin，`!` 表示有数量上限的恢复前安全
快照。下面示例里的 `SNAPSHOT` 可替换成 `list` 输出的 ID。

`list` 的普通输出按以下格式显示：

```text
<当前><pin><安全快照><快照 ID>  <创建时间>  <session>s/<window>w/<pane>p  <标签>
```

前三个位置分别表示当前、用户 pin 和恢复前安全快照；没有对应标记时显示空格。
创建时间使用 UTC，ID 中的 `Z` 和时间列中的 `+00:00` 都表示 UTC。快照 ID 由
UTC 创建时间和状态语义哈希的前 16 个十六进制字符组成。`s/w/p` 分别是 session、
window 和 pane 的数量。最后一列是通过 `save --label` 设置的标签，没有标签时为空。

例如，`*  20260809T133813.912729Z-b45b6e7b2e326e7a  ...  1s/3w/5p` 表示当前
快照包含 1 个 session、3 个 window 和 5 个 pane；`  !...  pre-auto-restore ...`
表示恢复前自动创建的安全快照。恢复指定历史时，使用完整 ID 或不会产生歧义的 ID
前缀；日期本身可能匹配多个快照，因此不能作为可靠的唯一选择器。

```sh
tmux-recover pin SNAPSHOT
tmux-recover unpin SNAPSHOT
```

### 查看、重载或停止 watcher

daemon 控制命令按同一个 canonical tmux socket 和 data directory 定位 watcher。
管理非默认实例时，应明确指定两者：

```sh
# 普通或 JSON 状态，包括 PID 和当前运行版本。
tmux-recover daemon --socket /tmp/tmux-1000/default --status
tmux-recover daemon --socket /tmp/tmux-1000/default --status --json

# 重新读取配置，并执行磁盘上当前安装的 binary。
tmux-recover daemon --socket /tmp/tmux-1000/default --reload

# 干净退出，但不启动替代进程。
tmux-recover daemon --socket /tmp/tmux-1000/default --stop
```

`--reload` 保留原 PID 和命令行，因此 TPM watcher 仍保持原来的 detached 关系，
systemd watcher 也仍归原 unit 管理。命令会等待替代进程重新发布状态，并确认运行版本
与发出控制请求的 binary 一致；`config.toml` 也会重新读取。

`--stop` 有意不启动新 watcher。由 supervisor 管理时，优先使用 supervisor 自己的
stop 命令。控制命令不会解析配置文件，因此无效配置不会阻止 status 或干净退出。
daemon 使用 `--data-dir` 或 `TMUX_RECOVER_DATA_DIR` 时，控制命令必须使用同一个值。
设置了 `XDG_RUNTIME_DIR` 时，控制 socket 位于 `$XDG_RUNTIME_DIR/tmux-recover`，
因此 daemon 和控制命令也必须看到相同的 runtime directory。

### 重启或 server 退出后恢复

先启动 tmux，再预览最新恢复计划：

```sh
tmux-recover restore current --dry-run
tmux-recover restore current
```

如果真实恢复命令从目标 server 的 pane 内启动，请使用 TPM 恢复按键，或让命令脱离该
pane 的前台终端生命周期运行：

```sh
tmux run-shell -b 'tmux-recover restore current'
```

如果真实恢复会在 durable report 写入前销毁调用它的 pane，dry-run 会把该问题写入
warning；真实执行会在交互确认前拒绝。此时使用上述按键或后台形式，让 report 能完整
写入。

第二条命令只能替换刚创建的 1 session / 1 window / 1 pane 空白 server。如果目标
server 已经有真实工作，必须先检查显式替换计划：

```sh
tmux-recover restore SNAPSHOT --dry-run --replace
tmux-recover restore SNAPSHOT --replace --yes
```

不传 `--yes` 会进行交互确认。原工作目录不存在时，preflight 会失败，不会悄悄换到
其他目录。确认原目录已不再需要时，可以自行选择 fallback：

```sh
tmux-recover restore SNAPSHOT --dry-run --cwd-fallback HOME
tmux-recover restore SNAPSHOT --cwd-fallback /known/safe/path
```

快照默认绑定原始 hostname、uid 和 canonical tmux socket。如果明确要把快照迁移到
其他主机、用户或 socket，先仔细检查快照，再显式跳过来源身份校验：

```sh
tmux-recover restore SNAPSHOT --dry-run --replace --allow-origin-mismatch
tmux-recover restore SNAPSHOT --replace --yes --allow-origin-mismatch
```

只有确认快照来源和内容可信时才使用 `--allow-origin-mismatch`。工作目录、布局、schema
等其他恢复校验仍然会执行。`list`、`show`、`validate`、`import-resurrect` 和
`restore --dry-run` 支持 `--json`。真实恢复加上 `--json` 时，会先输出 JSON 预检计划，
随后输出人类可读的安全快照和报告信息，因此整个 stdout 不是单个 JSON 文档。

每次真实恢复都会先把目标 server 保存成安全快照。commit point 之前的失败会恢复
原 session 名称和客户端附着状态，结果会写入 restore report。只有普通终端 client
（`client_control_mode=0`）才算可见；保存的 current/last session 会通过明确的 client
目标恢复。某个恢复后 session 没有普通 client 时会明确报告为不可见，不会把 watcher
执行事务时可能短暂存在的 control-mode client 当成终端。

### 在 Linux 上恢复选定程序

默认不重启进程。需要时先检查计划，再显式启用：

```sh
tmux-recover restore current --dry-run --restore-processes
tmux-recover restore current --restore-processes
```

只有 native restart metadata 标记为 trusted，且可执行文件名位于 allowlist 时才会
启动。可在配置中按需修改 `restore.process_allowlist`。恢复历史快照时只使用该快照
保存的进程元数据；只有从同一 socket 显式恢复 `current` 时才会考虑实时进程检查点。
如果不需要进程恢复，可将其设为 `[]`；此后 save 会跳过 Linux `/proc` 扫描，执行会
修改状态的 save、daemon 或 restore 时也会删除实时进程检查点。allowlist 为空时传入
`--restore-processes` 会在计划中报告为 disabled。
导入的 tmux-resurrect 命令文本永远不会执行。

### 启用自动恢复

自动恢复默认关闭。在 `config.toml` 中启用：

```toml
[restore]
auto = true
auto_bootstrap_max_age_seconds = 30
```

watcher 只会恢复刚创建的空白 server。默认 shell 和提示符辅助进程启动期间，它会对
结构上仍为空白的 pane 做短暂复查；较旧或已有内容的 server 保持不变。如果复查
超时，或 preflight 在修改 tmux 前失败，只要 server 仍是 1 session/1 window/1 pane
的 bootstrap，上一代 `current` 就会保持不变；加入真实结构后 autosave 自动恢复。
watcher 通过不附着任何 session 的一次性命令 client 执行采集和 hook 管理，因此不会
长期占用用户 session，也不会干扰其中程序的终端能力查询。只有真正执行自动恢复事务时
才会短暂创建 control-mode client，并在事务结束后释放。报告某个 session 没有普通
client 时，需要从真实终端正常 attach。

### 导入 tmux-resurrect 历史

```sh
tmux-recover import-resurrect \
  ~/.local/share/tmux/resurrect/tmux_resurrect_20260801T212922.txt \
  --label before-migration --pin

tmux-recover list --imports
tmux-recover show --imports current --json
tmux-recover restore --from-imports current --dry-run --cwd-fallback HOME
```

导入记录保存在单独的历史中。遇到有歧义或有损的旧格式行时会报告问题而不是猜测，
导入的命令文本始终不可执行。
完整的切换、验证和回退步骤见
[从 tmux-resurrect 与 tmux-continuum 迁移](docs/migrating-from-resurrect.zh-CN.md)。

### 使用多个 tmux server

命令默认使用 `$TMUX` 中的 socket；不在 client 内运行时使用 tmux 默认 socket。
需要时可显式选择其他 server：

```sh
tmux-recover save --socket /tmp/tmux-1000/other
tmux-recover list --socket /tmp/tmux-1000/other
tmux-recover daemon --socket /tmp/tmux-1000/other
```

## 配置与数据

把 [config.example.toml](config.example.toml) 复制到平台配置路径，只修改需要的值：

- Linux：`${XDG_CONFIG_HOME:-$HOME/.config}/tmux-recover/config.toml`
- macOS：`~/Library/Application Support/dev.tmux-recover.tmux-recover/config.toml`

如需对单次命令或另一套存储单独指定路径，可覆盖默认发现结果：

```sh
tmux-recover --config /path/to/config.toml list
tmux-recover --data-dir /path/to/tmux-recover-data list
```

`--data-dir` 也可以通过 `TMUX_RECOVER_DATA_DIR` 设置。列出、查看和恢复同一个快照时，
应保持 data directory 和 socket 选择一致。

默认保留最新 100 份快照、30 天内每小时一份、180 天内每天一份，以及最新 10 份
恢复前安全快照。current 和用户 pin 不会被清理。设置 `storage.zstd = true` 可启用
zstd 压缩。

Linux 数据默认位于 `${XDG_DATA_HOME:-$HOME/.local/share}/tmux-recover`，macOS 使用
标准 Application Support 目录。每个 canonical tmux socket 都有独立子目录。快照
可能包含工作目录、标题和进程参数，未经脱敏不要公开。

TPM 日志位于 `${XDG_STATE_HOME:-$HOME/.local/state}/tmux-recover/tpm.log`。binary
找不到、tmux 版本过旧、cwd 恢复失败、hook slot 冲突和旧开发版 hook 的处理方法见
[故障排查](docs/troubleshooting.md)。

## 升级与卸载

在源码或 TPM checkout 中，不需要 Rust 即可升级到最新预编译 Release：

```sh
./scripts/install.sh
```

Cargo 安装可执行：

```sh
cargo install --git https://github.com/hmgle/tmux-recover --locked --force
```

如果安装脚本本身也需要更新，先在源码 checkout 中运行 `git pull --ff-only`。本地
维护者使用 `./scripts/install.sh --local`。如果要安装到 `~/.cargo/bin`，设置：

```sh
PREFIX="$HOME/.cargo" ./scripts/install.sh --local
```

两种模式都会先写入临时文件，再原子替换目标 binary，因此已有 watcher 运行时不会
再出现 `Text file busy`。使用 TPM 时，按 `prefix` + <kbd>U</kbd> 更新插件 checkout，
再单独运行安装脚本。

只替换磁盘文件不会替换已经运行的 watcher。手动 CLI 会立即使用新 binary，而
watcher 会继续运行旧代码，直到 reload 或退出。可以原子安装后再重载一个精确实例：

```sh
socket="$(tmux display-message -p '#{socket_path}')"
./scripts/install.sh --reload-daemon --socket "$socket"
```

reload 必须显式启用。安装成功但 reload 失败时，新 binary 仍会保留，脚本会返回错误并
说明 watcher 尚未更新。本地维护构建可以同时使用 `--local` 和上述 reload 选项。
Cargo 安装完成后单独执行：

```sh
tmux-recover daemon --socket "$socket" --reload
```

早于控制协议的旧 daemon 无法接收第一次 reload。TPM watcher 可以等到该 tmux server
下次启动时采用新版。systemd 实例应重启精确 unit：

```sh
instance="$(systemd-escape "$socket")"
systemctl --user restart "tmux-recover@${instance}.service"
```

不要仅为升级 tmux-recover 而终止 tmux server。旧 TPM watcher 需要立即完成一次性
重启时，见[故障排查](docs/troubleshooting.md)。

卸载前先删除 TPM 配置或 supervisor unit，避免再次启动。然后按安装方式执行：

```sh
# 通过 scripts/install.sh 安装
./scripts/uninstall.sh

# 通过 cargo install 安装
cargo uninstall tmux-recover
```

配置、快照、报告和 TPM 文件会有意保留。请先备份或确认不再需要，再单独清理。

## 安全模型与限制

- 修改目标 server 前会校验快照 identity、对象引用、index、工作目录和 tmux layout；
- 可逆阶段会保留现有 session；只有新状态完整建立、客户端完成切换后，才开始删除旧
  backup；
- 普通终端 client 的 current/last session 会被保存和恢复；control-mode client 不参与
  可见性统计和 client 切换；
- capture 和多文件更新会串行执行，避免延迟的旧保存覆盖较新的 current；
- snapshot schema v1 不保存 scrollback 和 pane 内容；
- dead pane 会被抓取，但恢复时暂时拒绝，因为重建成 live shell 会改变原状态含义；
- macOS 暂不采集程序重启元数据；
- shell 或程序可通过 terminal escape sequence 再次修改 pane title。

更多实现细节见[架构与恢复事务](docs/architecture.md)和
[快照格式与原子存储](docs/snapshot-format.md)。

## 参与贡献

开发检查和隔离 tmux 测试要求见 [CONTRIBUTING.md](CONTRIBUTING.md)；敏感问题请按
[SECURITY.md](SECURITY.md) 私下报告；用户可见变更记录在
[CHANGELOG.md](CHANGELOG.md)。

tmux-recover 使用 [MIT License](LICENSE)。
