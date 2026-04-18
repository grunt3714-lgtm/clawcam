use anyhow::Result;
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct Detection {
    pub class: String,
    pub class_id: u32,
    pub score: f32,
    pub left: u32,
    pub top: u32,
    pub right: u32,
    pub bottom: u32,
}

#[derive(Debug, Serialize)]
pub struct WebhookPayload {
    pub ts: String,
    pub epoch: i64,
    #[serde(rename = "type")]
    pub event_type: String,
    pub detail: String,
    pub source: String,
    pub host: String,
    pub image: String,
    pub predictions: Vec<Detection>,
}

pub async fn send(
    url: &str,
    token: Option<&str>,
    payload: &WebhookPayload,
) -> Result<()> {
    let client = reqwest::Client::new();
    let mut req = client.post(url).json(payload);
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    let resp = req.send().await?;
    if !resp.status().is_success() {
        tracing::warn!("webhook returned {}: {}", resp.status(), resp.text().await.unwrap_or_default());
    } else {
        tracing::info!("webhook delivered successfully");
    }
    Ok(())
}
