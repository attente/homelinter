//! HTTP client for the homelinter service API.
//!
//! The client speaks HTTP/1.1 to a homelinter service instance bound to a
//! loopback address. All request and response bodies are JSON values except
//! `POST /api/v1/export` without a `path` (script text) and `GET
//! /api/v1/events` (a Server-Sent Events stream).
//!
//! Errors follow the service envelope `{"error":{"code","message","details"}}`
//! with the status mapped from `code` (400/404/409/422/500).

mod sse;

use std::fmt;

use http_body_util::BodyExt;
use http_body_util::Full;
use hyper::body::{Bytes, Incoming};
use hyper::header::HeaderValue;
use hyper::Method;
use hyper::{Request, StatusCode};
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client as HyperClient;
use hyper_util::rt::TokioExecutor;
use serde_json::{json, Value};

pub use crate::sse::{SseEvent, SseReader};

/// Error type for service API calls.
#[derive(Debug, Clone)]
pub enum ClientsError {
    /// Transport-level failure (connect, reset, timeout, malformed HTTP).
    Transport(String),
    /// The service answered with an error envelope.
    Api {
        status: u16,
        code: String,
        message: String,
        details: Option<Value>,
    },
    /// The response could not be decoded as JSON or UTF-8.
    Decode(String),
    /// The SSE stream ended without another event.
    StreamClosed,
}

impl fmt::Display for ClientsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ClientsError::Transport(m) => write!(f, "transport error: {m}"),
            ClientsError::Api {
                status,
                code,
                message,
                ..
            } => {
                write!(f, "api error {status} ({code}): {message}")
            }
            ClientsError::Decode(m) => write!(f, "decode error: {m}"),
            ClientsError::StreamClosed => write!(f, "event stream closed"),
        }
    }
}

impl std::error::Error for ClientsError {}

impl ClientsError {
    /// True when the response carries a 404 (e.g. no execution report yet).
    pub fn is_not_found(&self) -> bool {
        matches!(self, ClientsError::Api { status: 404, .. })
    }
}

/// Result of `POST /api/v1/export`.
#[derive(Debug, Clone)]
pub enum ExportOutcome {
    /// Script text returned in the body; platform and count came in headers.
    ScriptText {
        platform: String,
        count: u64,
        text: String,
    },
    /// The service wrote the script and returned the written path.
    Written {
        path: String,
        platform: String,
        count: u64,
    },
}

/// A loopback HTTP client for one service instance.
#[derive(Clone)]
pub struct Client {
    inner: HyperClient<HttpConnector, Full<Bytes>>,
    base: String,
}

impl Client {
    /// Connect to a service listening on the given loopback port.
    pub fn new(port: u16) -> Self {
        let connector = HttpConnector::new();
        let inner = HyperClient::builder(TokioExecutor::new()).build(connector);
        Client {
            inner,
            base: format!("http://127.0.0.1:{port}"),
        }
    }

    /// Base URL prefix, for diagnostics.
    pub fn base_url(&self) -> &str {
        &self.base
    }

