//! Binary entrypoint for the orchestration API.

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    elowen_api::run().await
}
