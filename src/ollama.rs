use anyhow::Result;
use serde::{Deserialize, Serialize};

pub struct OllamaClient {
    pub base_url: String,
    client: reqwest::Client,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OllamaModel {
    pub name: String,
    pub size: u64,
}

#[derive(Deserialize)]
struct TagsResponse {
    models: Vec<OllamaModel>,
}

#[derive(Serialize)]
struct GenerateRequest<'a> {
    model: &'a str,
    prompt: &'a str,
    stream: bool,
}

#[derive(Debug, Deserialize)]
pub struct GenerateResponse {
    pub response: String,
    pub done: bool,
}

impl OllamaClient {
    pub fn new(base_url: &str) -> Self {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(300))
            .build()
            .unwrap();
        OllamaClient {
            base_url: base_url.trim_end_matches('/').to_string(),
            client,
        }
    }

    pub async fn is_available(&self) -> bool {
        self.client
            .get(format!("{}/api/tags", self.base_url))
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false)
    }

    pub async fn list_models(&self) -> Result<Vec<OllamaModel>> {
        let resp = self
            .client
            .get(format!("{}/api/tags", self.base_url))
            .send()
            .await?
            .json::<TagsResponse>()
            .await?;
        Ok(resp.models)
    }

    pub async fn generate(&self, model: &str, prompt: &str) -> Result<GenerateResponse> {
        let body = GenerateRequest { model, prompt, stream: false };
        let resp = self
            .client
            .post(format!("{}/api/generate", self.base_url))
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow::anyhow!("Ollama returned {}: {}", status, text));
        }

        Ok(resp.json::<GenerateResponse>().await?)
    }
}
