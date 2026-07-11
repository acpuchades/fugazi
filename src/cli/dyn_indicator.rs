//! Runtime-typed indicator vocabulary — now lives in the library at
//! [`fugazi::runtime`]. This facade preserves the historical
//! `crate::dyn_indicator::*` path for CLI spec-builder callsites so no
//! individual builder needs to know the vocabulary moved.

pub use fugazi::runtime::*;
