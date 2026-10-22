# sshweave 配置与使用指南

[English](README.md) | **简体中文** · [产品介绍](../README_CN.md)

本指南集中介绍安装、配置、连接路由、命令和维护。
sshweave 负责配置编译、路由选择和字节流传输；系统 OpenSSH 负责认证、
主机密钥验证及会话，sshweave 不重新实现 SSH 协议。

## 目录

- [目录](#目录)
- [构建与开始使用](#构建与开始使用)
  - [发布包安装](#发布包安装)
  - [日常接入](#日常接入)
- [TOML 配置](#toml-配置)
  - [解析、继承和匹配规则](#解析继承和匹配规则)
  - [SSH 选项与运行时命令](#ssh-选项与运行时命令)
  - [网关与故障切换](#网关与故障切换)
  - [终端路由拓扑图（不连接）](#终端路由拓扑图不连接)
  - [独立 HTTP / SOCKS5 代理](#独立-http--socks5-代理)
  - [传输失败与限速](#传输失败与限速)
- [Hooks](#hooks)
- [命令](#命令)
  - [管理已实例化主机缓存](#管理已实例化主机缓存)
- [分层与维护](#分层与维护)
  - [解析性能](#解析性能)
  - [发布流程](#发布流程)
  - [提交与推送](#提交与推送)

## 构建与开始使用

从源码构建要求 Rust 1.88+、Unix 系统（macOS/Linux）和 OpenSSH。桌面通知还需要
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

### 发布包安装

[GitHub Releases](https://github.com/mingcheng/sshweave/releases) 的已发布制品可包含
`sshweave-VERSION-TARGET.tar.gz` 及对应 `.sha256` 文件，目标平台如下：

| TARGET | 平台 |
| --- | --- |
| `x86_64-unknown-linux-gnu` | Linux x86_64，在 Ubuntu 24.04 构建 |
| `aarch64-unknown-linux-gnu` | Linux ARM64，在 Ubuntu 24.04 构建 |
| `x86_64-apple-darwin` | macOS Intel，部署目标版本为 13.0 |
| `aarch64-apple-darwin` | macOS Apple Silicon，部署目标版本为 13.0 |

二进制仍需要 OpenSSH，但不需要 Rust、Python 或 Go。Linux 发布包需要兼容的
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
`SHA256SUMS` 汇总四个平台压缩包的校验和。

### 日常接入

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
- 当目标有多条候选路由时，原生 TCP 的 direct 候选也会等待目的端首字节。
  首字节前 EOF 或超时会尝试下一个候选，不转发会话数据；DNS、TCP 建连和探测
  共用一个超时预算，探测字节会完整回放。只有一条原生直连路由时保留通用 TCP
  行为，包括客户端先发数据和半关闭。HTTP/SOCKS5 与命令传输仍保留原有的
  服务端首字节探测，即使只有一条路由也不变。
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
sshweave cache list --json                    # 已实例化名称，不是 SSH 主机密钥
sshweave cache remove worker-eu-42 --dry-run
sshweave cache remove worker-eu-42
sshweave cache clear --dry-run
sshweave info
sshweave completions zsh
```

全局选项 `--config`/`-c`、`--verbose`/`-v`、`--debug`。
支持 `SSHWEAVE_CONFIG`、`SSHWEAVE_DEBUG` 和标准 `RUST_LOG`。
wrapper 中程序名（ssh/scp/sftp/rsync）后的 `-v` 等参数透传给被包装程序，
sshweave 的全局选项放在 wrapper 前。
wrapper 管理 `-F`、SSH 路由和 rsync 远程 shell。冲突的配置文件、SSH/SCP/SFTP 的
`-J`、通过 `-o` 覆盖路由以及 rsync `-e`，都会在写入缓存/配置之前明确拒绝；
`-Jjump`、`-vJjump` 等附加参数和组合短选项同样检查。
请用 `gateways` 或 `target/gateway` 显式链代替 `-J`。
合法的 rsync `-F` 正常透传，SSH 远程命令中的参数保持原样。
更复杂的 OpenSSH/rsync 调用可使用生成文件与原生命令。
ping 默认 4 次，`--count 0` 持续运行，Ctrl+C 输出已完成探测的统计；
没有成功探测时返回失败。sockets 仅枚举真正的 Unix socket，忽略同目录普通文件；
ControlPath 中的字面 `*`、`?`、`[]` 不作为 glob 展开，只有动态主机 token 用于匹配。

### 管理已实例化主机缓存

生成配置的展开上限为 4096 个去重后的具体目标，合并计算静态主机、字面 alias、
已登记目标、网关和链组件；这不是模式规则数量的上限。登记新目标时，会持有
注册表锁校验完整展开集合；超限操作明确报错，不会改变原有注册表。

```sh
sshweave cache list
sshweave cache list --json
sshweave cache remove old-worker old-worker/gateway --dry-run
sshweave cache remove old-worker old-worker/gateway
sshweave config build --write
# 如需清空所有动态登记名称以恢复：
sshweave cache clear --dry-run
sshweave cache clear
sshweave config build --write
```

删除按完整名称精确匹配，不支持模式；未知名称报错，不会部分应用。
命令向 stdout 输出选中的名称，向 stderr 输出汇总。`--dry-run` 不创建文件、
目录或锁文件，不获取文件锁，也不运行 hooks。
实际删除只修改 `settings.known_hosts`；显式 build 或下一次 wrapper 才更新生成配置。
静态主机及仍被其他路由引用的网关依然会出现在生成结果中；再次连接被删除的名称
可能重新登记。不会自动淘汰已有名称。

这些命令不修改 OpenSSH 主机密钥、源 TOML、私钥或控制套接字。

## 分层与维护

```text
lib.rs       crate 总览与模块组织（可用 `cargo doc --open` 查看）
main.rs      clap CLI、应用用例、子进程退出码
wrapper.rs   仅供可执行文件使用的 SSH 兼容参数处理与进程调用
config.rs    TOML 模型、include 装载、结构验证、继承
resolver.rs  借用式预编译解析器、alias 索引、capture、网关图验证、token
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
cargo test --doc --locked
python3 -m unittest discover -s scripts -p 'test_*.py'
python3 scripts/check_docs.py
go run github.com/rhysd/actionlint/cmd/actionlint@v1.7.12 .github/workflows/ci.yml .github/workflows/release.yml
```

Python 3.11+ 只用于开发/发布脚本，工作流校验使用 Go；两者都不是应用运行时依赖。

lint 策略位于 `Cargo.toml` 的 `[lints]`：公开项必须有文档注释，库和可执行文件禁止 `unsafe` 代码。

测试覆盖配置合并、优先级、模式、循环、无效输入、文件保护、原子更新、
并发重建、hook 错误、字节级传输、半关闭、远端提前断开、共享限速阈值、
随机/顺序网关切换、链式路由展开和覆盖、wrapper 参数/退出码，以及真实 `ssh -G` 和生成 ProxyCommand。
代理测试覆盖 HTTP/SOCKS5 握手、远端 DNS、IPv4/IPv6、二进制流与半关闭、
畸形/拒绝响应、超时、不绕过代理直连，以及代理与多级 SSH 跳板的组合。
集成测试使用本地 TCP 和受控代理进程，不需要外部 SSH 服务器或登录凭据。
CI 在 Linux/macOS 上分别使用 stable 和最低支持版本 Rust 1.88 执行上述检查。
还会检查 Markdown 本地文件链接、发布脚本、工作流语法和解析性能预算。
Actions 固定到提交哈希并由 Dependabot 更新；Cargo 缓存复用未变更的依赖，
重复的分支/PR 运行会取消，但不会取消标签发布。
外部跳板机认证、真实多级 SSH 会话和桌面通知仍需在实际部署环境验收。

### 解析性能

`Config::resolver()` 借用配置，一次性编译 regex/glob、建立字面 alias 索引，
并在批量查询中复用继承结果。渲染、拓扑、列表和路由展开均使用预编译视图。
原有 `Config::resolve()` API 保留；配置仍可在两次视图之间修改，修改后应新建
解析器。没有跨进程持久缓存，因此 include 或环境变化不会被陈旧缓存掩盖。

继承和网关图的记忆化保留循环及总深度检查；通过更长前缀访问已缓存子图时，
仍会检查整条路径深度。匹配优先级、声明顺序及生成结果的确定性保持不变。

```sh
cargo build --release --locked
python3 scripts/benchmark.py
python3 scripts/benchmark.py --iterations 5 --max-regex-ms 250
```

基准使用临时配置和 HOME，预热一次后重复运行 CLI `config check`，以 JSON
输出普通主机与正则规则的样本和中位数。CI 在 Linux runner 上对 180 条正则规则
设置 250 ms 中位数预算，并保留报告。测量包含进程启动，本机结果受机器负载影响，
不是生产环境 SLA。

### 发布流程

[发布工作流](../.github/workflows/release.yml) 为上述四个平台原生构建二进制。
手动 `workflow_dispatch` 执行验证和打包，生成包含 `SHA256SUMS` 的
`verified-release` 汇总制品，但**不会发布**。
推送 `vVERSION` 标签时，会验证其与 `Cargo.toml` 版本完全一致，执行可复用 CI
和各目标平台的测试，再打包二进制、文档与补全。

发布前必须集齐四个平台压缩包及各自的 SHA-256 文件，并全部通过校验；
随后生成 `SHA256SUMS`，创建带自动说明的 GitHub Release。预发布版本会标记为
pre-release。仅发布作业拥有 `contents: write`，不会覆盖已有 release。
该流程不执行 crates.io 发布、Apple 签名或公证。

本地打包验证时，将 TARGET 替换为本机 Rust target：

```sh
cargo build --release --locked --target TARGET
python3 scripts/release.py metadata
python3 scripts/release.py package --target TARGET --binary target/TARGET/release/sshweave --output target/release-check
```

压缩包统一所有者、权限和时间戳；CI 使用提交时间戳，本地默认使用 Unix epoch 0。
相同输入可得到可复现的打包结果，但不等同于编译器输出可复现。脚本本身从不上传。

维护者应先更新版本及锁文件，完成手动发布演练，再创建并推送匹配的标签。
普通构建、测试或本地打包均不会自动执行此操作。
从 Actions 页面启动手动演练前，需先将工作流合并到仓库默认分支。

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
