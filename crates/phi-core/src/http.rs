use std::{
    collections::{BTreeMap, HashSet},
    fs,
    path::PathBuf,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use futures_util::StreamExt;
use phi_protocol::Event;
use serde::Deserialize;
use serde_json::Value;

#[derive(Clone, Debug, Deserialize)]
pub struct SecretConfig {
    pub path: PathBuf,
    pub bearer_pointer: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    pub refresh: Option<RefreshConfig>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct RefreshConfig {
    pub url: String,
    pub client_id: String,
    pub refresh_pointer: String,
}

pub struct SseRequest<'a> {
    pub allowed_origins: &'a HashSet<String>,
    pub secrets: &'a BTreeMap<String, SecretConfig>,
    pub url: &'a str,
    pub secret_name: &'a str,
    pub headers: &'a BTreeMap<String, String>,
    pub body: Value,
    pub timeout_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HttpObservation {
    Attempt { attempt: u8 },
    Status { attempt: u8, status: u16 },
    Retry { attempt: u8, status: Option<u16> },
    Failure { attempt: u8 },
}

pub async fn post_sse<F>(request: SseRequest<'_>, on_event: F) -> Result<Event>
where
    F: FnMut(&Value) -> bool,
{
    post_sse_observed(request, on_event, |_| {}).await
}

pub async fn post_sse_observed<F, O>(
    request: SseRequest<'_>,
    mut on_event: F,
    mut observe: O,
) -> Result<Event>
where
    F: FnMut(&Value) -> bool,
    O: FnMut(HttpObservation),
{
    let SseRequest {
        allowed_origins,
        secrets,
        url,
        secret_name,
        headers,
        body,
        timeout_ms,
    } = request;
    let parsed = reqwest::Url::parse(url)?;
    let origin = parsed.origin().ascii_serialization();
    if !allowed_origins.contains(&origin) {
        bail!("HTTP origin is not allowed: {origin}");
    }
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_millis(timeout_ms))
        .read_timeout(Duration::from_millis(timeout_ms))
        .build()?;
    let secret = secrets.get(secret_name).context("unknown secret handle")?;
    let values = load_and_refresh_secret(&client, allowed_origins, secret).await?;
    let bearer = pointer_string(&values, &secret.bearer_pointer)?;
    let mut request = client
        .post(parsed)
        .bearer_auth(bearer)
        .header(reqwest::header::ACCEPT, "text/event-stream")
        .json(&body);
    for (header, value) in headers {
        request = request.header(header, value);
    }
    for (header, pointer) in &secret.headers {
        request = request.header(header, pointer_string(&values, pointer)?);
    }
    for attempt in 0..3 {
        let attempt_number = attempt as u8 + 1;
        observe(HttpObservation::Attempt {
            attempt: attempt_number,
        });
        let current = request
            .try_clone()
            .context("HTTP request is not retryable")?;
        let response = match current.send().await {
            Ok(response)
                if attempt < 2
                    && (response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS
                        || response.status().is_server_error()) =>
            {
                observe(HttpObservation::Status {
                    attempt: attempt_number,
                    status: response.status().as_u16(),
                });
                observe(HttpObservation::Retry {
                    attempt: attempt_number,
                    status: Some(response.status().as_u16()),
                });
                retry_delay(attempt).await;
                continue;
            }
            Ok(response) => response,
            Err(error) if attempt < 2 && (error.is_connect() || error.is_timeout()) => {
                observe(HttpObservation::Retry {
                    attempt: attempt_number,
                    status: None,
                });
                retry_delay(attempt).await;
                continue;
            }
            Err(error) => {
                observe(HttpObservation::Failure {
                    attempt: attempt_number,
                });
                return Err(error.into());
            }
        };
        let status = response.status();
        observe(HttpObservation::Status {
            attempt: attempt_number,
            status: status.as_u16(),
        });
        if !status.is_success() {
            return Ok(Event::HttpCompleted {
                success: false,
                status: status.as_u16(),
                events: Vec::new(),
                error: response.text().await?,
            });
        }

        let mut decoder = SseDecoder::default();
        let mut events = Vec::new();
        let mut emitted_output = false;
        let mut stream = response.bytes_stream();
        let mut retry = false;
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(chunk) => {
                    decoder.push(&chunk);
                    for event in decoder.drain()? {
                        emitted_output |= on_event(&event);
                        events.push(event);
                    }
                }
                Err(error) if attempt < 2 && error.is_timeout() && !emitted_output => {
                    retry = true;
                    break;
                }
                Err(error) => return Err(error.into()),
            }
        }
        if retry {
            observe(HttpObservation::Retry {
                attempt: attempt_number,
                status: Some(status.as_u16()),
            });
            retry_delay(attempt).await;
            continue;
        }
        for event in decoder.finish()? {
            on_event(&event);
            events.push(event);
        }
        return Ok(Event::HttpCompleted {
            success: true,
            status: status.as_u16(),
            events,
            error: String::new(),
        });
    }
    unreachable!("retry loop returns on its last attempt")
}

