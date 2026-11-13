# clipkeep-gateway

ClipKeep 网页版网关。把 DTK 的解析能力、Flutter web 的静态页面和浏览器下载，收敛到**一个静态链接的二进制**里。

## 它做什么

| 端点 | 行为 |
|---|---|
| `POST /api/session` | 用 `SITE_TOKEN` 换一张签名 cookie（`{token}`） |
| `GET /api/session` | 查看当前是否已登录 |
| `DELETE /api/session` | 清 cookie |
| `POST /api/parse` | 透传 DTK `/api/v1/parse`，注入 `X-API-Key`，`wait=20` 超时自动补轮询 |
| `GET /api/tasks/{id}` | 透传任务查询 |
| `GET /dl?u=<直链>&n=<文件名>` | 默认服务端流式中转（透传 Range）；`p=0` 时改为 302 跳 CDN |
| `GET /healthz` | 存活检查 |
| `GET /*` | Flutter web 静态产物，找不到回 `index.html` |

**不做**：用户体系、数据库、转码、落盘、任务队列。

透传 DTK 的 `{success, data}` envelope 是刻意的：Flutter 侧现成的 `dtk_mapper` 不用改一行。

## 部署（不需要装 Rust）

Release 直接给可执行文件，按机器架构选：

| 文件 | 适用 |
|---|---|
| `clipkeep-gateway-x86_64-linux` | 常见 x86_64 服务器（阿里云 ECS 默认） |
| `clipkeep-gateway-aarch64-linux` | ARM64（部分轻量应用服务器、Ampere） |

两者都是 **musl 静态链接**，不依赖系统 glibc 和任何运行时，`ldd` 会提示不是动态可执行文件。

### 1. 下载二进制

```bash
arch=$(uname -m)          # x86_64 或 aarch64
sudo mkdir -p /opt/clipkeep-gateway/static
cd /opt/clipkeep-gateway

sudo curl -L -o clipkeep-gateway \
  https://github.com/42419/clipkeep-web-gateway/releases/download/v0.1.1/clipkeep-gateway-${arch}-linux
# 国内慢就换 CNB 源：
# https://cnb.cool/yunfeiii/clipkeep-web-gateway/-/releases/download/v0.1.1/clipkeep-gateway-${arch}-linux

sudo chmod +x clipkeep-gateway
```

### 2. 写 `.env`

下面这份是完整配置，只有 `DTK_BASE`、`DTK_API_KEY`、`SITE_TOKEN` 三个值需要改：

```bash
sudo tee /opt/clipkeep-gateway/.env >/dev/null <<'EOF'
# 监听地址；放在 nginx 后面就改成 127.0.0.1:8080
BIND=0.0.0.0:8443
# DTK 地址，即 frps 在本机映射的端口
DTK_BASE=http://127.0.0.1:58000
# DTK 的 API Key
DTK_API_KEY=
# 前端访问口令，至少 8 位；用 openssl rand -base64 24 生成
SITE_TOKEN=
# Flutter web 产物目录
STATIC_DIR=./static
# proxy = 服务端中转（能下载、文件名可控）；redirect = 302 直连 CDN（会被浏览器内联播放）
DELIVERY=proxy
# 每个 IP 每分钟允许的解析次数，0 = 不限
PARSE_RATE_PER_MIN=20
# 会话有效期，小时
SESSION_TTL_HOURS=720
# 放在 nginx 后面才设 1
TRUST_PROXY=0
# 只有 HTTPS 部署才设 1
COOKIE_SECURE=0
EOF

sudo chmod 600 /opt/clipkeep-gateway/.env
sudo nano /opt/clipkeep-gateway/.env     # 填 DTK_API_KEY 与 SITE_TOKEN
```

### 3. 装 systemd

```bash
sudo useradd -r -s /usr/sbin/nologin clipkeep
sudo chown -R clipkeep:clipkeep /opt/clipkeep-gateway

sudo tee /etc/systemd/system/clipkeep-gateway.service >/dev/null <<'EOF'
[Unit]
Description=ClipKeep web gateway
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=clipkeep
Group=clipkeep
WorkingDirectory=/opt/clipkeep-gateway
EnvironmentFile=/opt/clipkeep-gateway/.env
ExecStart=/opt/clipkeep-gateway/clipkeep-gateway
Restart=on-failure
RestartSec=3

NoNewPrivileges=true
PrivateTmp=true
ProtectSystem=strict
ProtectHome=true
ReadOnlyPaths=/opt/clipkeep-gateway

[Install]
WantedBy=multi-user.target
EOF

sudo systemctl daemon-reload
sudo systemctl enable --now clipkeep-gateway
sudo systemctl status clipkeep-gateway --no-pager
```

