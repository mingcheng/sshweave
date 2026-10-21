# sshweave

[English](README.md) | **简体中文**

**少一些 SSH 配置负担，多一份连接从容。**

sshweave 帮助你在基础设施不断扩展时，有序管理 SSH 主机与连接路径。集中维护共用设置，轻松访问私有网络中的主机。

## 为什么选择 sshweave？

- **减少重复维护：** 在主机和环境之间复用共用设置，不必反复复制配置。
- **简化主机访问：** 用易记的名称访问主机，统一管理成组的机器，包括动态命名的主机。
- **灵活选择路径：** 支持直连、跳板机和网络代理，连接失败时可尝试备用路径。
- **连接路径更清晰：** 在连接之前预览到达目标主机的路线。
- **保留熟悉的工作流：** 继续使用 SSH、SCP、SFTP 和 rsync，由 OpenSSH 处理认证与主机验证。
- **配置始终可控：** 管理生成的配置，不自动覆盖你手写的 SSH 配置。

## 适合谁？

需要管理多个环境、私有网络主机或跳板机，希望减少重复配置、统一 SSH 使用体验的开发者与运维人员。

## 开始使用

[`sshweave` crate](https://crates.io/crates/sshweave) 提供名为 `sshweave` 的二进制命令行程序。通过 Cargo 安装
（需要 Rust 1.88+）：

```sh
cargo install sshweave --locked
sshweave --help
```

运行环境要求：macOS 或 Linux，并已安装 OpenSSH。

安装、配置示例和日常用法请参阅 **[配置与使用指南](examples/README.zh-CN.md)**。也可阅读 [English guide](examples/README.md)。

## 致谢与许可

作者：mingcheng <mingcheng@apache.org>。

设计参考 [moul/assh](https://github.com/moul/assh)，采用 [MIT 许可证](LICENSE)。