async fn retry_delay(attempt: usize) {
    tokio::time::sleep(Duration::from_millis(250 * (attempt as u64 + 1))).await;
}

#[derive(Default)]
struct SseDecoder {
    buffer: String,
}

impl SseDecoder {
    fn push(&mut self, bytes: &[u8]) {
        self.buffer.push_str(&String::from_utf8_lossy(bytes));
    }

    fn drain(&mut self) -> Result<Vec<Value>> {
        let mut values = Vec::new();
        while let Some(end) = self.buffer.find("\n\n") {
            let frame = self.buffer[..end].to_owned();
            self.buffer.drain(..end + 2);
            if let Some(value) = parse_frame(&frame)? {
                values.push(value);
            }
        }
        Ok(values)
    }

    fn finish(mut self) -> Result<Vec<Value>> {
        if !self.buffer.is_empty() {
            self.buffer.push_str("\n\n");
        }
        self.drain()
    }
}

fn parse_frame(frame: &str) -> Result<Option<Value>> {
    let data = frame
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(str::trim_start)
        .collect::<Vec<_>>()
        .join("\n");
    if data.is_empty() || data == "[DONE]" {
        return Ok(None);
    }
    Ok(Some(serde_json::from_str(&data)?))
}

async fn load_and_refresh_secret(
    client: &reqwest::Client,
    allowed_origins: &HashSet<String>,
    secret: &SecretConfig,
) -> Result<Value> {
    let path = expand_home(&secret.path)?;
    let mut values: Value = serde_json::from_slice(&fs::read(&path)?)?;
    let access = pointer_string(&values, &secret.bearer_pointer)?;
    let Some(refresh) = &secret.refresh else {
        return Ok(values);
    };
    if !jwt_expires_soon(access)? {
        return Ok(values);
    }
    let url = reqwest::Url::parse(&refresh.url)?;
    if !allowed_origins.contains(&url.origin().ascii_serialization()) {
        bail!("OAuth refresh origin is not allowed");
    }
    let refresh_token = pointer_string(&values, &refresh.refresh_pointer)?.to_owned();
    let response = client
        .post(url)
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token.as_str()),
            ("client_id", refresh.client_id.as_str()),
        ])
        .send()
        .await?;
    if !response.status().is_success() {
        bail!("OAuth refresh failed with {}", response.status());
    }
    let refreshed: Value = response.json().await?;
    *values
        .pointer_mut(&secret.bearer_pointer)
        .context("access token destination is missing")? =
        refreshed
            .get("access_token")
            .cloned()
            .context("refresh response has no access_token")?;
    if let Some(token) = refreshed.get("refresh_token") {
        *values
            .pointer_mut(&refresh.refresh_pointer)
            .context("refresh token destination is missing")? = token.clone();
    }
    write_secret(&path, &values)?;
    Ok(values)
}

fn jwt_expires_soon(token: &str) -> Result<bool> {
    let Some(payload) = token.split('.').nth(1) else {
        return Ok(false);
    };
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload)?;
    let claims: Value = serde_json::from_slice(&decoded)?;
    let Some(exp) = claims.get("exp").and_then(Value::as_u64) else {
        return Ok(false);
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    Ok(exp <= now + 60)
}

fn write_secret(path: &std::path::Path, value: &Value) -> Result<()> {
    crate::write_json_atomic(path, value, crate::AtomicWriteMode::Overwrite)
}

fn pointer_string<'a>(value: &'a Value, pointer: &str) -> Result<&'a str> {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .context("secret value is missing")
}

