//! Common functionality for the sentry relay.
#![warn(missing_docs)]

#[macro_use]
mod macros;

#[macro_use]
pub mod metrics;

mod glob;
mod log;
mod retry;
mod utils;

pub use crate::glob::*;
pub use crate::log::*;
pub use crate::retry::*;
pub use crate::utils::*;

pub use sentry_types::protocol::LATEST as PROTOCOL_VERSION;
pub use sentry_types::{Auth, AuthParseError, Dsn, DsnParseError, Scheme, Uuid};

/// Represents a project ID.
pub type ProjectId = u64;

/// Raised if a project ID cannot be parsed from a string.
pub type ProjectIdParseError = std::num::ParseIntError;