    /// Send a JSON-bodied request. Returns `(status, body)` where `body` is
    /// the parsed JSON response (or `Value::Null` for empty bodies).
    pub async fn json(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<(StatusCode, Value), ClientsError> {
        let (status, _, body_bytes) = self.send(method, path, body).await?;
        let value = if body_bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&body_bytes).map_err(|_| {
                ClientsError::Decode(format!(
                    "response body is not JSON ({}): {}",
                    status,
                    String::from_utf8_lossy(&body_bytes)
                        .chars()
                        .take(200)
                        .collect::<String>()
                ))
            })?
        };
        if !status.is_success() {
            return Err(self.envelope(status, &value));
        }
        Ok((status, value))
    }

    /// Send a request that expects a text body (used by script export).
    async fn text(
        &self,
        path: &str,
        body: Option<Value>,
    ) -> Result<(StatusCode, Vec<(String, String)>, String), ClientsError> {
        let (status, headers, body_bytes) = self.send(Method::POST, path, body).await?;
        if !status.is_success() {
            let value = serde_json::from_slice(&body_bytes).unwrap_or_else(|_| {
                json!({ "error": { "code": "unknown", "message": String::from_utf8_lossy(&body_bytes).into_owned() } })
            });
            return Err(self.envelope(status, &value));
        }
        let text = String::from_utf8(body_bytes.to_vec())
            .map_err(|e| ClientsError::Decode(format!("script body utf-8: {e}")))?;
        Ok((status, headers, text))
    }

    /// Send a request and return its status, headers, and raw body. Shared
    /// by the JSON and text response decoders.
    async fn send(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<(StatusCode, Vec<(String, String)>, Bytes), ClientsError> {
        let uri: hyper::Uri = format!("{}{}", self.base, path)
            .parse()
            .map_err(|e| ClientsError::Transport(format!("invalid uri: {e}")))?;
        let mut builder = Request::builder().method(method).uri(uri);
        builder = builder.header("host", self.host_header());
        let req = match body {
            Some(v) => {
                let bytes = serde_json::to_vec(&v)
                    .map_err(|e| ClientsError::Decode(format!("serialize: {e}")))?;
                builder = builder.header("content-type", "application/json");
                builder
                    .body(Full::new(Bytes::from(bytes)))
                    .map_err(|e| ClientsError::Transport(format!("request build: {e}")))?
            }
            None => builder
                .body(Full::new(Bytes::new()))
                .map_err(|e| ClientsError::Transport(format!("request build: {e}")))?,
        };
        let resp = self
            .inner
            .request(req)
            .await
            .map_err(|e| ClientsError::Transport(e.to_string()))?;
        let status = resp.status();
        let headers = resp
            .headers()
            .iter()
            .map(|(k, v)| {
                (
                    k.as_str().to_string(),
                    v.to_str().unwrap_or_default().to_string(),
                )
            })
            .collect::<Vec<_>>();
        let body_bytes = self.collect_body(resp.into_body()).await?;
        Ok((status, headers, body_bytes))
    }

    fn host_header(&self) -> HeaderValue {
        // Strip the scheme; value is always http://127.0.0.1:<port>.
        let host = self.base.trim_start_matches("http://");
        HeaderValue::from_str(host).unwrap_or_else(|_| HeaderValue::from_static("127.0.0.1"))
    }

    async fn collect_body(&self, body: Incoming) -> Result<Bytes, ClientsError> {
        body.collect()
            .await
            .map(|collected| collected.to_bytes())
            .map_err(|e| ClientsError::Transport(format!("body read: {e}")))
    }

    fn envelope(&self, status: StatusCode, value: &Value) -> ClientsError {
        let code = value["error"]["code"]
            .as_str()
            .unwrap_or("internal")
            .to_string();
        let message = value["error"]["message"]
            .as_str()
            .unwrap_or("unknown service error")
            .to_string();
        let details = match value["error"]["details"].clone() {
            Value::Null => None,
            other => Some(other),
        };
        ClientsError::Api {
            status: status.as_u16(),
            code,
            message,
            details,
        }
    }

    // ------------------------------------------------------------------
    // Endpoint helpers
    // ------------------------------------------------------------------

    /// GET /api/v1/health
    pub async fn health(&self) -> Result<Value, ClientsError> {
        let (_, v) = self.json(Method::GET, "/api/v1/health", None).await?;
        Ok(v)
    }

    /// POST /api/v1/session/scan; body fields follow the API contract.
    pub async fn start_scan(&self, body: Value) -> Result<Value, ClientsError> {
        let (_, v) = self
            .json(Method::POST, "/api/v1/session/scan", Some(body))
            .await?;
        Ok(v)
    }

    /// GET /api/v1/session
    pub async fn session(&self) -> Result<Value, ClientsError> {
        let (_, v) = self.json(Method::GET, "/api/v1/session", None).await?;
        Ok(v)
    }

    /// GET /api/v1/suggestions?status=... (filter optional)
    pub async fn suggestions(&self, status: Option<&str>) -> Result<Value, ClientsError> {
        let path = match status {
            Some(s) => format!("/api/v1/suggestions?status={s}"),
            None => "/api/v1/suggestions".to_string(),
        };
        let (_, v) = self.json(Method::GET, &path, None).await?;
        Ok(v)
    }

    /// POST /api/v1/suggestions/{id}/refresh: invalidate one suggestion's
    /// cached provider metadata and re-request it. Returns the suggestion
    /// marked pending under `{"item": ...}`.
    pub async fn refresh_metadata(&self, id: &str) -> Result<Value, ClientsError> {
        let (_, v) = self
            .json(Method::POST, &format!("/api/v1/suggestions/{id}/refresh"), None)
            .await?;
        Ok(v)
    }

    /// GET /api/v1/selection: `{"ids": [...]}`.
    pub async fn selection(&self) -> Result<Value, ClientsError> {
        let (_, v) = self.json(Method::GET, "/api/v1/selection", None).await?;
        Ok(v)
    }

    /// PUT /api/v1/selection: replace the selection exactly with `ids`.
    pub async fn selection_put(&self, ids: Vec<String>) -> Result<Value, ClientsError> {
        let (_, v) = self
            .json(
                Method::PUT,
                "/api/v1/selection",
                Some(json!({ "ids": ids })),
            )
            .await?;
        Ok(v)
    }

    /// POST /api/v1/selection/add with an id list or a filter object
    /// (`{"type":..., "asset_kind":..., "status":..., "selected":...}`).
    pub async fn selection_add(
        &self,
        ids: Option<Vec<String>>,
        filter: Option<Value>,
    ) -> Result<Value, ClientsError> {
        let mut body = json!({});
        if let Some(ids) = &ids {
            body["ids"] = json!(ids);
        }
        if let Some(f) = &filter {
            body["filter"] = f.clone();
        }
        let (_, v) = self
            .json(Method::POST, "/api/v1/selection/add", Some(body))
            .await?;
        Ok(v)
    }

    /// POST /api/v1/selection/remove with an id list or a filter object.
    pub async fn selection_remove(
        &self,
        ids: Option<Vec<String>>,
        filter: Option<Value>,
    ) -> Result<Value, ClientsError> {
        let mut body = json!({});
        if let Some(ids) = &ids {
            body["ids"] = json!(ids);
        }
        if let Some(f) = &filter {
            body["filter"] = f.clone();
        }
        let (_, v) = self
            .json(Method::POST, "/api/v1/selection/remove", Some(body))
            .await?;
        Ok(v)
    }

    /// POST /api/v1/validate with an optional id list (defaults to the
    /// selection). Returns `{"valid": bool, "rejected": [...]}`.
    pub async fn validate(&self, ids: Option<Vec<String>>) -> Result<Value, ClientsError> {
        let mut body = json!({});
        if let Some(ids) = &ids {
            body["ids"] = json!(ids);
        }
        let (_, v) = self
            .json(Method::POST, "/api/v1/validate", Some(body))
            .await?;
        Ok(v)
    }

    /// POST /api/v1/execute. `ids` is optional (defaults to the selection).
    pub async fn execute(&self, ids: Option<Vec<String>>) -> Result<Value, ClientsError> {
        let body = json!({ "ids": ids });
        let (_, v) = self
            .json(Method::POST, "/api/v1/execute", Some(body))
            .await?;
        Ok(v)
    }

    /// GET /api/v1/execution (404 -> ClientsError::Api{status:404})
    pub async fn execution_report(&self) -> Result<Value, ClientsError> {
        let (_, v) = self.json(Method::GET, "/api/v1/execution", None).await?;
        Ok(v)
    }

    /// POST /api/v1/export
    pub async fn export(
        &self,
        ids: Option<Vec<String>>,
        path: Option<String>,
    ) -> Result<ExportOutcome, ClientsError> {
        let mut body = json!({});
        if let Some(ids) = &ids {
            body["ids"] = json!(ids);
        }
        if let Some(path) = &path {
            body["path"] = json!(path);
        }
        let (status, headers, text) = self.text("/api/v1/export", Some(body)).await?;
        debug_assert!(status.is_success());
        let header = |name: &str| -> String {
            headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.clone())
                .unwrap_or_default()
        };
        let platform = header("x-homelinter-platform");
        let count = header("x-homelinter-count").parse().unwrap_or(0);
        if path.is_some() {
            // The service answers the JSON body variant when a path was
            // requested; re-parse the collected text.
            let v: Value = serde_json::from_str(&text)
                .map_err(|e| ClientsError::Decode(format!("export response: {e}")))?;
            Ok(ExportOutcome::Written {
                path: v["path"].as_str().unwrap_or_default().to_string(),
                platform: v["platform"].as_str().unwrap_or(&platform).to_string(),
                count: v["count"].as_u64().unwrap_or(count),
            })
        } else {
            Ok(ExportOutcome::ScriptText {
                platform,
                count,
                text,
            })
        }
    }

    /// DELETE /api/v1/cache
    pub async fn cache_clear(&self) -> Result<(), ClientsError> {
        self.json(Method::DELETE, "/api/v1/cache", None).await?;
        Ok(())
    }

    /// GET /api/v1/cache: cached provider results for a viewer. `kind`
    /// filters to one cache kind (e.g. `description`), `offset`/`limit`
    /// page through the entries.
    pub async fn cache_list(
        &self,
        kind: Option<&str>,
        offset: usize,
        limit: usize,
    ) -> Result<Value, ClientsError> {
        let mut path = format!("/api/v1/cache?offset={offset}&limit={limit}");
        if let Some(k) = kind {
            path.push_str("&kind=");
            path.push_str(k);
        }
        let (_, v) = self.json(Method::GET, &path, None).await?;
        Ok(v)
    }

    /// GET /api/v1/config
    pub async fn config(&self) -> Result<Value, ClientsError> {
        let (_, v) = self.json(Method::GET, "/api/v1/config", None).await?;
        Ok(v)
    }

    /// PUT /api/v1/config with a partial configuration body.
    pub async fn config_put(&self, partial: Value) -> Result<Value, ClientsError> {
        let (_, v) = self
            .json(Method::PUT, "/api/v1/config", Some(partial))
            .await?;
        Ok(v)
    }

    /// GET /api/v1/report/summary
    pub async fn summary(&self) -> Result<Value, ClientsError> {
        let (_, v) = self
            .json(Method::GET, "/api/v1/report/summary", None)
            .await?;
        Ok(v)
    }

    /// POST /api/v1/shutdown
    pub async fn shutdown(&self) -> Result<(), ClientsError> {
        self.json(Method::POST, "/api/v1/shutdown", None).await?;
        Ok(())
    }

    /// POST /api/v1/cancel
    pub async fn cancel(&self) -> Result<Value, ClientsError> {
        let (_, v) = self.json(Method::POST, "/api/v1/cancel", None).await?;
        Ok(v)
    }

    /// Open the SSE event channel. The reader yields events until the
    /// connection closes.
    pub async fn events(&self) -> Result<sse::SseReader, ClientsError> {
        let uri: hyper::Uri = format!("{}/api/v1/events", self.base)
            .parse()
            .map_err(|e| ClientsError::Transport(format!("invalid uri: {e}")))?;
        let req = Request::builder()
            .method(Method::GET)
            .uri(uri)
            .header("host", self.host_header())
            .header("accept", "text/event-stream")
            .body(Full::new(Bytes::new()))
            .map_err(|e| ClientsError::Transport(format!("request build: {e}")))?;
        let resp = self
            .inner
            .request(req)
            .await
            .map_err(|e| ClientsError::Transport(e.to_string()))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let bytes = self.collect_body(resp.into_body()).await?;
            let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            return Err(self.envelope(status, &value));
        }
        Ok(sse::SseReader::new(resp.into_body()))
    }
}
