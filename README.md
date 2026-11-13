# clipkeep-gateway

ClipKeep 网页版网关。把 DTK 的解析能力、Flutter web 的静态页面和浏览器原生下载，收敛到**一个静态链接的二进制**里。

## 它做什么

| 端点 | 行为 |
|---|---|
| `POST /api/session` | 用 `SITE_TOKEN` 换一张签名 cookie（`{token}`） |
| `GET /api/session` | 查看当前是否已登录 |
| `DELETE /api/session` | 清 cookie |
| `POST /api/parse` | 透传 DTK `/api/v1/parse`，注入 `X-API-Key`，`wait=20` 超时自动补轮询 |
| `GET /api/tasks/{id}` | 透传任务查询 |
| `GET /dl?u=<直链>&n=<文件名>&p=1` | 默认 302 跳 CDN；`p=1` 走服务端流式中转（透传 Range） |
| `GET /healthz` | 存活检查 |
| `GET /*` | Flutter web 静态产物，找不到回 `index.html` |

**不做**：用户体系、数据库、转码、落盘、任务队列。

透传 DTK 的 `{success, data}` envelope 是刻意的：Flutter 侧现成的 `dtk_mapper` 不用改一行。

## 环境变量

见 `.env.example`。必填三项：

```bash
DTK_API_KEY=...      # DTK 的 API Key，只在网关内部使用
SITE_TOKEN=...       # 前端访问口令，至少 8 位
DTK_BASE=http://127.0.0.1:8000
```

## 本地跑

```bash
cargo run --release
# 浏览器打开 http://127.0.0.1:8443/healthz
```

静态目录默认 `./static`，把 `flutter build web` 的产物放进去即可。

## 部署（不需要装 Rust）

Release 直接给可执行文件，按机器架构选：

| 文件 | 适用 |
|---|---|
| `clipkeep-gateway-x86_64-linux` | 常见 x86_64 服务器（阿里云 ECS 默认） |
| `clipkeep-gateway-aarch64-linux` | ARM64（部分轻量应用服务器、Ampere） |

两者都是 **musl 静态链接**，`ldd` 会报 `not a dynamic executable`，目标机不需要装任何运行时，
也不需要装 Rust 或 musl。

```bash
arch=$(uname -m)          # x86_64 或 aarch64
mkdir -p /opt/clipkeep-gateway/static
cd /opt/clipkeep-gateway

# GitHub 或 CNB 任选一个源
curl -L -o clipkeep-gateway \
  https://github.com/42419/clipkeep-web-gateway/releases/download/v0.1.0/clipkeep-gateway-${arch}-linux
# curl -L -o clipkeep-gateway \
#   https://cnb.cool/yunfeiii/clipkeep-web-gateway/-/releases/download/v0.1.0/clipkeep-gateway-${arch}-linux

chmod +x clipkeep-gateway

cp .env.example .env    # 然后改里面的值
chmod 600 .env

useradd -r -s /usr/sbin/nologin clipkeep
chown -R clipkeep:clipkeep /opt/clipkeep-gateway
cp deploy/clipkeep-gateway.service /etc/systemd/system/
systemctl daemon-reload
systemctl enable --now clipkeep-gateway
```

验证：

```bash
curl -s http://127.0.0.1:8443/healthz
ldd /opt/clipkeep-gateway/clipkeep-gateway   # 应输出 "not a dynamic executable"
```

## 两种部署拓扑

```
# A. 直接对外（HTTP，最省事）
浏览器 → http://<IP>:8443 → clipkeep-gateway

# B. 放在 nginx 后面（HTTPS）
浏览器 → https://<IP>:8443 (nginx, TLS) → 127.0.0.1:8080 → clipkeep-gateway
```

方案 B 记得设 `BIND=127.0.0.1:8080`、`TRUST_PROXY=1`、`COOKIE_SECURE=1`。

## 下载为什么默认 302

- 浏览器自己管进度、断点续传，服务端零流量
- dio 在 web 上会把整个响应读进内存再转 Blob，几百 MB 的视频会 OOM；302 绕开了这条路径
- 抖音 CDN 对浏览器放行（实测 403 响应带 `Access-Control-Allow-Origin: *`）；万一某些直链不行，加 `?p=1` 走中转即可

## 安全边界

- `X-API-Key` 永不下发前端，只存在于网关进程内存
- `/dl` 只放行 `ALLOWED_HOSTS` 里的域名，防止变成开放代理
- 解析接口按 IP 限流（`PARSE_RATE_PER_MIN`）
- 会话 cookie 是 HMAC 签名 + 过期时间，无需服务端状态

## 发版流程

1. 在 `CHANGELOG.md` 补 `## [vX.Y.Z] - YYYY-MM-DD` 段落（**缺了 CI 会在 notes 阶段失败**）
2. 同步 `Cargo.toml` 的 `version`
3. `git tag -a vX.Y.Z -m "..." && git push origin vX.Y.Z`

Release 说明由 `scripts/extract_changelog.sh` 从 CHANGELOG 截取，通过 `descriptionFromFile` 写入。

本地预览某个版本的说明：

```bash
bash scripts/extract_changelog.sh v0.1.0
```

## CI

两个平台各有一条流水线，产物完全一致：

| 平台 | 配置 | 产物 |
|---|---|---|
| GitHub | `.github/workflows/release.yml` | `clipkeep-gateway-{x86_64,aarch64}-linux` + `SHA256SUMS` |
| CNB | `.cnb.yml` | 同上 |

- GitHub 用 `cross` 并行编两个 musl target，quality job 先跑 fmt / clippy / test
- CNB 用 `scripts/build-release.sh`：x86_64 走系统 `musl-tools`，aarch64 临时拉 musl.cc 交叉工具链
- 两边都只认 `v*` tag，Release 说明从 `CHANGELOG.md` 截取
