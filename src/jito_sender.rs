use anyhow::Context;
use reqwest::Client;
use serde::Deserialize;
use serde_json::json;
use tracing::error;

#[derive(Deserialize)]
pub struct JitoResponse {
    //bundle id is response
    pub result: String,
}

pub async fn send_bundle(
    client: &Client,
    url: &str,
    encoded_transactions: Vec<String>,
) -> anyhow::Result<String> {
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "sendBundle",
        "params": [encoded_transactions]
    });
    let response = client.post(url).json(&body).send().await?;
    let status = response.status();
    let body = response.text().await?;
    if !status.is_success() {
        return Err(anyhow::anyhow!("failed to send tx: {}", body));
    }
    let parsed_resp =
        serde_json::from_str::<JitoResponse>(&body).context("cannot deserialize signature")?;
    Ok(parsed_resp.result)
}
