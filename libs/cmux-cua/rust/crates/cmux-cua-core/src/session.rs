//! Session lifecycle hooks.
//!
//! The cmux-cua daemon (`serve.rs`) drives ONE shared `ToolRegistry`; every
//! `cmux-cua mcp` proxy process connects to it and shares its state. A
//! proxy-minted `session_id` (carried in the daemon request envelope) lets the
//! daemon OWN and CLEAN UP per-session state.
//!
//! Recording ownership lives on the core `RecordingSession` directly. But some
//! session-scoped state is platform-specific (e.g. macOS per-session config
//! overrides in `platform-macos::tools::SessionConfigRegistry`) and the daemon
//! only holds an `Arc<ToolRegistry>` — it can't reach into a platform crate's
//! `ToolState`. This module bridges that gap with a small process-global list
//! of cleanup callbacks: each platform registers a `Fn(&str)` once at startup,
//! and the daemon's `session_end` arm fans the disconnecting `session_id` out
//! to all of them.
//!
//! This mirrors the existing screenshot/AX-snapshot callback pattern in
//! `recording.rs` — a registry-free, platform-pluggable hook set with no
//! reverse coupling from core into the platform crates.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

type SessionEndHook = Box<dyn Fn(&str) + Send + Sync>;
type SessionReviveHook = Box<dyn Fn(&str) + Send + Sync>;

static SESSION_END_HOOKS: OnceLock<Mutex<Vec<SessionEndHook>>> = OnceLock::new();
static SESSION_REVIVE_HOOKS: OnceLock<Mutex<Vec<SessionReviveHook>>> = OnceLock::new();

/// Last-activity timestamp per live session id. A session is "touched" every
/// time a tool call carries its explicit `session` id (see the daemon boundary
/// in `serve.rs`). The idle-TTL sweep ([`evict_idle`]) ends sessions that
/// haven't been touched within the TTL — this is the cleanup path that replaces
/// connection-EOF reaping now that a session is a caller-declared identity, not
/// a per-MCP-connection one. `"default"` and empty ids are never tracked (they
/// are the anonymous, cursor-less fallback).
static SESSION_ACTIVITY: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();

fn activity() -> &'static Mutex<HashMap<String, Instant>> {
    SESSION_ACTIVITY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Whether `id` is a real, trackable session id (not the anonymous fallback).
fn is_trackable(id: &str) -> bool {
    !id.is_empty() && id != "default"
}

/// Session ids held by a live control connection, with a holder count. The
/// idle-TTL sweep exists to reclaim sessions whose owner vanished; a session
/// whose proxy still holds its control connection has a live owner, so the
/// sweep must not end it however long the agent pauses between calls. The
/// control-connection EOF releases the hold and reaps the session itself.
static HELD_SESSIONS: OnceLock<Mutex<HashMap<String, usize>>> = OnceLock::new();

/// A lease is the daemon-owned identity for one embedding host and tool
/// profile. The MCP proxy process is allowed to disappear and reconnect with a
/// newer generation without tearing down the host's cursor or recording.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeaseRecord {
    pub lease_id: String,
    pub profile: String,
    pub session_id: String,
    pub generation: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LeaseAcquire {
    New,
    Resumed,
    Replaced { previous_session_id: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExpiredLease {
    pub lease_id: String,
    pub profile: String,
    pub session_id: String,
    pub generation: u64,
}

#[derive(Clone, Debug)]
struct LeaseEntry {
    record: LeaseRecord,
    holders: usize,
    last_renewed: Instant,
    disconnected_at: Option<Instant>,
}

static LEASES: OnceLock<Mutex<HashMap<String, LeaseEntry>>> = OnceLock::new();

/// Persisted generation high-water mark. The active bit lets a daemon restart
/// resume the exact live generation while still rejecting that same generation
/// after the lease was deliberately expired.
#[derive(Clone, Debug, Deserialize, Serialize)]
struct LeaseWatermark {
    profile: String,
    session_id: String,
    generation: u64,
    active: bool,
}

static LEASE_WATERMARKS: OnceLock<Mutex<HashMap<String, LeaseWatermark>>> = OnceLock::new();
static LEASE_WATERMARK_LOAD_ERROR: OnceLock<Option<String>> = OnceLock::new();
static LEASE_TRANSITION: OnceLock<Mutex<()>> = OnceLock::new();
static LEASE_PERSIST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn held() -> &'static Mutex<HashMap<String, usize>> {
    HELD_SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn leases() -> &'static Mutex<HashMap<String, LeaseEntry>> {
    LEASES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn lease_transition() -> &'static Mutex<()> {
    LEASE_TRANSITION.get_or_init(|| Mutex::new(()))
}

fn lease_watermarks() -> &'static Mutex<HashMap<String, LeaseWatermark>> {
    LEASE_WATERMARKS.get_or_init(|| {
        let (watermarks, error) = load_lease_watermarks();
        let _ = LEASE_WATERMARK_LOAD_ERROR.set(error);
        Mutex::new(watermarks)
    })
}

