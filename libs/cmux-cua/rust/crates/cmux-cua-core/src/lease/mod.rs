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
    "manaflow-ai/cmux@f3f431f4206:schemas/automation-lease/vectors.json";

/// SHA-256 of the vendored `vectors.json`, equal to the file at
/// [`VECTORS_SOURCE`]. A test fails when the copy drifts.
pub const VECTORS_SHA256: &str = "003c49f1a7ff9793652a544929b982116f9605f6993d4213340949a87f13f987";

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
    /// The caller named no session; the host substituted its default.
    pub implicit_session: bool,
}

impl AgentIdentity {
    /// The principal a user stop applies to.
    pub fn stop_key(&self) -> &str {
        self.on_behalf_of.as_deref().unwrap_or(&self.actor)
    }
}

/// The engine behind a target. Provider engines (the person's own tabs)
/// refuse the implicit default session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetEngine {
    Cef,
    Webkit,
    Headless,
    Desktop,
}

impl TargetEngine {
    pub fn requires_explicit_session(self) -> bool {
        matches!(self, Self::Cef | Self::Webkit)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeaseOp {
    /// Take the lease without acting.
    Acquire { target: String, engine: TargetEngine, who: AgentIdentity, now_ms: u64 },
    /// Any input to the target (click, type, key, scroll, drag, set value).
    Act { target: String, engine: TargetEngine, who: AgentIdentity, now_ms: u64 },
    /// Any read (snapshot, screenshot). Never blocked, never leases.
    Observe { target: String, session: String },
    Release { target: String, session: String },
    SessionEnd { session: String },
    /// A person used the target (signal from the app or the helper).
    UserInput { target: String },
    TakeOver { target: String, origin: String },
    HandBack { target: String, origin: String },
    Stop { target: String, origin: String },
    /// Lift a stop for one principal (`on_behalf_of`, else `actor`).
    Allow { actor: String, origin: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseError {
    LeaseHeld,
    PausedByUser,
    UserDriving,
    StaleAfterHandBack,
    StoppedByUser,
    SessionRequired,
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
            Self::SessionRequired => "session_required",
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
            Self::StoppedByUser => "the person stopped this agent; wait until they allow it again",
            Self::SessionRequired => "name a session to drive the person's own tabs",
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

    /// Whether the user stopped this principal (`on_behalf_of`, else `actor`).
    pub fn is_stopped(&self, principal: &str) -> bool {
        self.stopped.contains(principal)
    }

    /// Applies one operation and returns its result plus one frame for each
    /// target whose rendered lease changed.
    pub fn apply(&mut self, op: LeaseOp) -> LeaseOutcome {
        let affected = self.affected_targets(&op);
        let before: Vec<Option<Lease>> = affected.iter().map(|t| self.lease(t).cloned()).collect();
        let result = self.reduce(op);
        let frames = affected
            .into_iter()
            .zip(before)
            .filter_map(|(target, before)| {
                let after = self.lease(&target).cloned();
                (after != before).then_some(LeaseFrame { target, lease: after })
            })
            .collect();
        LeaseOutcome { result, frames }
    }

    fn affected_targets(&self, op: &LeaseOp) -> Vec<String> {
        match op {
            LeaseOp::SessionEnd { session } => self
                .leases
                .iter()
                .filter(|(_, entry)| &entry.lease.session == session)
                .map(|(target, _)| target.clone())
                .collect(),
            LeaseOp::Allow { .. } => Vec::new(),
            LeaseOp::Acquire { target, .. }
            | LeaseOp::Act { target, .. }
            | LeaseOp::Observe { target, .. }
            | LeaseOp::Release { target, .. }
            | LeaseOp::UserInput { target }
            | LeaseOp::TakeOver { target, .. }
            | LeaseOp::HandBack { target, .. }
            | LeaseOp::Stop { target, .. } => vec![target.clone()],
        }
    }

    fn reduce(&mut self, op: LeaseOp) -> Result<(), LeaseError> {
        match op {
            LeaseOp::Acquire { target, engine, who, now_ms } => self.drive(target, engine, who, now_ms, false),
            LeaseOp::Act { target, engine, who, now_ms } => self.drive(target, engine, who, now_ms, true),
            LeaseOp::Observe { target, session } => {
                if let Some(entry) = self.leases.get_mut(&target) {
                    if entry.lease.session == session && entry.lease.state == LeaseState::Driving {
                        entry.needs_fresh_observe = false;
                    }
                }
                Ok(())
            }
            LeaseOp::Release { target, session } => match self.leases.get(&target) {
                None => Ok(()),
                Some(entry) if entry.lease.session != session => Err(LeaseError::NotLeaseHolder),
                Some(_) => {
                    self.leases.remove(&target);
                    Ok(())
                }
            },
            LeaseOp::SessionEnd { session } => {
                self.leases.retain(|_, entry| entry.lease.session != session);
                Ok(())
            }
            LeaseOp::UserInput { target } => {
                if let Some(entry) = self.leases.get_mut(&target) {
                    if entry.lease.state == LeaseState::Driving {
                        entry.lease.state = LeaseState::Paused;
                    }
                }
                Ok(())
            }
            LeaseOp::TakeOver { target, origin } => {
                require_user(&origin)?;
                let entry = self.leases.get_mut(&target).ok_or(LeaseError::NoLease)?;
                entry.lease.state = LeaseState::UserDriving;
                Ok(())
            }
            LeaseOp::HandBack { target, origin } => {
                require_user(&origin)?;
                let entry = self.leases.get_mut(&target).ok_or(LeaseError::NoLease)?;
                if entry.lease.state == LeaseState::Driving {
                    return Err(LeaseError::NotPaused);
                }
                entry.lease.state = LeaseState::Driving;
                entry.needs_fresh_observe = true;
                Ok(())
            }
            LeaseOp::Stop { target, origin } => {
                require_user(&origin)?;
                let entry = self.leases.remove(&target).ok_or(LeaseError::NoLease)?;
                let principal = entry.lease.on_behalf_of.unwrap_or(entry.lease.actor);
                self.stopped.insert(principal);
                Ok(())
            }
            LeaseOp::Allow { actor, origin } => {
                require_user(&origin)?;
                self.stopped.remove(&actor);
                Ok(())
            }
        }
    }

    /// `acquire` (is_act = false) and `act` (is_act = true).
    fn drive(
        &mut self,
        target: String,
        engine: TargetEngine,
        who: AgentIdentity,
        now_ms: u64,
        is_act: bool,
    ) -> Result<(), LeaseError> {
        if who.origin == USER_ORIGIN {
            return Err(LeaseError::AgentOriginRequired);
        }
        if who.implicit_session && engine.requires_explicit_session() {
            return Err(LeaseError::SessionRequired);
        }
        if self.stopped.contains(who.stop_key()) {
            return Err(LeaseError::StoppedByUser);
        }
        let Some(entry) = self.leases.get(&target) else {
            let lease = Lease {
                session: who.session,
                actor: who.actor,
                on_behalf_of: who.on_behalf_of,
                origin: who.origin,
                label: who.label,
                since_ms: now_ms,
                state: LeaseState::Driving,
            };
            self.leases.insert(target, Entry { lease, needs_fresh_observe: false });
            return Ok(());
        };
        if entry.lease.session != who.session {
            return Err(LeaseError::LeaseHeld);
        }
        if !is_act {
            return Ok(());
        }
        match entry.lease.state {
            LeaseState::Paused => Err(LeaseError::PausedByUser),
            LeaseState::UserDriving => Err(LeaseError::UserDriving),
            LeaseState::Driving if entry.needs_fresh_observe => Err(LeaseError::StaleAfterHandBack),
            LeaseState::Driving => Ok(()),
        }
    }
}

fn require_user(origin: &str) -> Result<(), LeaseError> {
    if origin == USER_ORIGIN {
        Ok(())
    } else {
        Err(LeaseError::UserOriginRequired)
    }
}