### 4. 验证

```bash
curl -s http://127.0.0.1:8443/healthz                  # {"ok":true}
journalctl -u clipkeep-gateway -n 20 | grep 已启动      # 应显示 delivery=Proxy
```

外网访问需要安全组放行 8443。

### 5. 升级二进制

```bash
sudo systemctl stop clipkeep-gateway
sudo curl -L -o /opt/clipkeep-gateway/clipkeep-gateway <新版本地址>
sudo chmod +x /opt/clipkeep-gateway/clipkeep-gateway
sudo systemctl start clipkeep-gateway
```

### 想直接拿仓库里的文件

```bash
git clone https://github.com/42419/clipkeep-web-gateway.git
# 里面有 .env.example、deploy/clipkeep-gateway.service、README
```

## 本地跑

```bash
set -a; . ./.env; set +a     # 程序只读环境变量，不解析 .env
cargo run --release
```

静态目录默认 `./static`，把 `flutter build web` 的产物放进去即可。

## 两种部署拓扑

```
# A. 直接对外（HTTP，最省事）
浏览器 → http://<IP>:8443 → clipkeep-gateway

# B. 放在 nginx 后面（HTTPS）
浏览器 → https://<IP>:8443 (nginx, TLS) → 127.0.0.1:8080 → clipkeep-gateway
```

方案 B 记得设 `BIND=127.0.0.1:8080`、`TRUST_PROXY=1`、`COOKIE_SECURE=1`。

## 下载模式怎么选

| `DELIVERY` | 浏览器行为 | 带宽 | 文件名 |
|---|---|---|---|
| `proxy`（默认） | 直接落文件 | 过一遍服务器 | 可控 |
| `redirect` | `video/mp4` 被内联播放，**不触发下载** | 零 | 丢失 |

`redirect` 只在「想在网页里预览」时有用。浏览器对顶层导航到 `video/mp4` 的响应一律内联播放，
`Content-Disposition` 管不到跨域 302 之后的响应，`<a download>` 也只对同源生效。

单次请求可以用 `?p=0` 临时切成 redirect，`?p=1` 强制中转。

## 抖音链接为什么要用 play 接口

DTK 返回的每条流有三条 URL：两条 `douyinvod.com` 直链、一条 `www.douyin.com/aweme/v1/play?...`。

实测（2026-11-13）：

| 请求 | 结果 |
|---|---|
| `douyinvod` 直链，任意 Referer/UA | **403**（签名一次性/会过期） |
| `aweme/v1/play` 接口 | **302 → 新的 `douyinvod` 地址** |
| 跟随得到的新地址，含无 Referer | **206 + video/mp4** |

`play` 本质是换票接口，前端必须把它交给 `/dl`，而不是 `media.video.url`。gateway 不参与选链接。

## 安全边界

- `X-API-Key` 永不下发前端，只存在于网关进程内存
- `/dl` 只放行 `ALLOWED_HOSTS` 里的域名，防止变成开放代理
- `/dl` 与 `/api/parse` 共用会话 cookie，链接本身不可分享（要分享得另做签名 URL）
- 解析接口按 IP 限流（`PARSE_RATE_PER_MIN`）
- 会话 cookie 是 HMAC 签名 + 过期时间，无需服务端状态

## 发版流程

1. 在 `CHANGELOG.md` 补 `## [vX.Y.Z] - YYYY-MM-DD` 段落（**缺了 CI 会在 notes 阶段失败**）
2. 同步 `Cargo.toml` 的 `version`
3. `git tag -a vX.Y.Z -m "..." && git push origin vX.Y.Z && git push cnb vX.Y.Z`

本地预览某个版本的说明：

```bash
bash scripts/extract_changelog.sh v0.1.1
```

## CI

两个平台各有一条流水线，产物完全一致：

| 平台 | 配置 | 产物 |
|---|---|---|
| GitHub（`origin`） | `.github/workflows/release.yml` | `clipkeep-gateway-{x86_64,aarch64}-linux` + `SHA256SUMS` |
| CNB（`cnb`） | `.cnb.yml` | 同上 |

- GitHub 用 `cross` 并行编两个 musl target，quality job 先跑 fmt / clippy / test
- CNB 用 `scripts/build-release.sh`：x86_64 走系统 `musl-tools`，aarch64 临时拉 musl.cc 交叉工具链
- 两边都只认 `v*` tag，Release 说明从 `CHANGELOG.md` 截取