fn lease_watermark_load_error() -> Option<String> {
    let _ = lease_watermarks();
    LEASE_WATERMARK_LOAD_ERROR
        .get()
        .and_then(|error| error.clone())
}

fn lease_watermark_path() -> Option<PathBuf> {
    std::env::var_os(crate::session_state::STATE_DIR_ENV)
        .map(PathBuf::from)
        .map(|dir| dir.join("leases-v1.json"))
}

fn load_lease_watermarks() -> (HashMap<String, LeaseWatermark>, Option<String>) {
    let Some(path) = lease_watermark_path() else {
        return (HashMap::new(), None);
    };
    load_lease_watermarks_at(&path)
}

fn load_lease_watermarks_at(path: &Path) -> (HashMap<String, LeaseWatermark>, Option<String>) {
    let body = match std::fs::read(&path) {
        Ok(body) => body,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return (HashMap::new(), None);
        }
        Err(error) => {
            return (
                HashMap::new(),
                Some(format!("cannot read {}: {error}", path.display())),
            );
        }
    };
    let stored: HashMap<String, LeaseWatermark> = match serde_json::from_slice(&body) {
        Ok(stored) => stored,
        Err(error) => {
            return (
                HashMap::new(),
                Some(format!("cannot decode {}: {error}", path.display())),
            );
        }
    };
    (normalize_lease_watermarks(stored), None)
}

fn normalize_lease_watermarks(
    stored: HashMap<String, LeaseWatermark>,
) -> HashMap<String, LeaseWatermark> {
    let mut normalized: HashMap<String, LeaseWatermark> = HashMap::new();
    for (key, watermark) in stored {
        // Older lease files keyed records by profile and lease id. The lease
        // identity is global, matching LEASES; retaining the profile in the
        // lookup key would let another profile take ownership after restart.
        let prefix = format!("{}\0", watermark.profile);
        let lease_id = key.strip_prefix(&prefix).unwrap_or(&key).to_owned();
        if let Some(previous) = normalized.get_mut(&lease_id) {
            let conflicting_profile = previous.profile != watermark.profile;
            if watermark.generation > previous.generation {
                *previous = watermark;
            }
            // A file written before profile fencing may contain conflicting
            // owners. Require a newer generation instead of resuming either.
            if conflicting_profile {
                previous.active = false;
            }
        } else {
            normalized.insert(lease_id, watermark);
        }
    }
    normalized
}

fn persist_lease_watermarks(
    watermarks: &HashMap<String, LeaseWatermark>,
) -> Result<(), String> {
    let Some(path) = lease_watermark_path() else {
        return Ok(());
    };
    persist_lease_watermarks_at(&path, watermarks)
}

fn persist_lease_watermarks_at(
    path: &Path,
    watermarks: &HashMap<String, LeaseWatermark>,
) -> Result<(), String> {
    let Some(dir) = path.parent() else {
        return Err(format!("lease watermark path has no parent: {}", path.display()));
    };
    crate::session_state::ensure_private_dir(dir)
        .map_err(|error| format!("cannot create lease watermark directory: {error}"))?;
    let body = serde_json::to_vec(watermarks)
        .map_err(|error| format!("cannot encode lease watermarks: {error}"))?;
    let sequence = LEASE_PERSIST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temp_path = dir.join(format!(".leases-v1.json.tmp-{sequence}"));
    let result = (|| {
        use std::io::Write;
        let mut file = crate::session_state::create_private_temp_file(&temp_path)?;
        file.write_all(&body)?;
        file.sync_all()?;
        std::fs::rename(&temp_path, &path)
    })();
    if let Err(error) = result {
        let _ = std::fs::remove_file(temp_path);
        return Err(format!("cannot persist lease watermarks: {error}"));
    }
    Ok(())
}

fn validate_lease_part(value: &str, field: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err(format!("lease {field} must not be empty"));
    }
    if value.len() > 256 {
        return Err(format!("lease {field} is too long"));
    }
    Ok(())
}

