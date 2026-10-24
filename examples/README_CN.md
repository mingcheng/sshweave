# sshweave 配置与使用指南

[English](README.md) | **简体中文** · [产品介绍](../README_CN.md)

本指南介绍主机配置、连接路由，以及如何通过 sshweave 使用 SSH、SCP、SFTP 和 rsync。
认证和主机密钥验证仍由 OpenSSH 负责。

## 目录

- [目录](#目录)
- [安装与开始使用](#安装与开始使用)
  - [发布包安装](#发布包安装)
- [日常使用](#日常使用)
  - [Shell 别名](#shell-别名)
  - [OpenSSH Include 与自动更新](#openssh-include-与自动更新)
- [TOML 配置](#toml-配置)
  - [解析、继承和匹配规则](#解析继承和匹配规则)
  - [SSH 选项与运行时命令](#ssh-选项与运行时命令)
- [网关与故障切换](#网关与故障切换)
  - [终端路由拓扑图（不连接）](#终端路由拓扑图不连接)
  - [独立 HTTP / SOCKS5 代理](#独立-http--socks5-代理)
  - [超时与限速](#超时与限速)
- [Hooks](#hooks)
- [命令](#命令)
  - [连接复用](#连接复用)
  - [管理已实例化主机缓存](#管理已实例化主机缓存)

## 安装与开始使用

运行环境为 macOS 或 Linux，并需安装 OpenSSH。按需单独安装 `rsync`。
通过 Cargo 安装时，需要 Rust 1.88+：

```sh
cargo install sshweave --locked
```

已有本地源码时，也可在仓库根目录执行 `cargo install --path . --locked`。
不安装 Rust 时，可使用[发布包](#发布包安装)。

创建配置目录：

```sh
mkdir -p ~/.config/sshweave
```

将 [sshweave.toml](sshweave.toml) 保存为 `~/.config/sshweave/sshweave.toml`，
或合并到已有配置中。连接前，先替换示例中的域名、地址、用户名及密钥路径。
然后验证配置、预览路径并连接：

```sh
sshweave config check
sshweave topology database
sshweave config build --write
sshweave wrapper ssh database
```

修改配置后，重新运行 `config check`。它只检查配置，不检查远程主机是否可达；
`topology` 同样是离线预览。

### 发布包安装

[GitHub Releases](https://github.com/mingcheng/sshweave/releases) 的已发布制品可包含
`sshweave-VERSION-TARGET.tar.gz` 及对应 `.sha256` 文件，目标平台如下：

| TARGET                      | 平台                                     |
| --------------------------- | ---------------------------------------- |
| `x86_64-unknown-linux-gnu`  | Linux x86_64，在 Ubuntu 24.04 构建       |
| `aarch64-unknown-linux-gnu` | Linux ARM64，在 Ubuntu 24.04 构建        |
| `x86_64-apple-darwin`       | macOS Intel，部署目标版本为 13.0         |
| `aarch64-apple-darwin`      | macOS Apple Silicon，部署目标版本为 13.0 |

二进制仍需要 OpenSSH，但不需要 Rust。Linux 发布包需要兼容的
glibc；旧系统或 musl/Alpine 建议从源码构建。macOS 发布包不包含 Developer ID
签名或公证；要求这些能力的环境需使用组织认可的签名/分发流程或源码安装，
不要关闭系统安全检查。

从同一个 release 下载压缩包与校验文件，将下面的 VERSION、TARGET 替换为实际值：

```sh
# Linux:
sha256sum -c sshweave-VERSION-TARGET.tar.gz.sha256
# macOS:
shasum -a 256 -c sshweave-VERSION-TARGET.tar.gz.sha256
tar -xzf sshweave-VERSION-TARGET.tar.gz
mkdir -p "$HOME/.local/bin"
install -m 755 sshweave-VERSION-TARGET/sshweave "$HOME/.local/bin/sshweave"
```

按需将 `$HOME/.local/bin` 加入 PATH。发布包包含本指南、示例 TOML、许可证及
Bash/Fish/Zsh 补全文件；补全文件按 shell 的要求安装到对应目录。

## 日常使用

推荐使用 wrapper：在 OpenSSH 读取配置**之前**更新配置，
并透传程序参数、远程命令和退出码：

```sh
sshweave wrapper ssh -p 2222 -v database uname -a
sshweave wrapper scp file.txt database:/tmp/
sshweave wrapper sftp database
sshweave wrapper rsync -az source/ database:/srv/
```

wrapper 通过 `-F` 使用生成文件，而不是通常的 `~/.ssh/config`。
请把所需主机设置写入 TOML；若要合并生成配置与手写配置，
使用 [Include 接入方式](#openssh-include-与自动更新)。

wrapper 会以所选程序替换自身进程，因此信号与退出状态（包括被信号终止）
会原样传给调用方。可通过 `settings.scp_binary`、`settings.sftp_binary`、
`settings.rsync_binary` 指定其他客户端；SSH 使用 `settings.ssh_binary`。

程序名后的参数属于该程序：SSH 端口使用 `-p`，SCP/SFTP 使用 `-P`。
sshweave 自身的选项放在 `wrapper` 之前，例如
`sshweave --debug wrapper ssh database`；wrapper 帮助使用 `sshweave help wrapper`。

wrapper 负责 SSH 配置、路由及 rsync 远程 shell，因此会拒绝冲突的 SSH/SCP/SFTP
`-F`/`-J`、通过 `-o` 覆盖路由，以及 rsync `-e`/`--rsh`。
请使用 TOML 中的 `gateways` 或 `target/gateway` 链代替 `-J`。
若需其他配置或路由覆盖方式，请将生成文件用于原生命令。

### Shell 别名

可按需将以下别名加入 `~/.bashrc` 或 `~/.zshrc`：

```sh
alias ssh='sshweave wrapper ssh'
alias scp='sshweave wrapper scp'
alias sftp='sshweave wrapper sftp'
alias rsync='sshweave wrapper rsync'
```

随后即可使用 `ssh database`、`scp file.txt database:/tmp/` 等熟悉的命令。
通过 `command ssh database` 可临时绕过别名。Bash 登录 shell 可能需要在
`~/.bash_profile` 中加载 `~/.bashrc`。别名需要使用其他 TOML 文件时，设置
`SSHWEAVE_CONFIG`。别名仅影响当前 shell，不影响 Git 或 GUI 应用。

### OpenSSH Include 与自动更新

执行 `sshweave config build --write` 后，在**自己维护的** `~/.ssh/config`
靠前位置加入：

```sshconfig
Include ~/.config/sshweave/sshweave.conf
```

这样即可使用原生 SSH/SCP/SFTP/rsync，以及基于 OpenSSH 的 Git/GUI 应用。
OpenSSH 使用第一个生效的值，Include 的位置影响优先级。
生成配置中的 `Host *` 也会为未列出的目标启用 sshweave 路由。
需要其他代理的手写设置应放在 Include 之前，或使用专用 `ssh -F` 配置。

生成文件会记录其输入（源文件、缓存目标、引用的环境变量、sshweave 可执行文件
及版本）和自身内容的指纹。连接时若文件未被修改且仍是最新，则直接复用，
既不重新生成也不加锁；嵌套的网关跳不会重写该文件。

配置变更或首次使用新的模式主机、链式目标时，这种方式可能会更新生成文件，
并要求重新运行 SSH；wrapper 可避免这次重试。
设置 `settings.automatic_rewrite = false` 可关闭此类自动更新，
但显式 build 和 wrapper 仍会刷新配置。

sshweave 只覆盖带有自身标记的生成文件，**不会自动改写手写的
`~/.ssh/config`**。`config build` 默认输出到 stdout；推荐 `--write`，
不要将 stdout 重定向到正在读取或管理的同一个文件。

## TOML 配置

完整可用的配置结构见 [sshweave.toml](sshweave.toml)。

默认文件为 `~/.config/sshweave/sshweave.toml`。可在子命令之前使用 `-c`，
或设置 `SSHWEAVE_CONFIG` 选择其他文件。相对配置路径以 TOML 文件所在目录为基准。

只有根文件是 `~/.config/sshweave/sshweave.toml` 时，才会自动加载
`~/.config/sshweave/conf.d/*.toml`。该目录可为空或不存在。
用 `-c` 或 `SSHWEAVE_CONFIG` 指定其他文件时，不会加载 conf.d，除非该文件自己列出这个 include。
其他文件使用 `includes` 引入。
生成配置与主机缓存默认使用下例中的路径；修改 `settings.output` 后，
也要更新 OpenSSH Include。

下列示例均为配置片段。请添加到自己的配置中，或合并到已有表中，
不要重复声明同名 TOML 表。

```toml
# includes = ["extra.d/*.toml"]

[settings]
output = "~/.config/sshweave/sshweave.conf"
registry = "~/.config/sshweave/sshweave_known_hosts.json"
ssh_binary = "ssh"
scp_binary = "scp"
sftp_binary = "sftp"
rsync_binary = "rsync"
automatic_rewrite = true

[defaults]
user = "${USER}"
port = 22
connect_timeout = 10

[templates.production]
user = "deploy"

[templates.production.ssh]
IdentityFile = ["~/.ssh/id_ed25519", "~/.ssh/company_key"]
IdentitiesOnly = true

[hosts.bastion]
hostname = "bastion.example.net"
aliases = ["gate"]

[hosts.database]
inherits = ["production"]
hostname = "10.0.0.10"
gateways = ["direct", "bastion"]
gateway_strategy = "sequential"
rate_limit = "5 MiB/s"

[hosts.database.ssh]
LocalForward = ["8080 localhost:80", "9090 localhost:90"]
```

### 解析、继承和匹配规则

- includes 支持 glob、`~` 和 `${VAR}`，文件按名称排序。
  与默认 conf.d 不同，显式 include 模式必须匹配到已有文件。
  `[settings]` 只取根文件中的配置。
- 同名条目按字段合并：根文件 > 后面的 include > 前面的 include。
  `aliases` 和 `inherits` 同样如此：后面的条目省略它们（或设为 `[]`）时保留
  前面的列表，非空列表则整体替换。
  自动加载的 conf.d 位于显式 includes 之前，除非自行将它列在其他位置。
- 继承优先级：主机自身 > 第一个父项 > 后续父项，父项也可继续继承。
  先查模板，再查主机；用 `template:name` 或 `host:name` 可明确指定。
  缺失父项和循环继承均为错误。
- 匹配优先级：精确主机名 > 精确别名 > 按声明顺序匹配的 glob/regex > defaults。
  后续规则只补充尚未设置的字段。未列出的名称仍应用默认值，模板不能作为主机直接连接。
- glob 使用 `*`、`?` 和字符集合；正则使用显式 `regex:` 前缀，要求整个名字匹配。
  regex 的 hostname 可以使用 `{1}`、`{2}` 和 `{named_capture}`。
- `aliases` 不继承。省略 `hostname` 时，字面别名指向配置中的主机名称，
  模式规则则使用请求的名称。
- 列表整体覆盖，不自动追加；例如子级的 `ssh.IdentityFile` 替换父级列表。
  `gateways = []` 可关闭继承的网关；hook 的某个事件设为 `[]` 可关闭该事件。
- `${VAR}` 在路径、hostname、user、SSH 选项和命令参数中展开，变量缺失会报错。
  `$VAR` 留给 shell 命令处理；不支持 `~otheruser`。

```toml
[hosts."*.dev"]
hostname = "%n.example.net"

[hosts."regex:^worker-(?P<region>[a-z]+)-([0-9]+)$"]
inherits = ["production"]
hostname = "worker-{2}.{region}.internal"
gateways = ["bastion"]
```

### SSH 选项与运行时命令

常规 OpenSSH 选项放入 `[hosts.NAME.ssh]`、`[templates.NAME.ssh]` 或
`[defaults.ssh]`。支持字符串、整数、布尔值（转换为 yes/no）和字符串数组
（每项输出一条指令）。例如算法列表应写为一个逗号分隔的字符串；
`IdentityFile` / `LocalForward` 等可重复选项使用数组。

包含空格的路径要按 OpenSSH 语法加引号，例如
`IdentityFile = '"~/.ssh/company key"'`。选项名不区分大小写，重复选项会报错；
具体支持哪些选项由本机 OpenSSH 决定。
不要在 SSH 表中使用 `Host`、`Match` 或 `Include`。
主机名、用户、端口和超时使用 `hostname`、`user`、`port`、`connect_timeout`；
路由使用 `gateways`、`proxy` 或 `proxy_command`，而不是 SSH `ProxyJump`/`ProxyCommand`。

需要动态地址或自定义传输时，可选择配置命令数组：

```toml
[hosts.dynamic]
resolve_command = ["my-inventory", "address", "%n"]
connect_timeout = 10
proxy_command = ["nc", "%h", "%p"]
```

- `resolve_command` 不经过 shell，必须只输出一个主机名/IP；
  命令失败、输出无效或超时均会报错。
- `proxy_command` 在直连时于本机执行，使用 SSH 网关时于网关执行。
  所选工具需安装在执行位置；普通连接不需要 `nc` 或 `socat`。
- 常用 token 包括 `%h`（主机名）、`%n`（请求目标）、`%p`（端口）、
  `%r`（SSH 用户）、`%g`（网关）。本地 token 包括 `%u`（用户）、`%i`（UID）、
  `%d`（家目录）、`%l`/`%L`（完整/短主机名）、`%C`（连接哈希）和 `%%`（百分号）。
  常规 SSH 选项遵循 OpenSSH 的 token 规则。
- 用户名放在 `user` 或命令行的 `user@host` 中，不要写入 TOML 主机键名。

sshweave 自身发起的 TCP 连接（直连路线和 `ping` 探测）遵循被拨主机的
`AddressFamily`（`any`、`inet`、`inet6`）与 `BindAddress` SSH 选项。
解析出多个地址时，某次尝试失败或经过 250 ms 后即开始下一次尝试，并交替使用
IPv6 与 IPv4，因此一个不可达的地址族不会耗尽整个 `connect_timeout`。

## 网关与故障切换

可在主机、模板或 defaults 中设置 `gateways` 与 `gateway_strategy`，两者独立继承。
**默认策略是 `sequential`**，按列表顺序尝试候选。需要每次打乱时，显式设置 `gateway_strategy = "random"`。

| 策略                 | `gateways` 的含义                                         | 失败行为                   |
| -------------------- | --------------------------------------------------------- | -------------------------- |
| `sequential`（默认） | 按列表顺序尝试候选路由                                    | 依次尝试后续候选           |
| `random`             | 每次连接随机打乱全部候选路由，每个列表项尝试一次          | 依次尝试剩余候选           |
| `chain`              | 按列表顺序经过所有跳板，本机 → 第一项 → 第二项 → … → 目标 | 整条链失败，不绕过任何跳板 |

```toml
[hosts.balanced]
hostname = "10.0.0.20"
gateways = ["bastion", "edge"]

[hosts.fallback]
hostname = "10.0.0.10"
gateway_strategy = "sequential"
gateways = ["direct", "bastion"]

[hosts.private]
hostname = "10.0.0.30"
gateway_strategy = "chain"
gateways = ["edge", "bastion"]
```

此处的 `bastion` 和 `edge` 来自[示例配置](sshweave.toml)。
`gateways = []` 可清除继承的跳板机。`chain` 的每一项必须是单个 SSH 主机名或别名，
不能使用 `direct` 或 `/` 链。random/sequential 的候选可以是 `direct`、
主机或 `bastion/edge` 形式的链；网关也可配置自己的网关设置。

临时指定路由时，先写目标，再从内到外填写网关：

```sh
sshweave wrapper ssh database/bastion/edge
```

此命令按本机 → edge → bastion → database 连接，覆盖目标原有的网关和策略。
`/` 的顺序与 `chain` 列表相反。故障切换仅用于建连阶段，
不会因 SSH 认证失败而切换路径，也不会恢复已中断的会话。

### 终端路由拓扑图（不连接）

```sh
sshweave topology database
sshweave topology database/bastion/edge
sshweave topology worker-eu-42 --ascii
```

树形图展示候选路径、网关、代理，以及各主机的 `user@hostname:port`。
随机候选按配置顺序展示，不代表实际连接顺序。
`--ascii` 使用普通字符绘制分支，`NO_COLOR` 可关闭颜色。

这只是配置预览：不连接、不检查可达性、不执行 hooks 或命令，也不写入文件。
它不会检查自定义命令内部的路径、外部 OpenSSH 配置或已有复用连接。

### 独立 HTTP / SOCKS5 代理

在 `[proxies.NAME]` 中定义代理，并通过 `proxy` 引用名称。
**不要将代理 URL 放入 `gateways`。**

```toml
[proxies.local-http]
url = "http://127.0.0.1:7890"

[proxies.local-socks]
url = "socks5://127.0.0.1:7890"

[hosts.proxied]
hostname = "server.internal"
proxy = "local-http"

[hosts.proxied-chain]
hostname = "database.internal"
proxy = "local-socks"
gateway_strategy = "chain"
gateways = ["edge", "bastion"]
```

- 仅支持无需认证的 `http://` 和 `socks5://` 代理。URL 必须显式指定端口
  （1–65535），IPv6 地址需加方括号；不能包含凭据、查询、fragment，
  或结尾 `/` 之外的路径。
- `proxy` 与其他主机设置一样支持默认值、模板和继承。
  引用不存在的代理或无效 URL 会导致 `config check` 失败。
- 目标选定的代理用于本地到目标或最外层 SSH 网关的连接，嵌套路由也沿用该代理。
  目标域名由代理解析，网关无需安装代理客户端。
- `direct` 和 `gateways = []` 只取消 SSH 跳板，不取消代理。
  代理失败时**不会静默改为直连**。
- 同一段传输不要同时使用 `proxy` 和 `proxy_command`；
  本地代理仍可与 SSH 网关上执行的命令配合。
- 修改代理后，若希望后续会话使用新路径，请关闭已有[复用连接](#连接复用)。

### 超时与限速

`connect_timeout` 单位为秒，默认 10，设为 0 可禁用。
它覆盖建连阶段，包括 DNS、`resolve_command` 和代理启动。

`rate_limit = "5 MiB/s"` 限制每条连接的**收发总和**。
支持 B/KB/MB/GB 和 KiB/MiB/GiB，可省略 `/s`。
defaults 中的值分别应用于每条连接，不是全局配额。

## Hooks

六类事件：`before_connect`、`on_connect`、`on_connect_error`、`on_disconnect`、
`before_config_write`、`after_config_write`。
连接事件配置在主机、模板或 defaults 中，配置写入事件放在 defaults 中。
只有生成内容发生变化时才触发写入事件；同一事件的 hooks 按顺序执行，失败会报告。

```toml
[defaults.hooks]
before_config_write = [
  { driver = "write", message = "Writing {{ ssh_config_path }}" },
]

[hosts.database.hooks]
before_connect = [
  { driver = "write", message = "Connecting {{ host.name }} via {{ gateway }}" },
]
on_connect_error = [
  { driver = "write", message = "Route {{ gateway }} failed: {{ error }}" },
]
on_disconnect = [
  { driver = "exec", command = "printf '%s\\n' \"$SSHWEAVE_CONTEXT\" >> \"$HOME/sshweave-events.jsonl\"" },
]
```

| driver | 字段    | 行为                      |
| ------ | ------- | ------------------------- |
| write  | message | 向 stderr 输出消息        |
| exec   | command | 执行 shell 命令并等待完成 |
| daemon | command | 在后台执行命令            |
| notify | message | 发送桌面通知              |

模板使用 Jinja 风格语法，未定义变量会报错。
可用变量随事件而定，包括 `host`、`gateway`、`stats` 和 `error`。
stats 包含 `sent_bytes`、`received_bytes`、`duration_seconds`、
`average_bytes_per_second` 和 `connected_at`/`disconnected_at`（Unix 秒）。
桌面通知需要 macOS 的 `osascript` 或 Linux 的 `notify-send`。
`on_connect_error` hook 失败只记录日志，不会中止后续路线的尝试。

`on_connect` 表示传输已建立，不表示 SSH 认证成功。该事件的 notify 标题是 `sshweave: transport established`。
`on_disconnect` 在传输连接关闭时触发，而不是在每个复用的 SSH 会话结束时触发。
OpenSSH 客户端退出时会用 SIGHUP 结束 ProxyCommand，此时同样触发；
SIGTERM 和 SIGINT 也会触发该 hook，并带有 `error` 值。

exec/daemon 还会获得 JSON 环境变量 `SSHWEAVE_CONTEXT`。若在 shell 模板中插入
文本，请使用 `{{ host.name | shellquote }}`，或从 JSON 环境变量读取。
配置本身是**可信的可执行输入**，不要加载不可信的 TOML/hooks。

## 命令

| 用途                                | 命令                                              |
| ----------------------------------- | ------------------------------------------------- |
| 验证指定配置                        | `sshweave -c ./sshweave.toml config check`        |
| 输出生成的 OpenSSH 配置             | `sshweave config build`                           |
| 更新受管理的配置文件                | `sshweave config build --write`                   |
| 生成不自动更新的配置                | `sshweave config build --no-automatic-rewrite`    |
| 列出主机、别名和模式                | `sshweave config list`                            |
| 以 JSON 列出解析后的主机            | `sshweave config list --json`                     |
| 搜索主机                            | `sshweave config search production`               |
| 查看主机或链式目标                  | `sshweave config resolve database/bastion`        |
| 查看每个字段来自哪一层配置          | `sshweave config explain database`                |
| 导出合并后的配置                    | `sshweave config json`                            |
| 导出解析后的具体主机                | `sshweave config json --expand`                   |
| 以 JSON 预览路由，不连接            | `sshweave connect --dry-run database`             |
| 将关系导出为 Graphviz DOT           | `sshweave config graphviz`                        |
| 探测所选路线第一跳的 TCP 延迟       | `sshweave ping --count 4 --interval 0.5 database` |
| 检查 Include 并打印 Git 的 SSH 命令 | `sshweave doctor`                                 |
| 查看版本、路径及已加载文件          | `sshweave info`                                   |
| 清理不再需要的缓存目标              | `sshweave cache prune --dry-run`                  |
| 生成 shell 补全                     | `sshweave completions zsh`                        |

全局选项 `--config`/`-c`、`--verbose`/`-v`、`--debug`。
支持 `SSHWEAVE_CONFIG`、`SSHWEAVE_DEBUG` 和标准 `RUST_LOG`。
通过 `sshweave <command> --help` 查看选项；wrapper 例外，
需使用 `sshweave help wrapper`。

`ping` 对所选路线的第一跳做 TCP 握手计时：代理，或最外层跳板。
它不启动 `proxy_command`，不认证，也不继续拨后面的跳。`random` 按配置顺序探测。
`-p` 只在探测目标本身时覆盖端口。默认探测 4 次，
`--count 0` 持续运行至 Ctrl+C；没有成功探测时返回失败状态。
`doctor` 打印 Include 行和 `GIT_SSH_COMMAND`；Include 写在 `Host` 或 `Match` 之后会报错。
未设置 `settings.binary` 时，生成的 ProxyCommand 使用最近一次写入它的 sshweave
可执行文件路径；`doctor` 会报告不一致。从多个位置运行 sshweave 会导致它们轮流
重写该文件，因此请将 `settings.binary` 设为一个固定路径。
`config build --write` 在覆盖已托管文件之前，列出新增和删除的 `Host` 行。
使用 `sshweave config graphviz | dot -Tsvg > hosts.svg` 导出 SVG，
需另外安装 Graphviz 的 `dot` 命令。

### 连接复用

要复用连接并通过 `sockets` 管理它们，需配置 `ControlPath`：

```toml
[defaults]
control_master_mkdir = true

[defaults.ssh]
ControlMaster = "auto"
ControlPersist = "5m"
ControlPath = "~/.config/sshweave/control/%C"
```

`control_master_mkdir` 会按需创建控制套接字目录。
[示例配置](sshweave.toml) 已启用这组设置。

```sh
sshweave sockets list
sshweave sockets master database
sshweave sockets flush
```

`master` 建立后台 SSH 连接，可能需要认证。
`flush` 关闭受管理的控制连接，可能中断正在使用它们的会话。
已有复用连接可能在配置变更后继续使用旧路径，请在准备好重新连接时关闭它们。

### 管理已实例化主机缓存

动态主机和链式目标使用过的名称会记录在 `settings.registry` 中（仍接受旧键名 `known_hosts`）。
只记录需要独立生成配置段的名称：显式链、仅通过模式匹配的名称及其经过的网关。
这是目标名称缓存，**不是 OpenSSH 的主机密钥数据库**。
使用 `cache list` 返回的名称替换下面的 `old-worker`：

```sh
sshweave cache list
sshweave cache remove old-worker --dry-run
sshweave cache remove old-worker
sshweave config build --write
```

删除按完整名称精确匹配，不支持模式；未知名称会报错。
`--dry-run` 仅预览，不写入文件或运行 hooks。
清空全部缓存时，先运行 `sshweave cache clear --dry-run` 预览，
再执行 `sshweave cache clear` 并重新生成配置。脚本可使用 `cache list --json`。

删除只修改缓存。显式 build 或使用 wrapper 后，生成配置才会刷新；
静态主机及其他路由仍需使用的网关会保留。再次连接可能重新加入已删除的名称，
已有名称不会自动淘汰；`sshweave cache prune` 可删除不再需要独立配置段的名称，
例如对应模式已删除的名称，或旧版本缓存的普通名称，可先用 `--dry-run` 预览。OpenSSH 主机密钥、源 TOML、私钥和控制套接字均不会修改。

生成配置最多支持 4096 个不同目标，包含静态名称、别名、缓存目标及网关链组件。
若旧缓存导致达到上限，可删除不再使用的名称。
