//! Shared data model for homelinter.
//!
//! This crate owns the types every other crate depends on: asset kinds,
//! destination format strings, the suggestion model, session state, API
//! errors, path normalization, and the configuration file schema.

pub mod asset_kind;
pub mod config;
pub mod error;
pub mod format;
pub mod hash;
pub mod path;
pub mod prompt;
pub mod suggestion;

pub use asset_kind::AssetKind;
pub use error::{ApiError, ErrorBody, ErrorCode};
pub use format::{FormatError, FormatString, Placeholder, PlaceholderValues};
pub use suggestion::{
    DependentOperation, ExecutionReport, ExecutionStatus, Provenance, ScanSummary, SessionState,
    Suggestion, SuggestionStatus, SuggestionType,
};
