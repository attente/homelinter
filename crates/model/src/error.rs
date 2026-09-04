//! API error envelope shared by the service contract.
//!
//! Errors are returned as `{"error":{"code":..,"message":..,"details":{..}}}`
//! with a status of 400 (bad request), 404 (not found), 409 (conflict with
//! the current session state), or 422 (validation failure).

use std::collections::BTreeMap;

use serde::Serialize;

/// Machine-readable error codes. The set mirrors the transport-level
/// semantics of the status code and is stable for client matching.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    BadRequest,
    NotFound,
    Conflict,
    ValidationFailed,
    Internal,
}

impl ErrorCode {
    pub fn status(&self) -> u16 {
        match self {
            ErrorCode::BadRequest => 400,
            ErrorCode::NotFound => 404,
            ErrorCode::Conflict => 409,
            ErrorCode::ValidationFailed => 422,
            ErrorCode::Internal => 500,
        }
    }
}

/// The JSON body of an error response.
#[derive(Debug, Clone, Serialize)]
pub struct ErrorBody {
    pub code: ErrorCode,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<BTreeMap<String, serde_json::Value>>,
}

/// A structured API error with a status code.
#[derive(Debug, Clone)]
pub struct ApiError {
    pub body: ErrorBody,
}

impl ApiError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            body: ErrorBody {
                code,
                message: message.into(),
                details: None,
            },
        }
    }

    pub fn with_details(
        code: ErrorCode,
        message: impl Into<String>,
        details: BTreeMap<String, serde_json::Value>,
    ) -> Self {
        Self {
            body: ErrorBody {
                code,
                message: message.into(),
                details: Some(details),
            },
        }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::BadRequest, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::NotFound, message)
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Conflict, message)
    }

    pub fn validation(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::ValidationFailed, message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Internal, message)
    }

    /// Details map with per-field validation errors, one entry per field.
    pub fn validation_errors(
        message: impl Into<String>,
        fields: impl IntoIterator<Item = (String, String)>,
    ) -> Self {
        let mut details = BTreeMap::new();
        for (field, reason) in fields {
            details.insert(field, serde_json::Value::String(reason));
        }
        Self::with_details(ErrorCode::ValidationFailed, message, details)
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.body.code, self.body.message)
    }
}

impl std::error::Error for ApiError {}

impl From<ApiError> for serde_json::Value {
    fn from(e: ApiError) -> Self {
        let body = e.body;
        serde_json::json!({ "error": {
            "code": body.code,
            "message": body.message,
            "details": body.details,
        }})
    }
}
