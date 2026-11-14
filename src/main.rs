//! 启动入口。配置全部来自环境变量，见 `.env.example`。

use std::net::SocketAddr;
use std::process::ExitCode;

use clipkeep_gateway::{
    build_router,
    config::{Config, DeliveryMode},
    AppState,
};

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "clipkeep_gateway=info,tower_http=warn".into()),
        )
        .with_target(false)
        .init();

    let config = match Config::from_env() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("配置错误：{error}");
            eprintln!("参考 .env.example 补齐环境变量后重试。");
            return ExitCode::from(2);
        }
    };

    let state = match AppState::new(config) {
        Ok(state) => state,
        Err(error) => {
            eprintln!("初始化失败：{error}");
            return ExitCode::from(2);
        }
    };

    let app = build_router(state.clone());
    let bind = state.config.bind;

    // 兜底清理：残留目录不会超过 10 分钟，进程异常退出也不会长期占盘。
    spawn_bundle_janitor(state.config.bundle_tmp.clone());

    let listener = match tokio::net::TcpListener::bind(bind).await {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("无法监听 {bind}：{error}");
            return ExitCode::from(2);
        }
    };

    print_banner(&state.config);

    let serve = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal());

    if let Err(error) = serve.await {
        eprintln!("服务异常退出：{error}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

/// 每 10 分钟扫一次打包临时目录。
///
/// 正常情况下请求结束 120 秒就会删掉自己的目录；这个定时器只兜底
/// 「进程在延迟清理生效前重启」这类情况。
fn spawn_bundle_janitor(dir: std::path::PathBuf) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(600));
        // interval 第一次 tick 立即返回，跳过它。
        ticker.tick().await;
        loop {
            ticker.tick().await;
            clipkeep_gateway::bundle::purge_stale(&dir);
        }
    });
}

/// 启动摘要用纯文本，不带时间戳，方便一眼确认版本和关键配置。
fn print_banner(config: &Config) {
    let version = env!("CARGO_PKG_VERSION");
    let static_state = if config.static_dir.is_dir() {
        "存在"
    } else {
        "不存在，访问 / 会 404"
    };
    let delivery = match config.delivery {
        DeliveryMode::Proxy => "proxy —— 服务端中转，浏览器直接下载",
        DeliveryMode::Redirect => "redirect —— 302 直连 CDN，浏览器会内联播放",
    };
    let hours = config.session_ttl.as_secs() / 3600;

    println!("ClipKeep Gateway v{version}");
    println!("  监听      {}", config.bind);
    println!(
        "  静态目录  {}（{static_state}）",
        config.static_dir.display()
    );
    println!("  DTK       {}", config.dtk_base);
    println!("  下载      {delivery}");
    println!("  放行域名  {}", config.allowed_hosts.join(", "));
    println!(
        "  会话      {hours} 小时 · cookie Secure={} · 信任代理={}",
        yes_no(config.cookie_secure),
        yes_no(config.trust_proxy)
    );
    println!("  解析限流  {}/分钟/IP", config.parse_rate_per_min);
}

fn yes_no(value: bool) -> &'static str {
    if value {
        "是"
    } else {
        "否"
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
    tracing::info!("收到退出信号，正在关闭");
}