/// Acquire or resume a daemon-owned lease. A newer generation fences the old
/// proxy and keeps the same host/profile lease alive. An older generation is
/// rejected so a delayed reconnect cannot regain ownership after a restart.
pub fn acquire_lease(
    lease_id: &str,
    profile: &str,
    session_id: &str,
    generation: u64,
) -> Result<LeaseAcquire, String> {
    validate_lease_part(lease_id, "id")?;
    validate_lease_part(profile, "profile")?;
    validate_lease_part(session_id, "session")?;
    if generation == 0 {
        return Err("lease generation must be positive".to_owned());
    }
    if let Some(error) = lease_watermark_load_error() {
        return Err(format!(
            "lease watermark state is unavailable; refusing lease admission: {error}"
        ));
    }

    let _transition = lease_transition().lock().unwrap();
    let now = Instant::now();
    let mut map = leases().lock().unwrap();
    let mut watermarks = lease_watermarks().lock().unwrap();
    let record = LeaseRecord {
        lease_id: lease_id.to_owned(),
        profile: profile.to_owned(),
        session_id: session_id.to_owned(),
        generation,
    };
    let Some(entry) = map.get_mut(lease_id) else {
        if let Some(previous) = watermarks.get(lease_id) {
            if previous.profile != profile {
                return Err(format!(
                    "lease `{lease_id}` belongs to profile `{}`, not `{profile}`",
                    previous.profile
                ));
            }
            if generation < previous.generation {
                return Err(format!(
                    "stale lease generation {generation}; active generation is {}",
                    previous.generation
                ));
            }
            if generation == previous.generation {
                if previous.active && previous.session_id == session_id {
                    map.insert(
                        lease_id.to_owned(),
                        LeaseEntry {
                            record,
                            holders: 1,
                            last_renewed: now,
                            disconnected_at: None,
                        },
                    );
                    return Ok(LeaseAcquire::Resumed);
                }
                return Err(format!(
                    "stale lease generation {generation}; active generation is {}",
                    previous.generation
                ));
            }
        }
        let mut next_watermarks = watermarks.clone();
        next_watermarks.insert(
            lease_id.to_owned(),
            LeaseWatermark {
                profile: profile.to_owned(),
                session_id: session_id.to_owned(),
                generation,
                active: true,
            },
        );
        persist_lease_watermarks(&next_watermarks)?;
        map.insert(
            lease_id.to_owned(),
            LeaseEntry {
                record: record.clone(),
                holders: 1,
                last_renewed: now,
                disconnected_at: None,
            },
        );
        *watermarks = next_watermarks;
        return Ok(LeaseAcquire::New);
    };
    if entry.record.profile != profile {
        return Err(format!(
            "lease `{lease_id}` belongs to profile `{}`, not `{profile}`",
            entry.record.profile
        ));
    }
    if generation < entry.record.generation {
        return Err(format!(
            "stale lease generation {generation}; active generation is {}",
            entry.record.generation
        ));
    }
    if generation == entry.record.generation {
        if entry.record.session_id != session_id {
            return Err(format!(
                "lease generation {generation} is already owned by another session"
            ));
        }
        entry.holders = entry.holders.max(1);
        entry.last_renewed = now;
        entry.disconnected_at = None;
        return Ok(LeaseAcquire::Resumed);
    }

    let mut next_watermarks = watermarks.clone();
    next_watermarks.insert(
        lease_id.to_owned(),
        LeaseWatermark {
            profile: profile.to_owned(),
            session_id: session_id.to_owned(),
            generation,
            active: true,
        },
    );
    persist_lease_watermarks(&next_watermarks)?;
    let previous_session_id = std::mem::replace(&mut entry.record, record).session_id;
    entry.holders = 1;
    entry.last_renewed = now;
    entry.disconnected_at = None;
    *watermarks = next_watermarks;
    Ok(LeaseAcquire::Replaced {
        previous_session_id,
    })
}

/// Renew a live lease. The generation check is the fencing boundary shared by
/// all proxy generations and prevents a delayed heartbeat from reviving an old
/// owner.
pub fn renew_lease(
    lease_id: &str,
    profile: &str,
    session_id: &str,
    generation: u64,
) -> Result<(), String> {
    let _transition = lease_transition().lock().unwrap();
    let mut map = leases().lock().unwrap();
    let Some(entry) = map.get_mut(lease_id) else {
        return Err(format!("lease `{lease_id}` is not active"));
    };
    if entry.record.profile != profile
        || entry.record.session_id != session_id
        || entry.record.generation != generation
    {
        return Err("stale or mismatched lease renewal".to_owned());
    }
    entry.last_renewed = Instant::now();
    entry.disconnected_at = None;
    Ok(())
}

/// Mark a control connection as gone. Cleanup is deferred until the crash
/// grace period expires, allowing a proxy restart to resume its lease.
pub fn release_lease(lease_id: &str, generation: u64) -> Result<(), String> {
    let _transition = lease_transition().lock().unwrap();
    let mut map = leases().lock().unwrap();
    let Some(entry) = map.get_mut(lease_id) else {
        return Ok(());
    };
    if entry.record.generation != generation {
        return Err("stale lease release".to_owned());
    }
    entry.holders = entry.holders.saturating_sub(1);
    if entry.holders == 0 {
        entry.disconnected_at = Some(Instant::now());
    }
    Ok(())
}

