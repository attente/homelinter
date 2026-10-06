//! Server-Sent Events: the push channel between the service and its
//! clients. One broadcast channel per service instance; every connected
//! `GET /events` stream subscribes and receives the same ordered events.

use serde_json::{json, Value};
use tokio::sync::broadcast;

use homelinter_model::suggestion::{ExecutionReport, ScanSummary, SessionState, Suggestion};

/// Event payloads defined by the Service API events table. The `name` maps
/// onto the SSE `event:` field and `data` onto `data:`.
#[derive(Debug, Clone)]
pub enum ServerEvent {
    State(SessionState),
    ScanProgress {
        phase: &'static str,
        scanned: u64,
        elapsed_ms: u64,
    },
    ScanComplete {
        summary: ScanSummary,
    },
    SuggestionBatch {
        items: Vec<Suggestion>,
        total: usize,
    },
    BatchValidated {
        valid: bool,
        rejected: Vec<RejectedId>,
    },
    MetadataProgress {
        completed: usize,
        total: usize,
    },
    ExecutionOp {
        suggestion_id: String,
        operation: String,
        status: String,
        detail: Value,
    },
    ExecutionComplete {
        report: ExecutionReport,
    },
    ConfigUpdated {
        config: Value,
    },
    CacheCleared,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct RejectedId {
    pub id: String,
    pub reason: String,
}

impl ServerEvent {
    pub fn name(&self) -> &'static str {
        match self {
            ServerEvent::State(_) => "session.state",
            ServerEvent::ScanProgress { .. } => "scan.progress",
            ServerEvent::ScanComplete { .. } => "scan.complete",
            ServerEvent::SuggestionBatch { .. } => "suggestion.batch",
            ServerEvent::BatchValidated { .. } => "batch.validated",
            ServerEvent::MetadataProgress { .. } => "metadata.progress",
            ServerEvent::ExecutionOp { .. } => "execution.op",
            ServerEvent::ExecutionComplete { .. } => "execution.complete",
            ServerEvent::ConfigUpdated { .. } => "config.updated",
            ServerEvent::CacheCleared => "cache.cleared",
        }
    }

    pub fn data(&self) -> Value {
        match self {
            ServerEvent::State(s) => json!({ "state": s }),
            ServerEvent::ScanProgress {
                phase,
                scanned,
                elapsed_ms,
            } => {
                json!({ "phase": phase, "scanned": scanned, "elapsed_ms": elapsed_ms })
            }
            ServerEvent::ScanComplete { summary } => {
                json!({ "summary": serde_json::to_value(summary).unwrap_or(Value::Null) })
            }
            ServerEvent::SuggestionBatch { items, total } => {
                let items_v: Vec<Value> = items
                    .iter()
                    .filter_map(|s| serde_json::to_value(s).ok())
                    .collect();
                json!({ "items": items_v, "total": total })
            }
            ServerEvent::BatchValidated { valid, rejected } => {
                json!({ "valid": valid, "rejected": rejected })
            }
            ServerEvent::MetadataProgress { completed, total } => {
                json!({ "completed": completed, "total": total })
            }
            ServerEvent::ExecutionOp {
                suggestion_id,
                operation,
                status,
                detail,
            } => {
                json!({
                    "suggestion_id": suggestion_id,
                    "operation": operation,
                    "status": status,
                    "detail": detail,
                })
            }
            ServerEvent::ExecutionComplete { report } => {
                json!({ "report": serde_json::to_value(report).unwrap_or(Value::Null) })
            }
            ServerEvent::ConfigUpdated { config } => json!({ "config": config }),
            ServerEvent::CacheCleared => json!({}),
        }
    }
}

/// A broadcast sender helper with lossless fan-out for all connected
/// clients. Subscribers that lag behind drop out of the stream; the SSE
/// handler reconnects them with a fresh snapshot on their next request.
#[derive(Debug, Clone)]
pub struct EventBus {
    tx: broadcast::Sender<ServerEvent>,
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

impl EventBus {
    pub fn new() -> Self {
        let (tx, _rx) = broadcast::channel(4096);
        EventBus { tx }
    }

    pub fn publish(&self, event: ServerEvent) {
        let _ = self.tx.send(event);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<ServerEvent> {
        self.tx.subscribe()
    }
}
