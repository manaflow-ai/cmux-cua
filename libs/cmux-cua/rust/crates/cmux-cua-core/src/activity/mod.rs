//! Activity store core: computer use sessions, their event log, redaction
//! and retention. Pure logic, no I/O; the daemon owns persistence.
//!
//! Design: cmux-next `plans/cmux-next/computer-use.md` (manaflow-ai/cmux,
//! branch feat-cmux-next). The CUA host is the single writer of every type
//! here; clients (the cmux Agent activity pane, the CLI, remote viewers) are
//! projections that read records and events and send user operations.

pub mod model;
pub mod reducer;
pub mod redact;
pub mod retention;
pub mod thumbnail;
pub mod host;
pub mod store;
