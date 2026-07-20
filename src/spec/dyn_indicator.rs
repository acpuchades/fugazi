//! Runtime-typed indicator vocabulary — now lives in the library at
//! [`crate::runtime`]. This facade preserves the historical
//! `crate::spec::dyn_indicator::*` path for CLI spec-builder callsites so no
//! individual builder needs to know the vocabulary moved.

pub use crate::runtime::*;
