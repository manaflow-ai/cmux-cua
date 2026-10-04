//! Automation lease: who drives a window or app, and whether the person
//! paused, took over or stopped that agent.
//!
//! Contract: `plans/cmux-next/automation-lease.md` in manaflow-ai/cmux. The
//! cmux browser host implements the same state machine for browser tabs;
//! both replay the shared vectors in `vectors.json` (vendored, see
//! [`VECTORS_SOURCE`]). The host owns this state; the cmux app only renders
//! the [`LeaseFrame`]s it receives.
//!
//! The table is a pure reducer: no I/O, no clock. Callers stamp identity
//! (session, actor, origin) from the connection, never from the request.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::{Deserialize, Serialize};

#[cfg(test)]
mod tests;

/// Where the vendored `vectors.json` comes from. Update both together.
pub const VECTORS_SOURCE: &str =
    "manaflow-ai/cmux@6ba78d24069:schemas/automation-lease/vectors.json";

/// The origin value of the person's own authenticated client.
pub const USER_ORIGIN: &str = "user";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaseState {
    Driving,
    Paused,
    UserDriving,
}

/// The rendered lease: exactly what a `lease` frame carries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lease {
    pub session: String,
    pub actor: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_behalf_of: Option<String>,
    pub origin: String,
    pub label: String,
    pub since_ms: u64,
    pub state: LeaseState,
}

/// Identity of an agent request, stamped from the connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentIdentity {
    pub session: String,
    pub actor: String,
    pub on_behalf_of: Option<String>,
    pub origin: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeaseOp {
    /// Take the lease without acting.
    Acquire { target: String, who: AgentIdentity, now_ms: u64 },
    /// Any input to the target (click, type, key, scroll, drag, set value).
    Act { target: String, who: AgentIdentity, now_ms: u64 },
    /// Any read (snapshot, screenshot). Never blocked, never leases.
    Observe { target: String, session: String },
    Release { target: String, session: String },
    SessionEnd { session: String },
    /// A person used the target (signal from the app or the helper).
    UserInput { target: String },
    TakeOver { target: String, origin: String },
    HandBack { target: String, origin: String },
    Stop { target: String, origin: String },
    Allow { session: String, origin: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseError {
    LeaseHeld,
    PausedByUser,
    UserDriving,
    StaleAfterHandBack,
    StoppedByUser,
    NotLeaseHolder,
    NoLease,
    NotPaused,
    UserOriginRequired,
    AgentOriginRequired,
}

impl LeaseError {
    /// The wire code shared with the browser host.
    pub fn code(self) -> &'static str {
        match self {
            Self::LeaseHeld => "lease_held",
            Self::PausedByUser => "paused_by_user",
            Self::UserDriving => "user_driving",
            Self::StaleAfterHandBack => "stale_after_hand_back",
            Self::StoppedByUser => "stopped_by_user",
            Self::NotLeaseHolder => "not_lease_holder",
            Self::NoLease => "no_lease",
            Self::NotPaused => "not_paused",
            Self::UserOriginRequired => "user_origin_required",
            Self::AgentOriginRequired => "agent_origin_required",
        }
    }

    fn reason(self) -> &'static str {
        match self {
            Self::LeaseHeld => "another agent session drives this target",
            Self::PausedByUser => "the person used this target; wait for hand back, do not retry",
            Self::UserDriving => "the person took over this target; wait for hand back, do not retry",
            Self::StaleAfterHandBack => "the person handed back control; observe the target before acting",
            Self::StoppedByUser => "the person stopped this session",
            Self::NotLeaseHolder => "this session does not hold the lease",
            Self::NoLease => "no agent holds this target",
            Self::NotPaused => "the agent is already driving this target",
            Self::UserOriginRequired => "only the person's own client may do this",
            Self::AgentOriginRequired => "this is an agent operation",
        }
    }
}

impl fmt::Display for LeaseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code(), self.reason())
    }
}

impl std::error::Error for LeaseError {}

/// One rendered change for the app: `lease: None` clears the badge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseFrame {
    pub target: String,
    pub lease: Option<Lease>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseOutcome {
    pub result: Result<(), LeaseError>,
    pub frames: Vec<LeaseFrame>,
}

#[derive(Debug, Clone)]
struct Entry {
    lease: Lease,
    needs_fresh_observe: bool,
}

#[derive(Debug, Clone, Default)]
pub struct LeaseTable {
    leases: BTreeMap<String, Entry>,
    stopped: BTreeSet<String>,
}

impl LeaseTable {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn lease(&self, target: &str) -> Option<&Lease> {
        self.leases.get(target).map(|entry| &entry.lease)
    }

    pub fn needs_fresh_observe(&self, target: &str) -> bool {
        self.leases.get(target).is_some_and(|entry| entry.needs_fresh_observe)
    }

    pub fn is_stopped(&self, session: &str) -> bool {
        self.stopped.contains(session)
    }

    pub fn apply(&mut self, op: LeaseOp) -> LeaseOutcome {
        let _ = op;
        unimplemented!("automation lease reducer")
    }
}