/// Return leases whose owner has exceeded the disconnect grace period or whose
/// heartbeat has gone stale. The caller owns the actual session cleanup hook.
pub fn expire_leases(grace: Duration, ttl: Duration) -> Vec<ExpiredLease> {
    expire_leases_at(Instant::now(), grace, ttl)
}

fn expire_leases_at(now: Instant, grace: Duration, ttl: Duration) -> Vec<ExpiredLease> {
    if lease_watermark_load_error().is_some() {
        return Vec::new();
    }
    let _transition = lease_transition().lock().unwrap();
    let mut map = leases().lock().unwrap();
    let expired_ids: Vec<String> = map
        .iter()
        .filter(|(_, entry)| {
            if entry.holders > 0 {
                return now.duration_since(entry.last_renewed) >= ttl;
            }
            entry
                .disconnected_at
                .is_some_and(|at| now.duration_since(at) >= grace)
        })
        .map(|(id, _)| id.clone())
        .collect();
    let expired = expired_ids
        .iter()
        .filter_map(|id| map.get(id).map(|entry| ExpiredLease {
            lease_id: entry.record.lease_id.clone(),
            profile: entry.record.profile.clone(),
            session_id: entry.record.session_id.clone(),
            generation: entry.record.generation,
        }))
        .collect::<Vec<_>>();
    if !expired.is_empty() {
        let mut watermarks = lease_watermarks().lock().unwrap();
        let mut next_watermarks = watermarks.clone();
        for lease in &expired {
            if let Some(watermark) = next_watermarks.get_mut(&lease.lease_id) {
                watermark.active = false;
            }
        }
        if lease_watermark_path().is_some()
            && expired
                .iter()
                .any(|lease| !next_watermarks.contains_key(&lease.lease_id))
        {
            return Vec::new();
        }
        // Keep the lease active if its tombstone cannot be made durable. A
        // subsequent sweep retries the write; removing it first would allow a
        // delayed proxy to reacquire the expired generation in this process.
        if persist_lease_watermarks(&next_watermarks).is_err() {
            return Vec::new();
        }
        for id in expired_ids {
            map.remove(&id);
        }
        *watermarks = next_watermarks;
    }
    expired
}

/// Run cleanup for an expired lease while excluding a replacement that may
/// have acquired the same lease id between expiry and the sweep callback. The
/// transition lock stays held through `cleanup`, so stable host state cannot be
/// removed after a replacement has started writing it.
pub fn with_expired_lease_cleanup(
    expired: &ExpiredLease,
    cleanup: impl FnOnce(bool),
) {
    let _transition = lease_transition().lock().unwrap();
    let replacement_active = leases().lock().unwrap().contains_key(&expired.lease_id);
    cleanup(!replacement_active);
}

/// Whether a session id belongs to a generation that has been superseded or
/// expired. This also consults persisted host high-water marks so a daemon
/// restart cannot let an older proxy issue calls before its session_begin is
/// rejected.
pub fn is_lease_session_fenced(session_id: &str, profile: &str) -> bool {
    if lease_watermark_load_error().is_some() {
        return true;
    }
    let _transition = lease_transition().lock().unwrap();
    let lease_id = crate::session_state::durable_lease_id(session_id);
    let generation = crate::session_state::lease_generation(session_id);
    let watermarks = lease_watermarks().lock().unwrap();
    watermarks.get(&lease_id).is_some_and(|watermark| {
        watermark.profile != profile
            || generation < watermark.generation
            || (generation == watermark.generation
                && (!watermark.active || watermark.session_id != session_id))
    })
}

/// Fence and clean up a superseded proxy generation without disturbing the
/// stable host lease. The replacement generation owns the shared host scope.
pub fn retire_session(session_id: &str) {
    activity().lock().unwrap().remove(session_id);
    held().lock().unwrap().remove(session_id);
    fire_session_end(session_id);
}

/// Mark `session_id` as owned by a live control connection.
pub fn hold_session(session_id: &str) {
    if !is_trackable(session_id) {
        return;
    }
    *held()
        .lock()
        .unwrap()
        .entry(session_id.to_owned())
        .or_insert(0) += 1;
}

/// Release one control-connection hold on `session_id`.
pub fn release_session(session_id: &str) {
    let mut map = held().lock().unwrap();
    if let Some(count) = map.get_mut(session_id) {
        *count -= 1;
        if *count == 0 {
            map.remove(session_id);
        }
    }
}

/// Whether a live control connection currently holds `session_id`.
pub fn is_session_held(session_id: &str) -> bool {
    held().lock().unwrap().contains_key(session_id)
}