fn expand_home(path: &std::path::Path) -> Result<PathBuf> {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix("~/") {
        return Ok(PathBuf::from(std::env::var("HOME")?).join(rest));
    }
    Ok(path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    fn secret(path: PathBuf) -> SecretConfig {
        fs::write(&path, r#"{"token":"test"}"#).unwrap();
        SecretConfig {
            path,
            bearer_pointer: "/token".into(),
            headers: BTreeMap::new(),
            refresh: None,
        }
    }

    fn request<'a>(
        origin: &'a str,
        allowed_origins: &'a HashSet<String>,
        secrets: &'a BTreeMap<String, SecretConfig>,
        headers: &'a BTreeMap<String, String>,
        timeout_ms: u64,
    ) -> SseRequest<'a> {
        SseRequest {
            allowed_origins,
            secrets,
            url: origin,
            secret_name: "test",
            headers,
            body: serde_json::json!({}),
            timeout_ms,
        }
    }

    #[test]
    fn decodes_sse_across_chunks() {
        let mut decoder = SseDecoder::default();
        decoder.push(b"data: {\"type\":\"one\"");
        assert!(decoder.drain().unwrap().is_empty());
        decoder.push(b"}\n\ndata: [DONE]\n\n");
        assert_eq!(
            decoder.drain().unwrap(),
            vec![serde_json::json!({ "type": "one" })]
        );
    }

    #[tokio::test]
    async fn read_timeout_resets_when_data_arrives() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 2048];
            let _ = stream.read(&mut request).unwrap();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
            for event in ["one", "two"] {
                std::thread::sleep(Duration::from_millis(60));
                write!(stream, "data: {{\"type\":\"{event}\"}}\n\n").unwrap();
                stream.flush().unwrap();
            }
        });
        let root = tempfile::tempdir().unwrap();
        let origin = format!("http://{address}");
        let allowed_origins = HashSet::from([origin.clone()]);
        let secrets = BTreeMap::from([("test".into(), secret(root.path().join("auth.json")))]);
        let headers = BTreeMap::new();

        let event = post_sse(
            request(&origin, &allowed_origins, &secrets, &headers, 100),
            |_| false,
        )
        .await
        .unwrap();
        server.join().unwrap();
        let Event::HttpCompleted { events, .. } = event else {
            panic!("expected HTTP completion");
        };
        assert_eq!(events.len(), 2);
    }

    #[tokio::test]
    async fn retries_an_idle_stream_before_visible_output() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            for attempt in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0; 2048];
                let _ = stream.read(&mut request).unwrap();
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n"
                )
                .unwrap();
                stream.flush().unwrap();
                if attempt == 0 {
                    write!(stream, "data: {{\"type\":\"response.created\"}}\n\n").unwrap();
                    stream.flush().unwrap();
                    std::thread::sleep(Duration::from_millis(100));
                } else {
                    write!(stream, "data: {{\"type\":\"done\"}}\n\n").unwrap();
                }
            }
        });
        let root = tempfile::tempdir().unwrap();
        let origin = format!("http://{address}");
        let allowed_origins = HashSet::from([origin.clone()]);
        let secrets = BTreeMap::from([("test".into(), secret(root.path().join("auth.json")))]);
        let headers = BTreeMap::new();

        let mut observations = Vec::new();
        let event = post_sse_observed(
            request(&origin, &allowed_origins, &secrets, &headers, 50),
            |_| false,
            |observation| observations.push(observation),
        )
        .await
        .unwrap();
        server.join().unwrap();
        let Event::HttpCompleted { events, .. } = event else {
            panic!("expected HTTP completion");
        };
        assert_eq!(events, vec![serde_json::json!({ "type": "done" })]);
        assert_eq!(
            observations,
            vec![
                HttpObservation::Attempt { attempt: 1 },
                HttpObservation::Status {
                    attempt: 1,
                    status: 200,
                },
                HttpObservation::Retry {
                    attempt: 1,
                    status: Some(200),
                },
                HttpObservation::Attempt { attempt: 2 },
                HttpObservation::Status {
                    attempt: 2,
                    status: 200,
                },
            ]
        );
    }

    #[test]
    fn reads_jwt_expiry() {
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"exp":1}"#);
        assert!(jwt_expires_soon(&format!("x.{payload}.y")).unwrap());
    }

    #[tokio::test]
    async fn refreshes_and_persists_an_expired_token() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 2048];
            let _ = stream.read(&mut request).unwrap();
            let body = r#"{"access_token":"fresh.access.token","refresh_token":"fresh-refresh"}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        });
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("auth.json");
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"exp":1}"#);
        fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({
                "tokens": {
                    "access_token": format!("x.{payload}.y"),
                    "refresh_token": "old-refresh"
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let origin = format!("http://{address}");
        let secret = SecretConfig {
            path: path.clone(),
            bearer_pointer: "/tokens/access_token".into(),
            headers: BTreeMap::new(),
            refresh: Some(RefreshConfig {
                url: format!("{origin}/oauth/token"),
                client_id: "client".into(),
                refresh_pointer: "/tokens/refresh_token".into(),
            }),
        };
        let values =
            load_and_refresh_secret(&reqwest::Client::new(), &HashSet::from([origin]), &secret)
                .await
                .unwrap();
        server.join().unwrap();
        assert_eq!(values["tokens"]["access_token"], "fresh.access.token");
        let saved: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(saved["tokens"]["refresh_token"], "fresh-refresh");
    }
}
