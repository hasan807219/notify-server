use {
    dotenv::dotenv,
    notify_server::{config::Configuration, Result},
    std::str::FromStr,
    tokio::sync::broadcast,
    tracing_subscriber::fmt::format::FmtSpan,
};

#[tokio::main]
async fn main() -> Result<()> {
    let (_signal, shutdown) = broadcast::channel(1);
    dotenv().ok();

    let config = Configuration::new().expect("Failed to load config!");
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::from_str(&config.log_level).expect("Invalid log level"))
        .with_span_events(FmtSpan::CLOSE)
        .with_ansi(std::env::var("ANSI_LOGS").is_ok())
        .init();

    notify_server::bootstap(shutdown, config).await
}
