# 更新日志

本文件记录 clipkeep-gateway 的版本变更。**发布时 CI 会截取对应版本段落作为 Release 说明**，
所以每次打 tag 前必须先在下面补好该版本的内容，否则构建直接失败（避免发出空说明的 Release）。

格式：`## [vX.Y.Z] - YYYY-MM-DD`，版本从新到旧排列。

## [Unreleased]

## [v0.1.9] - 2026-11-14

### 修复

- `valid_token` 用 `all()` 检查整串，把 `bundle-` 前缀里的字母也当成非法字符，
  所有合法编号都被判成「打包编号不合法」，打包下载完全不可用
- 改为只校验前缀之后的部分，并补三组单测覆盖合法格式与路径穿越

## [v0.1.8] - 2026-11-14

### 变更

- `/api/bundle` 改为两段式：POST 只负责打包并返回 `{token, saved, skipped}`，
  另用 `GET /api/bundle/{token}` 取 zip。
  原先直接回传文件，前端拿不到状态码，500 被塞进隐藏 iframe，用户只看到「已打包」却没有文件
- 前端据此提示失败项数量，包内 `_skipped.txt` 写明具体原因

### 修复

- README 补上 `BUNDLE_TMP`、`ReadWritePaths` 与临时目录的创建步骤：
  `ProtectSystem=strict` 下不显式放行会报 `Read-only file system`

## [v0.1.7] - 2026-11-14

### 新增

- `POST /api/bundle`：把多个文件下载到临时目录，压成一个 zip 流式返回。
  浏览器会拦截连续触发的多个下载（用户只收到第一个），打包后只触发一次。
  - 上限：50 个文件 / 单文件 200 MB / 整包 500 MB / 整体 180 秒
  - 失败项跳过，包内附 `_skipped.txt` 写明原因
  - 同时接受 JSON 与表单提交：浏览器无法用 XHR 触发带 `Content-Disposition` 的下载
- `BUNDLE_TMP` 环境变量（默认 `./tmp`），临时目录按请求隔离，120 秒后清理，另有 10 分钟惰性兜底

### 部署注意

- systemd unit 需加 `ReadWritePaths=/opt/clipkeep-gateway/tmp`，否则 `ProtectSystem=strict` 会挡住写盘
- 该目录属主设为 `clipkeep:clipkeep`
- nginx 的 `location /api/bundle` 要 `proxy_read_timeout 180s` + `proxy_buffering off`

## [v0.1.6] - 2026-11-14

### 修复

- `prefer_play_urls` 补上 `media.images[].video`：实况图的短视频挂在这里，
  漏掉会保留 `douyinvod` 直链，下载必然 403
- 补一个单元测试覆盖实况图分支

## [v0.1.5] - 2026-11-13

### 修复

- 增加 `/api/v1/parse` 与 `/api/v1/tasks/{id}` 别名。Flutter 端的 `DtkClient` 直连 DTK 时
  打的就是 `/api/v1/*`，打到网关上会 404 并落到静态回退返回 `index.html`，
  前端因此报「解析服务返回格式异常」

## [v0.1.4] - 2026-11-13

### 修复

- 剔除 `watermark=true` 的流：典型是 720×720、码率缺失、体积反而更大的烧录水印版
- 整条内容只有水印流时保留原样，避免返回空列表

### 说明

- 水印标记来自 DTK 归一化的 `media.streams[].watermark`
- 顺序：先去水印 → 再筛 play 地址 → 最后回填帧率

## [v0.1.3] - 2026-11-13

### 新增

- 从 `raw.video.bit_rate[].FPS` 回填帧率到 `media.streams[]` 与 `media.video`
- 按（宽、高、码率）三元组匹配，同分辨率不同码率不会串档
- 上游已有 `fps` 时不覆盖；数字兼容 int / float / 字符串三种形态

### 变更

- 启动摘要改为多行中文：带版本号、静态目录状态、下载模式说明、完整域名列表、限流值
- `/healthz` 返回 `version`，`curl` 一下就能确认线上版本

### 说明

- DTK 归一化的 `streams[]` 本来不带 fps，帧率只存在于原始响应里，
  请求侧一直有带 `include_raw: true`，问题是透传时没把它回填

## [v0.1.2] - 2026-11-13

### 变更

- `/api/parse` 返回前剔除 `douyinvod.com` 直链，只保留 `www.douyin.com/aweme/v1/*` 播放接口地址
- 每条流的 `url` 与 `urls` 同步改写，前端不需要再自己挑链接
- 一条 play 地址都没有的流原样保留，避免把还能用的流删空

### 说明

- `douyinvod` 直链实测 403（签名一次性或已过期），play 接口每次返回一张新票
- 改写只发生在响应里，DTK 原始数据不动

## [v0.1.1] - 2026-11-13

### 变更

- `DELIVERY` 默认值从 `redirect` 改为 `proxy`：浏览器对顶层导航到 `video/mp4` 一律内联播放，
  redirect 模式下点下载只会开始播放，对下载工具是硬伤
- README 部署章节改为自包含：`.env` 与 systemd unit 直接内联，不再假设已经 clone 仓库

### 修复

- 部署文档缺了获取 `.env.example` 的步骤，照抄命令会在 `cp .env.example .env` 处失败

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
