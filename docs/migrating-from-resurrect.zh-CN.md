# 从 tmux-resurrect 与 tmux-continuum 迁移

本指南用于把现有 TPM 配置迁移到 tmux-recover，同时保留旧快照。tmux-recover 可导入
tmux-resurrect v3 和 v4 文件，但会把它们放在独立历史中，避免迁移过程悄悄覆盖 native
快照。

## 功能对应关系

| 原有行为 | tmux-recover 对应功能 |
| --- | --- |
| `prefix` + <kbd>Ctrl-s</kbd> | 默认使用同一按键立即创建 native 快照 |
| `prefix` + <kbd>Ctrl-r</kbd> | 默认使用同一按键恢复最新 native 快照 |
| Continuum 定时保存 | 事件驱动保存，无法使用 hook 时退回轮询 |
| `@continuum-restore 'on'` | 在 `config.toml` 中设置 `[restore] auto = true` |
| Continuum 自动启动 tmux | 不提供；需要保留或替换独立的启动服务 |

`@continuum-save-interval` 没有直接对应项。watcher 会对结构事件做 debounce，限制最短
写入间隔，并轮询 tmux 未通过 hook 暴露的变化。相应配置见
[`config.example.toml`](../config.example.toml)。

自动恢复也比 Continuum 更保守：只会在
`restore.auto_bootstrap_max_age_seconds` 时间内替换刚创建的
1 session / 1 window / 1 pane 空白 server；较旧或非空 server 不会被修改。

## 1. 创建最后一份 resurrect 快照

暂时保留旧插件，按一次 `prefix` + <kbd>Ctrl-s</kbd>，看到保存成功提示后再修改
`.tmux.conf`。

如果设置了 `@resurrect-dir`，以下命令会显示快照目录：

```sh
tmux show-options -gqv @resurrect-dir
```

未设置时，如果 `~/.tmux/resurrect` 已存在，tmux-resurrect 会使用该目录；否则使用
`${XDG_DATA_HOME:-$HOME/.local/share}/tmux/resurrect`。`last` 链接指向最新文件，带
时间戳的文件名格式为 `tmux_resurrect_YYYYMMDDTHHMMSS.txt`。

迁移期间不要删除该目录。导入只读取源文件，不会修改它。

## 2. 安装 binary 并导入快照

按[快速开始](../README.zh-CN.md#快速开始)中的任一方式安装 tmux-recover binary，
但不要同时加载两套 TPM 集成：它们默认使用相同的保存/恢复按键，而且会同时运行两个
后台保存程序。

先导入最新快照；需要永久保留时可同时 pin：

```sh
tmux-recover import-resurrect \
  /path/to/tmux_resurrect_YYYYMMDDTHHMMSS.txt --pin
```

对确实需要保留的旧检查点重复执行即可。导入记录与各 socket 的 native 历史分开存放；
导入文件不会变成自动恢复使用的 native 快照。

修改插件前先验证结果：

```sh
tmux-recover list --imports
tmux-recover validate --imports current
tmux-recover show --imports current --json
tmux-recover restore --from-imports current --dry-run --replace
```

最后一条命令只打印计划。如果记录的工作目录已经不存在，应先检查提示，再显式添加
`--cwd-fallback HOME` 或一个已知安全路径，然后才能真正恢复。

导入的命令字符串只作为 metadata 保存，永远不会执行。tmux-resurrect 创建的 pane
内容归档不会被导入。修复过、有歧义或不支持的旧记录会出现在 diagnostics 中，而不会
被静默猜测。

## 3. 替换 TPM 条目

完成最后一次 resurrect 保存后，在加载 tmux-recover 前先停止当前 server 后续的
Continuum 保存：

```sh
tmux set-option -g @continuum-save-interval 0
```

这项运行时修改可避免切换期间出现两个后台保存程序；如果需要回退，旧配置可重新设置
原来的间隔。

删除 tmux-resurrect、tmux-continuum 及其 option，在 TPM 初始化前加入 tmux-recover：

```tmux
set -g @plugin 'tmux-plugins/tpm'
set -g @plugin 'hmgle/tmux-recover'

# 可选；以下是默认按键。
set -g @tmux-recover-save-key 'C-s'
set -g @tmux-recover-restore-key 'C-r'

run '~/.tmux/plugins/tpm/tpm'
```

按 `prefix` + <kbd>I</kbd> 安装新 checkout，然后重新加载 tmux 配置。watcher 启动后，
创建并保留一份 native 检查点：

```sh
tmux-recover save --label migration-complete --pin
tmux-recover list
```

确认 native 保存和恢复计划正常后，才使用 TPM cleanup（`prefix` +
<kbd>Alt-u</kbd>）移除旧插件 checkout。cleanup 不会删除 resurrect 快照目录；至少等
迁移经历一次重启和恢复测试后再考虑处理旧数据。如果 Continuum 开机启动 unit 仍引用
其 checkout，应保留该 checkout，直到启动服务被替换。

## 4. 重新启用需要的 Continuum 行为

如需替代 Continuum 自动恢复，在 tmux-recover 的平台配置文件中启用：

```toml
[restore]
auto = true
auto_bootstrap_max_age_seconds = 30
```

TPM 集成只能在 tmux server 已存在后启动 watcher。tmux-recover 不会在登录或重启后
自行创建 tmux server。如果原来由 Continuum 提供这一能力，应保留其外部启动 unit，
直到配置好等价的 tmux 启动服务；不要为了自动启动而继续加载 Continuum TPM 插件。

## 以后恢复导入的检查点

切换后仍可使用独立的导入历史：

```sh
tmux-recover restore --from-imports SNAPSHOT --dry-run --replace
# 仅在检查 dry-run 并确认目标 socket 后执行。
tmux-recover restore --from-imports SNAPSHOT --replace --yes
tmux-recover save --label imported-checkpoint-restored --pin
```

最后一条命令会把恢复后的状态写入 native 历史，之后手动或自动恢复不再需要
`--from-imports`。

## 回退迁移

如果验证失败，停止或卸载 tmux-recover 集成，并恢复两个旧 TPM 条目。导入器不会修改
原 resurrect 文件，因此它们仍可使用。排查期间保留两套快照目录，同时避免并行运行
两个后台保存程序。
