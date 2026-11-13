# 更新日志

本文件记录 clipkeep-gateway 的版本变更。**发布时 CI 会截取对应版本段落作为 Release 说明**，
所以每次打 tag 前必须先在下面补好该版本的内容，否则构建直接失败（避免发出空说明的 Release）。

格式：`## [vX.Y.Z] - YYYY-MM-DD`，版本从新到旧排列。

## [Unreleased]

## [v0.1.0] - 2026-11-13

### 新增

- DTK `/api/*` 透传：服务端注入 `X-API-Key`，`wait=20` 超时补两次轮询，`{success, data}` envelope 原样返回
- 会话：`SITE_TOKEN` 换 HMAC 签名 cookie，服务端无状态，口令不进 URL
- `/dl` 下载：默认 302 跳 CDN（零带宽、浏览器自带续传），`?p=1` 改流式中转并透传 Range
- 静态托管 Flutter web 产物，未命中回退 `index.html`
- 解析接口按 IP 限流，`/dl` 只放行白名单域名，避免变成开放代理
- CNB 流水线：`rust:1-slim` + `musl-tools` 编 `x86_64-unknown-linux-musl`，
  fmt / clippy / test / build 全绿
- GitHub Actions 流水线：`cross` 并行编 `x86_64` 与 `aarch64` 两个 musl target
- Release 附件：`clipkeep-gateway-{x86_64,aarch64}-linux` + `SHA256SUMS`，并标记 `latest`
- Release 说明从本文件自动截取，缺对应版本段落时 CI 失败

### 说明

- 两个架构都是 musl 静态链接，目标机不需要装 Rust 或 musl，下载后 `chmod +x` 即可运行
- 单文件二进制约 3.8 MB，`ldd` 报 `not a dynamic executable`
- GitHub 与 CNB 双平台构建，产物命名与内容一致，可互为镜像