/// Session ids that have already had their `session_end` fired. Dedupes the
/// control-connection EOF teardown (the reaper) against any stray legacy
/// `session_end` method that a mixed-version (new proxy / old proxy) rollout
/// might still send — `fire_session_end` is the single fan-out point and must
/// be idempotent because the overlay Remove + recording stop must run exactly
/// once. Growth is bounded (one short string per ended session over the
/// daemon's lifetime); eviction is a deliberate non-blocking follow-up.
static ENDED_SESSIONS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

fn hooks() -> &'static Mutex<Vec<SessionEndHook>> {
    SESSION_END_HOOKS.get_or_init(|| Mutex::new(Vec::new()))
}

fn revive_hooks() -> &'static Mutex<Vec<SessionReviveHook>> {
    SESSION_REVIVE_HOOKS.get_or_init(|| Mutex::new(Vec::new()))
}

fn ended_sessions() -> &'static Mutex<HashSet<String>> {
    ENDED_SESSIONS.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Register a callback invoked with the disconnecting `session_id` whenever a
/// session ends (graceful proxy EOF → daemon `session_end`). Each platform
/// registers its session-scoped cleanup here once at startup. Idempotency and
/// "unknown session id" tolerance are the hook's responsibility — `session_end`
/// fires once per proxy exit, but a hook should treat a clear of an unseen id
/// as a no-op.
pub fn register_session_end_hook(hook: impl Fn(&str) + Send + Sync + 'static) {
    hooks().lock().unwrap().push(Box::new(hook));
}

/// Register a callback for an explicit reuse of an ended session id. Platform
/// cursor overlays use this to order a `Revive` event after their prior
/// `Remove`, preserving the late-command guard without making the tombstone
/// permanent.
pub fn register_session_revive_hook(hook: impl Fn(&str) + Send + Sync + 'static) {
    revive_hooks().lock().unwrap().push(Box::new(hook));
}

/// Fan a session-end out to every registered cleanup hook. Called by the daemon
/// on control-connection EOF (the reaper) and by the legacy `session_end` method
/// arm. Idempotent: the FIRST fire for a given `session_id` runs every hook; any
/// later fire for the same id is a no-op. This dedupes the EOF path against a
/// stray legacy `session_end` (mixed-version rollout) so cursor-remove +
/// recording-stop run exactly once. No-op when no hooks are registered.
pub fn fire_session_end(session_id: &str) {
    // Mark-then-fan-out under a short critical section, releasing the lock
    // before running hooks (hooks may be slow / re-entrant and must not hold
    // the dedupe lock).
    {
        let mut ended = ended_sessions().lock().unwrap();
        if !ended.insert(session_id.to_owned()) {
            return; // already ended — idempotent no-op.
        }
    }
    for hook in hooks().lock().unwrap().iter() {
        hook(session_id);
    }
}

/// Whether `fire_session_end` has already run for this `session_id`. This is
/// the daemon-side late-action guard until an explicit [`revive_session`] call;
/// platform overlays keep an ordered render-side tombstone keyed on the same id.
pub fn is_session_ended(session_id: &str) -> bool {
    ended_sessions().lock().unwrap().contains(session_id)
}

/// Run an ordered lifecycle operation only while `session_id` is live.
///
/// The ended-session lock stays held through `operation`, so a concurrent
/// [`fire_session_end`] cannot mark the session and enqueue its cleanup between
/// the live check and the operation. This is intended for short, non-blocking
/// lifecycle queue writes such as cursor revival.
pub fn with_live_session<R>(session_id: &str, operation: impl FnOnce() -> R) -> Option<R> {
    if !is_trackable(session_id) {
        return Some(operation());
    }
    let ended = ended_sessions().lock().unwrap();
    if ended.contains(session_id) {
        return None;
    }
    Some(operation())
}

/// Revive a previously-ended session id by clearing its tombstone, so a fresh
/// `start_session` with a recycled id works as a caller would expect: the id
/// becomes live again and its actions stop being rejected by the resurrection
/// guard. Returns whether the id had actually been ended (i.e. was revived).
///
/// This is the deliberate, EXPLICIT counterpart to the resurrection guard. The
/// guard exists so a *stray late action* on a dead id can't silently re-create
/// session-owned state; reviving requires an explicit `start_session` re-declare
/// of the same id, which is exactly what a caller reusing an id intends. No-op
/// for the anonymous fallback (`"default"` / empty), which is never tracked.
pub fn revive_session(session_id: &str) -> bool {
    if !is_trackable(session_id) {
        return false;
    }
    // Serialize revivals through the hook lock, but do not hold the ended-set
    // lock while invoking callbacks. The tombstone remains present while hooks
    // enqueue their ordered lifecycle events, so concurrent actions still see
    // the session as ended. Reentrant hooks may safely query that state.
    let hooks = revive_hooks().lock().unwrap();
    if !ended_sessions().lock().unwrap().contains(session_id) {
        return false;
    }
    for hook in hooks.iter() {
        hook(session_id);
    }
    ended_sessions().lock().unwrap().remove(session_id)
}

