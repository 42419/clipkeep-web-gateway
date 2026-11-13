//! 启动入口。配置全部来自环境变量，见 `.env.example`。

use std::net::SocketAddr;
use std::process::ExitCode;

use clipkeep_gateway::{build_router, config::Config, AppState};

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

    let bind = config.bind;
    let static_dir = config.static_dir.clone();
    let delivery = config.delivery;
    let allowed = config.allowed_hosts.len();

    let state = match AppState::new(config) {
        Ok(state) => state,
        Err(error) => {
            eprintln!("初始化失败：{error}");
            return ExitCode::from(2);
        }
    };

    if !static_dir.is_dir() {
        tracing::warn!(
            dir = %static_dir.display(),
            "静态目录不存在，/ 会返回 404；放入 flutter build web 产物即可"
        );
    }

    let app = build_router(state);
    let listener = match tokio::net::TcpListener::bind(bind).await {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("无法监听 {bind}：{error}");
            return ExitCode::from(2);
        }
    };

    tracing::info!(
        %bind,
        static_dir = %static_dir.display(),
        ?delivery,
        allowed_hosts = allowed,
        "clipkeep-gateway 已启动"
    );

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
