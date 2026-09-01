//! Mock backends library target.
//!
//! The two binaries (`mock-crm`, `mock-hrm`) import the shared echo server
//! from this library target via the crate name.

pub mod common;

pub use common::serve;
