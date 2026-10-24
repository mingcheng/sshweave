<h1 align="center">
<img src="./sshweave.webp" width="250" alt="sshweave 标志" />
<br/>
sshweave
</h1>
<p align="center">少一些 SSH 配置负担，多一份连接从容。</p>
<p align="center">
<a href="README.md">English</a> | 简体中文
</p>

sshweave 帮助你在基础设施不断扩展时，有序管理 SSH 主机与连接路径。
集中维护共用设置，轻松访问私有网络中的主机，并继续使用熟悉的工具。

sshweave 将 TOML 配置转换为 OpenSSH 配置，并提供
`ProxyCommand` 传输。它不重新实现 SSH 协议：认证、主机密钥验证及会话
仍由 OpenSSH 处理。因此，远程主机无需安装 sshweave 代理程序或守护进程。

## 为什么选择 sshweave？

- **减少重复维护：** 在主机之间共享默认值、模板和继承设置，通过 includes
  将配置拆分为可复用的 TOML 文件。
- **简化主机访问：** 使用别名、glob 模式和正则表达式，统一管理固定主机与动态命名的机器。
- **灵活选择路径：** 组合直连、SSH 跳板机及 HTTP CONNECT/SOCKS5 代理，
  支持备用路径或固定的多跳链路。
- **连接路径更清晰：** 查看解析后的设置，在终端中预览完整连接路径，无需实际连接。
- **保留熟悉的工作流：** 通过 wrapper 或 OpenSSH `Include` 继续使用 SSH、SCP、
  SFTP 和 rsync。
- **运维工具：** 配置生命周期 hooks、限制单条连接的流量速率，并管理 OpenSSH 多路复用套接字。
- **配置始终可控：** 管理生成的配置，不自动覆盖你手写的 SSH 配置。

## 适合谁？

需要管理多个环境、私有网络主机或跳板机，希望减少重复配置、统一 SSH 使用体验的开发者与运维人员。

## 开始使用

### 安装

运行环境要求：macOS 或 Linux，并已安装 OpenSSH。使用 rsync wrapper 时，
还需要单独安装 `rsync`。

sshweave 使用 Rust 编写，并以 Rust crate 形式发布。
该 [crate](https://crates.io/crates/sshweave) 提供名为 `sshweave` 的命令行程序。
通过 Cargo 安装：

```sh
cargo install sshweave --locked
sshweave --help
```

标签发布流程也会构建 macOS/Linux 的 x86_64、ARM64 压缩包，包含 SHA-256 校验和、
文档及 shell 补全。已发布制品见 [GitHub Releases](https://github.com/mingcheng/sshweave/releases)，
平台要求及校验方法见 [发布包安装说明](examples/README_CN.md#发布包安装)。
运行发布包中的二进制程序无需安装 Rust。

安装、配置示例和日常用法请参阅 **[配置与使用指南](examples/README_CN.md)**。也可阅读 [English guide](examples/README.md)。

## 致谢与许可

本项目的灵感来自 [moul/assh](https://github.com/moul/assh)，感谢原作者的启发。

本项目由 mingcheng <mingcheng@apache.org> 维护，并采用 [MIT 许可证](LICENSE)发布。

欢迎通过项目的 GitHub 仓库参与贡献或报告问题。
