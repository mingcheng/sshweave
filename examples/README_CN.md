# sshweave 配置与使用指南

[English](README.md) | **简体中文** · [产品介绍](../README_CN.md)

本指南集中介绍安装、配置、连接路由、命令和维护。
sshweave 负责配置编译、路由选择和字节流传输；系统 OpenSSH 负责认证、
主机密钥验证及会话，sshweave 不重新实现 SSH 协议。

## 目录

- [目录](#目录)
- [构建与开始使用](#构建与开始使用)
- [TOML 配置](#toml-配置)
  - [解析、继承和匹配规则](#解析继承和匹配规则)
  - [SSH 选项与运行时命令](#ssh-选项与运行时命令)
  - [网关与故障切换](#网关与故障切换)
  - [终端路由拓扑图（不连接）](#终端路由拓扑图不连接)
  - [独立 HTTP / SOCKS5 代理](#独立-http--socks5-代理)
  - [传输失败与限速](#传输失败与限速)
- [Hooks](#hooks)
- [命令](#命令)
- [分层与维护](#分层与维护)
  - [提交与推送](#提交与推送)

## 构建与开始使用

要求 Rust 1.88+、Unix 系统（macOS/Linux）和 OpenSSH。桌面通知还需要
macOS 的 `osascript` 或 Linux 的 `notify-send`。普通连接不依赖 nc/socat。
以下构建、安装、复制示例和维护命令均从仓库根目录执行，而不是 examples 目录。

```sh
cargo build --release --locked
cargo install --path . --locked
mkdir -p ~/.config/sshweave
cp examples/sshweave.toml ~/.config/sshweave/sshweave.toml
# 先按自己的环境修改示例中的域名、用户名和密钥路径。
sshweave config check
sshweave config build --write
sshweave wrapper ssh database
```

推荐使用 `sshweave wrapper ssh`：在 OpenSSH 读取配置**之前**完成模式匹配、网关链实例化、
控制套接字目录创建和配置更新，并透传 SSH 参数、远程命令及退出码。

```sh
sshweave wrapper ssh -p 2222 -v database uname -a
sshweave wrapper ssh worker-eu-42
sshweave wrapper ssh database/bastion/outer-gateway
sshweave wrapper scp file.txt database:/tmp/
sshweave wrapper sftp database
sshweave wrapper rsync -az source/ database:/srv/
```

在 Bash/Zsh 中可以按需设置以下 alias，继续使用熟悉的命令：

```sh
# 放入 ~/.bashrc 或 ~/.zshrc；macOS Bash 登录 shell 可在 ~/.bash_profile 中加载。
alias ssh='sshweave wrapper ssh'
alias scp='sshweave wrapper scp'
alias sftp='sshweave wrapper sftp'
alias rsync='sshweave wrapper rsync'

ssh -p 2222 -v database uname -a
ssh database/bastion/outer-gateway
scp -P 2222 file.txt database:/tmp/
sftp -P 2222 database
rsync -az source/ database:/srv/

# 临时绕过对应 alias，调用原生命令。
command ssh database
command scp file.txt database:/tmp/
command sftp database
command rsync -az source/ database:/srv/
```

自定义配置可设置 `export SSHWEAVE_CONFIG="$HOME/.config/sshweave/sshweave.toml"`，
上述所有 alias 都会使用该配置；
或使用 `alias ssh='sshweave -c /absolute/path/sshweave.toml wrapper ssh'`。
其他 alias 同样可以把 `-c /absolute/path/sshweave.toml` 放在 wrapper 之前。
程序名后的参数均透传给对应程序；例如 SSH 端口使用 `ssh -p`，
SCP/SFTP 端口使用 `scp -P` / `sftp -P`。`wrapper ssh` 后的 `-v`、`-c`、`-V`、`-h`
也均属于 OpenSSH，而不是 sshweave。
sshweave 自身的参数放在 wrapper 之前，入口帮助使用 `sshweave help wrapper`。
底层直接启动 `settings.ssh_binary`（SSH 默认从 PATH 查找 `ssh`）或 PATH 中的
`scp` / `sftp` / `rsync`，不经过 shell，因此不会再次展开 alias。
alias 仅影响当前 shell，不会改变 Git/GUI 等程序的 SSH 调用；
这些场景仍可使用下面的 Include 配置。

也可以在**自己维护的** `~/.ssh/config` 顶部加入：

```sshconfig
Include ~/.config/sshweave/sshweave.conf
```

随后直接使用 `ssh`、`scp`、`sftp`、`rsync` 或基于 OpenSSH 的 Git/GUI。
OpenSSH 使用第一个生效的值，Include 的位置会影响优先级。
生成文件最后的 `Host *` 为任意目的地启用 sshweave ProxyCommand；
需要特殊代理的手写配置应放在 Include 之前，或使用专用 `ssh -F` 配置。

**自动更新的边界：** ProxyCommand 运行时，外层 OpenSSH 已读取配置。
因此，配置发生变化或第一次使用新的 glob/regex 主机、`host/gateway` 链时，
sshweave 会原子更新文件并明确报错，要求重新运行 SSH，不会继续使用旧的认证参数。
wrapper 避免了这一次重试。`--no-rewrite` / `automatic_rewrite = false` 禁用
ProxyCommand 自动更新；显式 build 和 wrapper 仍会按要求生成最新配置。

sshweave 只覆盖带有自身标记的生成文件，**不会自动改写手写的
`~/.ssh/config`**。`config build` 默认输出到 stdout；推荐 `--write`，
不要将 stdout 重定向到正在读取或管理的同一个文件。

## TOML 配置

完整可用的配置结构见 [sshweave.toml](sshweave.toml)。

默认配置文件为 `$HOME/.config/sshweave/sshweave.toml`，生成的 OpenSSH 配置、
已实例化主机缓存、锁文件和原子写入的临时文件均位于 `$HOME/.config/sshweave/`。
示例中的控制套接字也存放在该目录的 `control/` 子目录。
`-c` / `SSHWEAVE_CONFIG` 可覆盖配置文件位置，`settings.output`、
`settings.known_hosts` 和 SSH `ControlPath` 仍可显式指定其他路径；
相对配置路径以 TOML 文件所在目录为基准。旧路径不会自动迁移，已有配置需手动移动，
并同步修改显式路径及 `~/.ssh/config` 中的 Include。
SSH 私钥和 OpenSSH 自身的 `known_hosts` 仍使用原有位置，不会迁移。

无需声明 includes，程序会自动加载 `${HOME}/.config/sshweave/conf.d/*.toml`，
并将该路径加入根配置的有效 includes 列表（`config json` 可查看）。
即使使用 `-c` / `SSHWEAVE_CONFIG`，仍会加载此固定目录；目录不存在或无匹配文件时允许为空。

```toml
# 可选：默认 conf.d 之外的其他配置
includes = ["extra.d/*.toml", "${HOME}/.config/sshweave/shared.toml"]

[settings]
output = "~/.config/sshweave/sshweave.conf"
known_hosts = "~/.config/sshweave/sshweave_known_hosts.json"
ssh_binary = "ssh"
automatic_rewrite = true
# binary = "/absolute/path/to/sshweave"  # 默认使用当前可执行文件的绝对路径

[defaults]
user = "${USER}"
port = 22
connect_timeout = 10

[templates.production]
user = "deploy"

[templates.production.ssh]
IdentityFile = ["~/.ssh/id_ed25519", "~/.ssh/company_key"]
IdentitiesOnly = true

[hosts.gateway]
hostname = "gateway.example.net"
aliases = ["gate"]

[hosts.database]
inherits = ["production"]
hostname = "10.0.0.10"
gateways = ["direct", "gateway"]
gateway_strategy = "sequential"
rate_limit = "5 MiB/s"

[hosts.database.ssh]
LocalForward = ["8080 localhost:80", "9090 localhost:90"]
```

### 解析、继承和匹配规则

- includes 相对包含它的文件解析，支持 glob、`~` 和 `${VAR}`，匹配结果排序；
  文件按规范化路径去重。默认 conf.d 允许为空，其他未匹配的 include、
  缺失文件、循环和无效 TOML 都会报错。
- 同名条目按字段合并：根文件 > 后面的 include > 前面的 include。
  默认 conf.d 自动放在 includes 开头，显式 includes 可覆盖它；若已显式声明该默认路径，
  则保留其声明位置。conf.d 内按文件名排序，后面的文件优先。
  settings 仅取根文件；其他文件用于 defaults/templates/hosts/proxies/includes。
- 继承允许任意深度（防护上限 64）：主机自身 > 第一个 parent > 后续 parent。
  先查 template，再查 host；可用 `template:name` / `host:name` 明确指定。
  循环及不存在的 parent 是错误，未使用的模板也会验证。
- 主机解析：精确主机名 > 精确 alias > 按声明顺序匹配的 glob/regex > defaults。
  后续匹配规则仅补充尚未设置的字段。模板不是可直接连接的主机条目。
  未声明的普通主机名仍可直连，并应用 defaults。
- glob 使用 `*`、`?` 和字符集合；正则使用显式 `regex:` 前缀，要求整个名字匹配。
  regex 的 hostname 可以使用 `{1}`、`{2}` 和 `{named_capture}`。
- `aliases` 是主机的入口名称，不从父级继承。省略 hostname 的字面 alias 指向
  主机的规范名称；模式规则省略 hostname 时使用输入名称。
- 列表整体覆盖，不自动追加；例如子级的 `ssh.IdentityFile` 替换父级列表。
  `gateways = []` 可关闭继承的网关；hook 的某个事件设为 `[]` 可关闭该事件。
  SSH 选项名不区分大小写，同一条目中的大小写重复会报错。
- 配置不运行命令也能验证/导出/解析。`${VAR}` 在路径、hostname、user、SSH
  选项和运行时命令参数中展开；变量缺失报错。不会把 `$VAR` 当成环境变量，
  以免干扰 shell 命令。`~otheruser` 不受支持。

```toml
[hosts."*.dev"]
hostname = "%n.example.net"

[hosts."regex:^db-(?P<region>[a-z]+)-([0-9]+)$"]
hostname = "{region}-{2}.internal"
user = "dbadmin"
```

### SSH 选项与运行时命令

常规 OpenSSH 选项放入 `[hosts.NAME.ssh]`、`[templates.NAME.ssh]` 或
`[defaults.ssh]`。支持字符串、整数、布尔值（转换为 yes/no）和字符串数组
（每项输出一条指令）。例如算法列表应写为一个逗号分隔的字符串；
`IdentityFile` / `LocalForward` 等可重复选项使用数组。

字符串是 OpenSSH 的**参数表达式**，包含空格的路径要按 OpenSSH 语法加引号：
`IdentityFile = '"~/.ssh/company key"'`。不允许换行/空字符注入新指令。
sshweave 不维护易过时的 SSH 选项白名单；未知选项由本机 OpenSSH 检查。
`Host`、`Match`、`Include` 不能放在 ssh 表中；Hostname/User/Port 使用强类型字段，
ProxyCommand/ProxyJump 使用下面的运行时字段和 gateways，ConnectTimeout 使用
`connect_timeout` 字段（同时传递给 OpenSSH 和代理运行时）。

```toml
[hosts.dynamic]
resolve_command = ["my-inventory", "address", "%n"]
connect_timeout = 10
proxy_command = ["nc", "%h", "%p"]
control_master_mkdir = true

[hosts.dynamic.ssh]
ControlMaster = "auto"
ControlPath = "~/.config/sshweave/control/%C"
```

- `resolve_command` 为 argv 数组，不经 shell，stdout 必须只包含一个 hostname/IP；
  非零退出、无效 UTF-8、空结果、多主机结果和超时均报错。
- `proxy_command` 同样是 argv 数组。直连时在本机执行；通过网关时作为网关上的
  远程命令执行（参数进行 shell 引用），可选择 nc、socat 或自定义传输。
  未配置时优先使用原生 TCP / `ssh -W`，不需要隐式尝试额外工具。
- 命令参数支持 `%h`（解析后 hostname）、`%n`（原始目标）、`%p`、`%r`、
  `%g`（当前网关）、`%u`、`%i`（本地 UID）、`%d`、`%l`、`%L`、`%C` 和 `%%`。
  ControlPath 支持相同的本地主机 token。常用 `%C` 使用 OpenSSH 的 SHA-1 组合。
  由 OpenSSH 渲染的常规 ssh 选项仍使用 OpenSSH 自身的 token 语义。
- `connect_timeout` 单位为秒，默认 10；0 禁用超时。覆盖 TCP 建连（包括 DNS）、
  resolve_command 和代理启动阶段。代理进程必须读到目的端的首字节才算建立成功，
  适用于会先发送 banner 的 SSH 服务；该字节不会丢失。代理关闭 stdout 却未退出时，
  等待进程结束也受启动超时约束，不会无限阻塞后续候选路由。
- 如果 `control_master_mkdir = true`，在需要时以 0700 创建 ControlPath 父目录。
- 目标名称允许通常的 DNS/IP/alias 字符，不接受 shell 表达式、空白或选项注入；
  用户名请使用 `user` 字段或 OpenSSH 的 `user@host`，不要写进主机键名。

### 网关与故障切换

`gateway_strategy` 可在 host、template 和 defaults 中配置，使用相同的继承、
include 合并和模式匹配规则；与 `gateways` 分别继承。
**未配置策略时默认 `random`**，不再保证按列表顺序连接。
依赖原有顺序的配置（例如优先直连）应显式设置 `gateway_strategy = "sequential"`。

| 策略             | `gateways` 的含义                                         | 失败行为                   |
| ---------------- | --------------------------------------------------------- | -------------------------- |
| `random`（默认） | 每次连接随机打乱全部候选路由，每个列表项尝试一次          | 依次尝试剩余候选           |
| `sequential`     | 按列表顺序尝试候选路由                                    | 依次尝试后续候选           |
| `chain`          | 按列表顺序经过所有跳板，本机 → 第一项 → 第二项 → … → 目标 | 整条链失败，不绕过任何跳板 |

```toml
[hosts.balanced]
hostname = "10.0.0.10"
gateways = ["gateway-a", "gateway-b"]  # 默认 random

[hosts.fallback]
hostname = "10.0.0.10"
gateway_strategy = "sequential"
gateways = ["direct", "gateway-a", "gateway-b"]

[hosts.private]
hostname = "10.0.0.20"
gateway_strategy = "chain"
gateways = ["outer", "inner"]  # 本机 → outer → inner → private
```

空列表（包括 `gateways = []`）在所有策略下都表示没有 SSH 跳板，可关闭继承的网关；
如另有 `proxy` 配置，仍会通过代理连接。
`direct` 是 random/sequential 的直连候选；chain 的每一项必须是单个主机名或 alias，
不能使用 `direct` 或含 `/` 的链。网关自身仍可配置自己的策略和 gateways。
random/sequential 的单个候选可以是 `inner/outer` 形式的完整链。

临时链 `destination/inner/outer` 表示本机先到 outer，再到 inner，最后到 destination；
它覆盖 destination 原有 gateways **和策略**，不对显式链顺序进行随机化。
注意 `/` 语法从目标向外书写，与 chain 列表的实际经过顺序相反。

配置编译会展开静态链及所需后缀，确保各跳板使用对应主机的认证配置；
动态 glob/regex 链由 wrapper 或自动更新机制实例化。
`config resolve`、`config list --json` 输出解析后的 `gateway_strategy`；
`connect --dry-run` 的 `routes` 展示确定性的候选列表或规范化链（如 `inner/outer`），
不执行随机打乱、命令或网络操作。随机顺序只在实际连接时生成，不触发配置重写。
Graphviz 网关边标注生效策略，chain 边指向规范化链。

### 终端路由拓扑图（不连接）

```sh
sshweave topology database
sshweave topology database/inner/outer
sshweave topology worker-eu-42 --ascii
sshweave -c ./sshweave.toml topology database > route.txt
```

使用第三方终端树形库 `ptree` 绘制从本机到指定目标的完整候选路径，不需要
Graphviz 或全屏交互。支持 alias、glob/regex、继承、临时链和嵌套网关；
每个 SSH 节点显示名称及 `user@hostname:port`，HTTP/SOCKS5 代理显示在第一跳之前。
例如 chain 配置 `gateways = ["outer", "inner"]` 的图形结构如下：

```text
SSH route topology: database [preview only]
└── Local (this machine)
    └── Route 1 | database: chain #1 via inner/outer
        └── outer  [SSH ops@gateway.example.net:22]
            └── inner  [SSH ops@10.0.0.2:22]
                └── database  [SSH ops@10.0.0.10:22]
```

多个候选分别显示为 Route 分支，而不是串成一条链。`sequential` 按配置顺序回退；
`random` 为便于比较固定按配置顺序展示，**不表示实际连接时的尝试顺序**；
`chain` 必须依次经过所有跳板。嵌套候选会展开为完整路径，
循环、深度超限或超过 4096 个展开节点时明确报错，不静默截断。

此命令仅进行静态配置解析，**不建立 SSH/TCP 连接、不探测可用性、不执行 hooks、
`resolve_command` 或 `proxy_command`，也不写入生成配置、主机缓存或控制套接字目录**。
动态解析的节点标明地址尚未由命令确认；自定义传输仅标明命令在本地或跳板执行的位置，
无法推断该命令内部的网络路径。图形描述 sshweave 配置路由，不是 IP traceroute，
也不合并外部手写 OpenSSH 配置或已有复用会话。
终端自动着色，重定向无 ANSI 颜色；支持 `NO_COLOR`、`TERM=dumb` 和 `--ascii`。

### 独立 HTTP / SOCKS5 代理

代理在顶层 `proxies` 中集中定义，主机通过 `proxy` 引用名称。
**不要将代理 URL 或 `proxy+...` 放入 `gateways`**；`gateways` 仍只接受
`direct` 和 SSH 主机/链。`direct` 表示没有 SSH 跳板，并不取消已配置的代理。

```toml
[proxies.local-http]
url = "http://127.0.0.1:7890"

[proxies.local-socks]
url = "socks5://127.0.0.1:7890"

[hosts.proxied]
hostname = "server.internal"
proxy = "local-http"  # 本机 → HTTP CONNECT 代理 → server.internal

[hosts.proxied-chain]
hostname = "database.internal"
proxy = "local-socks"
gateway_strategy = "chain"
gateways = ["outer", "inner"]
# 本机 → SOCKS5 代理 → outer → inner → database.internal
```

- 支持无认证 HTTP CONNECT 和 SOCKS5 TCP CONNECT；不需要外部 nc/curl 进程。
  URL 必须显式指定端口（1–65535），支持 DNS、IPv4、`[IPv6]`；
  可带一个结尾 `/`，不接受路径、查询、fragment、用户名密码或其他协议。
  不支持认证的代理会明确报错，不会静默降级。
- `proxy` 与其他主机字段一样支持 defaults、template、include、alias 和模式继承。
  同名代理定义按根配置 > 后面的 include > 前面的 include 覆盖；
  缺失的引用、无效 URL、未知字段在 `config check` 时拒绝。
- 主机选定的代理只用于本地到目标或最外层 SSH 跳板的连接。
  对嵌套网关会传递同一个代理选择，不被中间跳板自身的 `proxy` 覆盖，
  也不会要求在远端运行代理客户端。
- 代理失败时连接明确失败，**不会绕过代理直连**。
  网关策略仍可尝试其他候选，但每个候选都继续使用已选代理。
  `connect_timeout` 覆盖代理 DNS/TCP 建连、握手和目的端首字节，0 禁用超时。
  HTTP/SOCKS5 握手数据不会进入 SSH 字节流。
- HTTP CONNECT 和 SOCKS5 的目标域名交给代理解析，不在本地解析目标域名。
  目标支持 ASCII hostname、IPv4 和 IPv6。
- 同一段传输不能同时使用 `proxy` 和 `proxy_command`；这种配置会明确报错。
  经 SSH 跳板执行的远程 `proxy_command` 仍可配合本地第一跳代理。
- 代理配置不会作为 SSH 主机写入生成配置或主机缓存；`config json` 导出定义，
  `config resolve` / `connect --dry-run` 的 `host.options.proxy` 显示引用名称，
  Graphviz 将代理显示为独立节点。
- 使用代理时，内部跳板 SSH 禁用控制连接复用，避免复用旧的直连跳板连接。
  外层 OpenSSH 已建立的目标会话仍遵循其复用配置；切换代理后如需全新链路，
  应先关闭相应已有控制连接。

### 传输失败与限速

失败发生在建连或目的端首字节到来前时才会切换候选路由。
一旦开始转发会话，任何错误都会返回，不会重新连接并混入另一条 SSH 字节流。
静态网关循环在加载时拒绝，动态链还有跨进程访问栈与深度防护。

`rate_limit = "5 MiB/s"` 限制一次代理连接的**收发总和**，不是各方向各 5 MiB。
支持 B/KB/MB/GB 和 KiB/MiB/GiB，可省略 `/s`。这不是跨进程/跨会话的全局配额；
defaults 中的值是每个会话的默认值。

## Hooks

六类事件：`before_connect`、`on_connect`、`on_connect_error`、`on_disconnect`、
`before_config_write`、`after_config_write`。
前四类定义在 host/template/defaults；配置写入事件读取解析继承后的 defaults。
同一事件顺序执行，失败明确返回，不静默忽略。

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

驱动：

| driver | 字段    | 行为                                       |
| ------ | ------- | ------------------------------------------ |
| write  | message | 写 stderr，不污染 SSH stdout               |
| exec   | command | `/bin/sh -c`，等待完成，非零退出报错       |
| daemon | command | 独立进程组后台执行，失败记录到 stderr 日志 |
| notify | message | 通过系统命令发送桌面通知，失败报错         |

模板使用 MiniJinja，不是 Go template。未定义变量报错。连接上下文包含 `host`
（name/hostname/user/port/options 等）、`gateway`（建连尝试事件）、`stats`
（连接/断开事件）或 `error`（失败/断开事件）。
stats 包含 connected_at/disconnected_at（Unix 秒）、sent_bytes、received_bytes、
duration_seconds、average_bytes_per_second。on_connect 仅表示 TCP/代理传输已建立，
不表示 SSH 认证完成；on_disconnect 跟随代理流生命周期，而非单个复用会话。

exec/daemon 还会获得 JSON 环境变量 `SSHWEAVE_CONTEXT`。若在 shell 模板中插入
文本，请使用 `{{ host.name | shellquote }}`，或从 JSON 环境变量读取。
配置本身是**可信的可执行输入**，不要加载不可信的 TOML/hooks。

## 命令

```sh
sshweave -c ./sshweave.toml config check
sshweave config build                         # OpenSSH 配置到 stdout
sshweave config build --write                 # 原子更新 settings.output
sshweave config build --no-automatic-rewrite
sshweave config json                          # 合并后的配置
sshweave config json --expand                 # 解析后的具体主机
sshweave config list
sshweave config list --json
sshweave config search production
sshweave config resolve database/bastion
sshweave config graphviz | dot -Tsvg > hosts.svg
sshweave connect --dry-run database            # 只导出路由；不连接、不运行 hook/命令
sshweave topology database                    # 终端拓扑图；只打印，不连接
sshweave ping --count 4 --interval 0.5 database # TCP 延迟、成功率；不支持网关或代理
sshweave sockets list
sshweave sockets master database
sshweave sockets flush                        # ssh -O exit，不只是删除 socket 文件
sshweave info
sshweave completions zsh
```

全局选项 `--config`/`-c`、`--verbose`/`-v`、`--debug`。
支持 `SSHWEAVE_CONFIG`、`SSHWEAVE_DEBUG` 和标准 `RUST_LOG`。
wrapper 中程序名（ssh/scp/sftp/rsync）后的 `-v` 等参数透传给被包装程序，
sshweave 的全局选项放在 wrapper 前。
wrapper 管理 `-F`/rsync 远程 shell，不支持再传入冲突的配置文件或 rsync `-e`；
rsync 的 `-e` 附加参数和组合短选项也会检查，合法的 rsync `-F` 则正常透传。
更复杂的 OpenSSH/rsync 调用可使用生成文件与原生命令。
ping 默认 4 次，`--count 0` 持续运行，Ctrl+C 输出已完成探测的统计；
没有成功探测时返回失败。sockets 仅枚举真正的 Unix socket，忽略同目录普通文件；
ControlPath 中的字面 `*`、`?`、`[]` 不作为 glob 展开，只有动态主机 token 用于匹配。

## 分层与维护

```text
lib.rs       crate 总览与模块组织（可用 `cargo doc --open` 查看）
main.rs      clap CLI、应用用例、子进程退出码
config.rs    TOML 模型、include 装载、结构验证、继承
resolver.rs  纯主机解析、模式/alias/capture、网关图验证、token
render.rs    确定性 OpenSSH / Graphviz 编译
topology.rs  只读路由拓扑、完整候选路径展开、终端树形绘制
store.rs     注册主机、文件锁、0600 临时文件、fsync + 原子替换
runtime.rs   TCP/代理传输、启动检测、双向 relay、限速、连接统计
proxy.rs     独立代理 URL 校验、HTTP CONNECT / SOCKS5 无认证握手
hooks.rs     模板、事件与驱动执行（始终隔离协议 stdout）
sockets.rs   控制 socket 发现与生命周期管理
util.rs      严格环境/路径展开、输入和单位验证、私有目录创建
tests/       集成测试；tests/common 隔离 HOME，避免读取开发者本机的 conf.d
```

生成文件优先输出具体主机，通配规则排除已实例化的主机，避免 OpenSSH
累加 IdentityFile/LocalForward 等选项导致重复。注册文件不是 OpenSSH 的
`known_hosts` 主机密钥数据库，不存储私钥/凭据，仅记录已实例化的名称。
不会删除或清理用户已有的 SSH 文件。

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --all-targets --locked
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --locked
```

lint 策略位于 `Cargo.toml` 的 `[lints]`：公开项必须有文档注释，库和可执行文件禁止 `unsafe` 代码。

测试覆盖配置合并、优先级、模式、循环、无效输入、文件保护、原子更新、
并发重建、hook 错误、字节级传输、半关闭、远端提前断开、共享限速阈值、
随机/顺序网关切换、链式路由展开和覆盖、wrapper 参数/退出码，以及真实 `ssh -G` 和生成 ProxyCommand。
代理测试覆盖 HTTP/SOCKS5 握手、远端 DNS、IPv4/IPv6、二进制流与半关闭、
畸形/拒绝响应、超时、不绕过代理直连，以及代理与多级 SSH 跳板的组合。
集成测试使用本地 TCP 和受控代理进程，不需要外部 SSH 服务器或登录凭据。
CI 在 Linux/macOS 上分别使用 stable 和最低支持版本 Rust 1.88 执行上述检查。
外部跳板机认证、真实多级 SSH 会话和桌面通知仍需在实际部署环境验收。

### 提交与推送

仓库地址为 `git@github.com:mingcheng/sshweave.git`，默认分支为 `main`。
提交前运行上述检查，并使用 `git status --short` 和 `git diff --cached --check`
检查待提交内容。`Cargo.lock` 应随应用源码一起提交，以便本地与 CI 使用相同的依赖版本。

仓库使用 UTF-8、LF 换行；Rust 格式以 `cargo fmt` 为准。
构建目录（包括符号链接）、本机编辑器设置、环境文件和常见私钥文件已加入忽略规则，
但忽略规则不能替代人工检查：不要提交真实 SSH 配置、主机清单或凭据。
首次推送使用 `git push -u origin main`；推送前需先创建本地提交、
确认远程仓库已存在，并配置好 GitHub SSH 认证。

MIT 许可证。项目设计参考 assh，保留其版权归属，详见 [LICENSE](../LICENSE)。
