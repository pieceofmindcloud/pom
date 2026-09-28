use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;

/// A readiness probe must not inherit the long timeout used by generation.
const AVAILABILITY_TIMEOUT: Duration = Duration::from_millis(500);
/// `/health` reports the last result immediately and refreshes it lazily in
/// the background instead of connecting to an optional service on every hit.
const AVAILABILITY_REFRESH: Duration = Duration::from_secs(15);

pub struct OllamaClient {
    pub base_url: String,
    client: reqwest::Client,
    availability: Arc<tokio::sync::Mutex<AvailabilityCache>>,
}

struct AvailabilityCache {
    value: bool,
    checked_at: Option<std::time::Instant>,
    refreshing: bool,
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
            availability: Arc::new(tokio::sync::Mutex::new(AvailabilityCache {
                value: false,
                checked_at: None,
                refreshing: false,
            })),
        }
    }

    pub async fn is_available(&self) -> bool {
        let (cached, refresh) = {
            let mut cache = self.availability.lock().await;
            let refresh = cache
                .checked_at
                .map(|checked_at| checked_at.elapsed() >= AVAILABILITY_REFRESH)
                .unwrap_or(true)
                && !cache.refreshing;
            if refresh {
                cache.refreshing = true;
            }
            (cache.value, refresh)
        };

        if refresh {
            let client = self.client.clone();
            let url = format!("{}/api/tags", self.base_url);
            let availability = self.availability.clone();
            tokio::spawn(async move {
                let value = probe_availability(client, url).await;
                let mut cache = availability.lock().await;
                cache.value = value;
                cache.checked_at = Some(std::time::Instant::now());
                cache.refreshing = false;
            });
        }
        cached
    }

    pub async fn generate(&self, model: &str, prompt: &str) -> Result<GenerateResponse> {
        let body = GenerateRequest {
            model,
            prompt,
            stream: false,
        };
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

async fn probe_availability(client: reqwest::Client, url: String) -> bool {
    match tokio::time::timeout(AVAILABILITY_TIMEOUT, client.get(url).send()).await {
        Ok(Ok(response)) => response.status().is_success(),
        Ok(Err(_)) | Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    #[tokio::test]
    async fn availability_probe_does_not_hold_readiness_open() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_secs(2)).await;
        });
        let client = OllamaClient::new(&format!("http://{address}"));

        let result = tokio::time::timeout(Duration::from_secs(1), client.is_available()).await;
        assert!(
            result.is_ok(),
            "an unavailable optional Ollama service must not block /health"
        );
        assert!(!result.unwrap());
        server.abort();
    }

    #[tokio::test]
    async fn availability_is_cached_while_the_next_probe_runs_in_background() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(AtomicUsize::new(0));
        let server_requests = requests.clone();
        let server = tokio::spawn(async move {
            loop {
                let accepted =
                    tokio::time::timeout(Duration::from_millis(250), listener.accept()).await;
                let Ok(Ok((mut stream, _))) = accepted else {
                    break;
                };
                server_requests.fetch_add(1, Ordering::SeqCst);
                let mut request = [0u8; 1024];
                let _ = stream.read(&mut request).await.unwrap();
                stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .await
                    .unwrap();
            }
        });

        let client = OllamaClient::new(&format!("http://{address}"));
        assert!(!client.is_available().await);

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if client.is_available().await {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("a disponibilidade deveria ser atualizada em background");
        assert_eq!(requests.load(Ordering::SeqCst), 1);

        assert!(client.is_available().await);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(requests.load(Ordering::SeqCst), 1);
        server.await.unwrap();
    }
}
