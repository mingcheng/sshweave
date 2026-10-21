# sshweave

Rust 实现的可组合 SSH 配置与连接路由工具，功能设计参考
[moul/assh](https://github.com/moul/assh)。由系统 OpenSSH 负责认证、主机密钥验证及会话；
sshweave 负责配置编译、路由选择和字节流传输，不重新实现 SSH 协议。

## 构建与开始使用

要求 Rust 1.88+、Unix 系统（macOS/Linux）和 OpenSSH。桌面通知还需要
macOS 的 `osascript` 或 Linux 的 `notify-send`。普通连接不依赖 nc/socat。

```sh
cargo build --release --locked
cargo install --path . --locked
mkdir -p ~/.ssh
cp examples/sshweave.toml ~/.ssh/sshweave.toml
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

自定义配置可设置 `export SSHWEAVE_CONFIG="$HOME/.ssh/sshweave.toml"`，
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
Include ~/.ssh/sshweave.conf
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

完整可用的配置结构见 [examples/sshweave.toml](examples/sshweave.toml)。

```toml
includes = ["conf.d/*.toml", "${HOME}/.config/sshweave/shared.toml"]

[settings]
output = "~/.ssh/sshweave.conf"
known_hosts = "~/.ssh/sshweave_known_hosts.json"
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
rate_limit = "5 MiB/s"

[hosts.database.ssh]
LocalForward = ["8080 localhost:80", "9090 localhost:90"]
```

### 解析、继承和匹配规则

- includes 相对包含它的文件解析，支持 glob、`~` 和 `${VAR}`，匹配结果排序；
  文件按规范化路径去重。缺失文件、未匹配的 include、循环和无效 TOML 都会报错。
- 同名条目按字段合并：根文件 > 后面的 include > 前面的 include。
  settings 仅取根文件；其他文件仅用于 defaults/templates/hosts/includes。
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
ControlPath = "~/.ssh/controls/%n/%C"
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

`gateways = ["direct", "gateway-a", "gateway-b"]` 是**依次尝试的候选路由**，
不是必须依次经过的跳板。网关自身可以继续配置 gateways。
临时链 `destination/inner/outer` 表示本机先到 outer，再到 inner，最后到 destination；
它覆盖 destination 原有 gateways。

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
sshweave ping --count 4 --interval 0.5 database # TCP 延迟、成功率；不支持网关
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

## 与 assh 的功能对照

| assh 功能                              | sshweave                                     |
| -------------------------------------- | -------------------------------------------- |
| Host、alias、glob 模式                 | hosts/aliases；匹配顺序确定                  |
| regex/动态名称                         | 显式 regex:、捕获替换、具体主机注册文件      |
| templates、inherits、defaults          | 递归继承、强类型字段、循环检测               |
| YAML includes、环境变量                | TOML includes、相对路径、glob、严格 `${VAR}` |
| SSH 选项与多值选项                     | ssh 表，支持任意本机 OpenSSH 选项            |
| 网关链、direct 优先、备用网关          | 原生 TCP、ssh -W、嵌套网关和建连前故障切换   |
| ResolveCommand、ProxyCommand           | argv 数组，显式超时与退出错误                |
| 本地命令、hooks 四种驱动/六类事件      | MiniJinja hooks + OpenSSH LocalCommand       |
| 连接限速与统计                         | 双向共享限速、收发字节/时长/平均速率         |
| 自动配置重建、ControlPath mkdir        | 原子替换、文件锁、权限、wrapper 预构建       |
| config build/json/list/search/graphviz | 对应命令，另有 check/resolve                 |
| connect dry-run、ping、info            | 对应命令；ping 与上游一样只支持直接 TCP      |
| sockets list/master/flush              | 支持，flush 发送 OpenSSH 退出请求            |
| SSH wrapper、shell completion          | SSH/SCP/SFTP/rsync wrapper，clap completion  |

有意不移植原项目的实现细节/缺陷：父进程命令行窥探式日志等级推断、
静默跳过坏 include/继承/失败 hook、未实现的自定义 DNS nameserver 字段、
任意 SSH 连接时覆盖手写配置、Windows shell/通知后端。
升级前请注意所有配置键、输出 JSON、模板语法和部分 CLI 参数均非兼容接口。

## 分层与维护

```text
main.rs      clap CLI、应用用例、子进程退出码
config.rs    TOML 模型、include 装载、结构验证、继承
resolver.rs  纯主机解析、模式/alias/capture、网关图验证、token
render.rs    确定性 OpenSSH / Graphviz 编译
store.rs     注册主机、文件锁、0600 临时文件、fsync + 原子替换
runtime.rs   TCP/代理传输、启动检测、双向 relay、限速、连接统计
hooks.rs     模板、事件与驱动执行（始终隔离协议 stdout）
sockets.rs   控制 socket 发现与生命周期管理
util.rs      严格环境/路径展开、输入和单位验证
```

生成文件优先输出具体主机，通配规则排除已实例化的主机，避免 OpenSSH
累加 IdentityFile/LocalForward 等选项导致重复。注册文件不是 OpenSSH 的
`known_hosts` 主机密钥数据库，不存储私钥/凭据，仅记录已实例化的名称。
不会删除或清理用户已有的 SSH 文件。

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --all-targets --locked
```

测试覆盖配置合并、优先级、模式、循环、无效输入、文件保护、原子更新、
并发重建、hook 错误、字节级传输、半关闭、远端提前断开、共享限速阈值、
网关候选切换、wrapper 参数/退出码，以及真实 `ssh -G` 和生成 ProxyCommand。
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

MIT 许可证。项目设计参考 assh，保留其版权归属，详见 [LICENSE](LICENSE)。
