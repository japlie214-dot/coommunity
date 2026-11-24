#[tokio::main]
async fn main() -> anyhow::Result<()> {
    coommunity::run_cli().await
}
