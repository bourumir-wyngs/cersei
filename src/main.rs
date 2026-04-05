#[tokio::main]
async fn main() -> anyhow::Result<()> {
    abstract_cli::run().await
}