/// Record activity for an explicit session id, resetting its idle-TTL clock.
/// Called at the daemon boundary on every tool call that carries an explicit
/// `session`. No-op for the anonymous fallback (`"default"` / empty) and for a
/// session that has already ended (so a late in-flight call can't resurrect a
/// reaped session's TTL entry).
pub fn touch_session(session_id: &str) {
    if !is_trackable(session_id) || is_session_ended(session_id) {
        return;
    }
    activity()
        .lock()
        .unwrap()
        .insert(session_id.to_owned(), Instant::now());
}

/// End a session explicitly (the `end_session` tool / `session end` CLI verb):
/// drop its idle-TTL entry and fan `fire_session_end` out to every cleanup hook
/// (overlay remove, recording stop, config-override clear). Idempotent via
/// `fire_session_end`'s dedupe. No-op for the anonymous fallback.
pub fn end_session(session_id: &str) {
    if !is_trackable(session_id) {
        return;
    }
    activity().lock().unwrap().remove(session_id);
    fire_session_end(session_id);
}

/// End every session whose last activity is older than `ttl`, returning the ids
/// ended. This is the idle-TTL sweep the daemon runs periodically: a
/// caller-declared session is no longer tied to a connection's lifetime, so a
/// run that finishes (or crashes) without calling `end_session` is reclaimed
/// here instead of leaking its cursor / recording. Sessions touched within the
/// TTL are left untouched.
pub fn evict_idle(ttl: Duration) -> Vec<String> {
    let now = Instant::now();
    let stale: Vec<String> = {
        let map = activity().lock().unwrap();
        let held = held().lock().unwrap();
        map.iter()
            .filter(|(id, last)| now.duration_since(**last) >= ttl && !held.contains_key(*id))
            .map(|(id, _)| id.clone())
            .collect()
    };
    for id in &stale {
        end_session(id);
    }
    stale
}

/// Number of sessions with a live idle-TTL entry. Diagnostics only.
pub fn active_session_count() -> usize {
    activity().lock().unwrap().len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn held_session_survives_idle_sweep_until_released() {
        let sid = "test-held-session-5F3A";
        touch_session(sid);
        hold_session(sid);
        assert!(evict_idle(Duration::ZERO).iter().all(|id| id != sid));
        assert!(!is_session_ended(sid));
        release_session(sid);
        assert!(!is_session_held(sid));
        assert!(evict_idle(Duration::ZERO).iter().any(|id| id == sid));
        assert!(is_session_ended(sid));
    }

    #[test]
    fn fire_session_end_is_idempotent_per_id() {
        // Distinct, test-local ids so we don't collide with other tests that
        // share the process-global ENDED_SESSIONS set.
        let sid = "test-dedupe-session-AABBCC";
        let calls = Arc::new(AtomicUsize::new(0));
        let calls2 = calls.clone();
        let want = sid.to_owned();
        register_session_end_hook(move |got| {
            if got == want {
                calls2.fetch_add(1, Ordering::Relaxed);
            }
        });

        assert!(!is_session_ended(sid));
        fire_session_end(sid);
        assert!(is_session_ended(sid));
        // Second + third fire for the same id must be no-ops.
        fire_session_end(sid);
        fire_session_end(sid);
        assert_eq!(
            calls.load(Ordering::Relaxed),
            1,
            "hook must run exactly once for a given session id"
        );
    }

    #[test]
    fn touch_then_evict_by_ttl() {
        let sid = "test-ttl-session-DDEEFF";
        touch_session(sid);
        // A huge TTL leaves it alone (just touched).
        assert!(
            evict_idle(Duration::from_secs(3600))
                .iter()
                .all(|s| s != sid)
        );
        // A zero TTL treats any prior activity as idle → evicts it.
        let evicted = evict_idle(Duration::ZERO);
        assert!(
            evicted.iter().any(|s| s == sid),
            "zero-TTL must evict a touched session"
        );
        assert!(is_session_ended(sid), "evicted session is ended");
    }

    #[test]
    fn anonymous_ids_are_never_tracked() {
        touch_session("default");
        touch_session("");
        // Neither shows up under a zero-TTL sweep (they were never inserted).
        let evicted = evict_idle(Duration::ZERO);
        assert!(!evicted.iter().any(|s| s == "default" || s.is_empty()));
    }

    #[test]
    fn end_session_is_explicit_teardown() {
        let sid = "test-end-session-112233";
        touch_session(sid);
        end_session(sid);
        assert!(is_session_ended(sid));
        // Its TTL entry is gone, so a later sweep doesn't re-fire for it.
        assert!(!evict_idle(Duration::ZERO).iter().any(|s| s == sid));
    }

    #[test]
    fn revive_clears_the_tombstone_for_an_ended_id() {
        let sid = "test-revive-session-445566";
        touch_session(sid);
        end_session(sid);
        assert!(is_session_ended(sid), "ended id is tombstoned");

        // Explicit re-declare revives it: tombstone cleared, returns true.
        assert!(revive_session(sid), "revive reports the id was ended");
        assert!(!is_session_ended(sid), "revived id is live again");

        // Reviving a live (or never-ended) id is a no-op returning false.
        assert!(!revive_session(sid), "reviving a live id is a no-op");
        assert!(!revive_session("test-never-ended-778899"));
    }

    #[test]
    fn revive_notifies_hooks_once_after_an_actual_end() {
        let sid = "test-revive-hook-session-A1B2C3";
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_for_hook = calls.clone();
        let expected = sid.to_owned();
        register_session_revive_hook(move |got| {
            if got == expected {
                calls_for_hook.fetch_add(1, Ordering::Relaxed);
            }
        });

        end_session(sid);
        assert!(revive_session(sid));
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert!(!revive_session(sid));
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn revive_hook_can_reenter_session_state_while_tombstone_is_still_present() {
        let sid = "test-revive-reentrant-session-D4E5F6";
        let saw_ended = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let expected = sid.to_owned();
        let saw_ended_for_hook = saw_ended.clone();
        register_session_revive_hook(move |got| {
            if got == expected {
                saw_ended_for_hook.store(is_session_ended(got), Ordering::Relaxed);
            }
        });
        end_session(sid);
        assert!(revive_session(sid));
        assert!(saw_ended.load(Ordering::Relaxed));
        assert!(!is_session_ended(sid));
    }

    #[test]
    fn concurrent_double_revive_enqueues_hooks_once() {
        let sid = "test-double-revive-session-E5F6A7";
        let calls = Arc::new(AtomicUsize::new(0));
        let entered_hook = Arc::new(std::sync::Barrier::new(2));
        let release_hook = Arc::new(std::sync::Barrier::new(2));
        let expected = sid.to_owned();
        let calls_for_hook = calls.clone();
        let entered_for_hook = entered_hook.clone();
        let release_for_hook = release_hook.clone();
        register_session_revive_hook(move |got| {
            if got == expected {
                calls_for_hook.fetch_add(1, Ordering::Relaxed);
                entered_for_hook.wait();
                release_for_hook.wait();
            }
        });
        end_session(sid);

        let first_sid = sid.to_owned();
        let first = std::thread::spawn(move || revive_session(&first_sid));
        entered_hook.wait();

        let second_sid = sid.to_owned();
        let second = std::thread::spawn(move || revive_session(&second_sid));

        release_hook.wait();
        assert!(first.join().unwrap());
        assert!(!second.join().unwrap());
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn revive_is_noop_for_anonymous_ids() {
        // The anonymous fallback is never tracked, so there is nothing to revive.
        assert!(!revive_session("default"));
        assert!(!revive_session(""));
    }

    #[test]
    fn live_session_operation_orders_before_concurrent_end_cleanup() {
        use std::sync::{Arc, Barrier};

        let sid = "test-live-operation-order-1A2B3C";
        let events = Arc::new(Mutex::new(Vec::new()));
        let hook_events = events.clone();
        register_session_end_hook(move |ended| {
            if ended == sid {
                hook_events.lock().unwrap().push("remove");
            }
        });

        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let worker_events = events.clone();
        let worker_entered = entered.clone();
        let worker_release = release.clone();
        let worker = std::thread::spawn(move || {
            with_live_session(sid, || {
                worker_events.lock().unwrap().push("revive");
                worker_entered.wait();
                worker_release.wait();
            })
        });

        entered.wait();
        let ender = std::thread::spawn(move || fire_session_end(sid));
        release.wait();
        assert!(worker.join().unwrap().is_some());
        ender.join().unwrap();
        assert_eq!(&*events.lock().unwrap(), &["revive", "remove"]);
    }

    #[test]
    fn lease_fences_old_proxy_generations_and_resumes_new_ones() {
        let lease = "test-lease-fence-7A8B9C";
        let first = "test-lease-session-a";
        let second = "test-lease-session-b";
        assert_eq!(
            acquire_lease(lease, "native", first, 1).unwrap(),
            LeaseAcquire::New
        );
        assert!(renew_lease(lease, "native", first, 1).is_ok());
        assert!(renew_lease(lease, "native", first, 0).is_err());
        assert!(acquire_lease(lease, "native", first, 0).is_err());
        assert_eq!(
            acquire_lease(lease, "native", second, 2).unwrap(),
            LeaseAcquire::Replaced {
                previous_session_id: first.to_owned()
            }
        );
        assert!(renew_lease(lease, "native", first, 1).is_err());
        assert!(renew_lease(lease, "native", second, 2).is_ok());
        assert!(release_lease(lease, 2).is_ok());
        assert!(
            expire_leases_at(
                Instant::now() + Duration::from_secs(60),
                Duration::from_secs(1),
                Duration::from_secs(300),
            )
            .iter()
            .any(|expired| expired.lease_id == lease)
        );
    }

    #[test]
    fn lease_rejects_profile_hijack() {
        let lease = "test-lease-profile-D1E2F3";
        assert!(acquire_lease(lease, "native", "test-lease-session-c", 1).is_ok());
        let error = acquire_lease(
            lease,
            "codex-computer-use-compat",
            "test-lease-session-d",
            2,
        )
        .expect_err("one host lease cannot change tool profiles");
        assert!(error.contains("belongs to profile"));
    }

    #[test]
    fn persisted_lease_rejects_profile_hijack_after_restart() {
        let lease = "test-lease-profile-restart-M4N5O6";
        let first = format!("{lease}-mcp-101-100");
        let second = format!("{lease}-mcp-102-101");
        assert!(acquire_lease(lease, "native", &first, 100).is_ok());
        {
            let _transition = lease_transition().lock().unwrap();
            // A new daemon has only the persisted watermark for this lease.
            leases().lock().unwrap().remove(lease);
        }
        let error = acquire_lease(lease, "codex-computer-use-compat", &second, 101)
            .expect_err("a restart must preserve the lease's profile owner");
        assert!(error.contains("belongs to profile"));
        assert_eq!(
            acquire_lease(lease, "native", &first, 100).unwrap(),
            LeaseAcquire::Resumed
        );
    }

    #[test]
    fn lease_tool_fence_rejects_calls_through_another_profile() {
        let lease = "test-lease-profile-tool-fence-P7Q8R9";
        let session = format!("{lease}-mcp-103-200");
        assert!(acquire_lease(lease, "native", &session, 200).is_ok());
        assert!(!is_lease_session_fenced(&session, "native"));
        assert!(
            is_lease_session_fenced(&session, "codex-computer-use-compat"),
            "tool calls must not bypass the profile established at lease admission"
        );
    }

    #[test]
    fn replacing_a_lease_ends_the_retired_generation() {
        let lease = "test-lease-retire-generation-G4H5I6";
        let first = "test-lease-retire-session-a";
        let second = "test-lease-retire-session-b";
        assert!(acquire_lease(lease, "native", first, 10).is_ok());
        match acquire_lease(lease, "native", second, 11).unwrap() {
            LeaseAcquire::Replaced { previous_session_id } => retire_session(&previous_session_id),
            result => panic!("expected replacement, got {result:?}"),
        }
        assert!(
            is_session_ended(first),
            "a fenced generation must be tombstoned before its successor runs"
        );
    }

    #[test]
    fn an_expired_generation_cannot_reacquire_its_lease() {
        let lease = "test-lease-expired-generation-J7K8L9";
        let session = "test-lease-expired-session";
        assert!(acquire_lease(lease, "native", session, 20).is_ok());
        assert!(release_lease(lease, 20).is_ok());
        assert!(
            expire_leases_at(
                Instant::now() + Duration::from_secs(60),
                Duration::from_secs(1),
                Duration::from_secs(300),
            )
            .iter()
            .any(|expired| expired.lease_id == lease)
        );
        assert!(
            acquire_lease(lease, "native", session, 20).is_err(),
            "lease expiry must retain a generation fence for delayed reconnects"
        );
    }

    #[test]
    fn unwritable_lease_watermark_path_fails_closed() {
        let path = std::env::temp_dir().join(format!(
            "cua-lease-watermark-parent-file-{}-{}",
            std::process::id(),
            LEASE_PERSIST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&path, b"parent is a file")
            .expect("create an unwritable watermark parent fixture");
        let watermark_path = path.join("leases-v1.json");
        let error = persist_lease_watermarks_at(&watermark_path, &HashMap::new())
            .expect_err("watermark persistence must report an unwritable state directory");
        assert!(error.contains("cannot create lease watermark directory"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn corrupt_lease_watermark_file_fails_closed() {
        let path = std::env::temp_dir().join(format!(
            "cua-lease-watermark-corrupt-{}-{}",
            std::process::id(),
            LEASE_PERSIST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&path, b"not-json")
            .expect("create a corrupt watermark fixture");
        let (watermarks, error) = load_lease_watermarks_at(&path);
        assert!(watermarks.is_empty());
        assert!(error.is_some_and(|error| error.contains("cannot decode")));
        let _ = std::fs::remove_file(path);
    }
}
