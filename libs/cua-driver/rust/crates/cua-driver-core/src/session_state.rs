//! Best-effort embedded-driver process state file.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

pub const STATE_DIR_ENV: &str = "CUA_DRIVER_STATE_DIR";
pub const STATE_WRITER_PID_ARG: &str = "_state_writer_pid";
const SCHEMA_VERSION: u8 = 2;

/// Cross-platform fallback process-name resolver. Platform registries may
/// replace this with a native resolver when they have one.
pub fn resolve_process_name(pid: i64) -> Option<String> {
    if pid <= 0 {
        return None;
    }
    #[cfg(unix)]
    {
        let output = std::process::Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "comm="])
            .output()
            .ok()?;
        let raw = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if raw.is_empty() {
            return None;
        }
        return Some(
            Path::new(&raw)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(&raw)
                .to_owned(),
        );
    }
    #[cfg(windows)]
    {
        let filter = format!("PID eq {pid}");
        let output = std::process::Command::new("tasklist")
            .args(["/FI", &filter, "/FO", "CSV", "/NH"])
            .output()
            .ok()?;
        let row = String::from_utf8_lossy(&output.stdout);
        return row
            .trim()
            .strip_prefix('"')
            .and_then(|value| value.split("\",").next())
            .filter(|value| !value.is_empty())
            .map(str::to_owned);
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        None
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DriverProcessState {
    pub driver_pid: u32,
    /// Kernel-authenticated process that sent the action to a long-running daemon.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub writer_pid: Option<u32>,
    pub session: Option<String>,
    pub target_app: Option<String>,
    pub target_pid: Option<i64>,
    pub target_window_id: Option<u64>,
    pub last_action_at: String,
    pub schema: u8,
}

impl DriverProcessState {
    fn for_action(driver_pid: u32, args: &serde_json::Value, target_app: Option<String>) -> Self {
        Self {
            driver_pid,
            writer_pid: args
                .get(STATE_WRITER_PID_ARG)
                .and_then(serde_json::Value::as_u64)
                .and_then(|value| u32::try_from(value).ok())
                .filter(|value| *value > 1),
            session: session_for_action(args, crate::embedded_default_session_id()),
            target_app,
            target_pid: args.get("pid").and_then(|value| value.as_i64()),
            target_window_id: args.get("window_id").and_then(|value| value.as_u64()),
            last_action_at: time::OffsetDateTime::now_utc()
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned()),
            schema: SCHEMA_VERSION,
        }
    }
}

fn session_for_action(
    args: &serde_json::Value,
    embedded_default: Option<&str>,
) -> Option<String> {
    args.get("session")
        .or_else(|| args.get("_session_id"))
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .or(embedded_default)
        .map(str::to_owned)
}

/// One state file for the current driver process. Writes are always performed
/// through a same-directory temporary file followed by `rename`.
pub struct StateFile {
    dir: PathBuf,
    driver_pid: u32,
    temp_counter: AtomicU64,
}

impl StateFile {
    pub fn from_env() -> Option<Self> {
        std::env::var_os(STATE_DIR_ENV).map(|dir| Self::new(PathBuf::from(dir), std::process::id()))
    }

    pub fn new(dir: PathBuf, driver_pid: u32) -> Self {
        Self {
            dir,
            driver_pid,
            temp_counter: AtomicU64::new(0),
        }
    }

    pub fn path(&self) -> PathBuf {
        self.dir.join(format!("{}.json", self.driver_pid))
    }

    pub fn update(
        &self,
        args: &serde_json::Value,
        target_app: Option<String>,
    ) -> std::io::Result<()> {
        ensure_private_dir(&self.dir)?;
        let state = DriverProcessState::for_action(self.driver_pid, args, target_app);
        let body = serde_json::to_vec(&state)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        let sequence = self.temp_counter.fetch_add(1, Ordering::Relaxed);
        let temp_path = self
            .dir
            .join(format!(".{}.json.tmp-{}", self.driver_pid, sequence));

        let result = (|| {
            use std::io::Write;
            let mut file = create_private_temp_file(&temp_path)?;
            file.write_all(&body)?;
            file.sync_all()?;
            std::fs::rename(&temp_path, self.path())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temp_path);
        }
        result
    }

    pub fn remove(&self) -> std::io::Result<()> {
        match std::fs::remove_file(self.path()) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            result => result,
        }
    }
}

impl Drop for StateFile {
    fn drop(&mut self) {
        if let Err(error) = self.remove() {
            eprintln!("[cua-driver] warning: failed to remove state file: {error}");
        }
    }
}

pub(crate) fn ensure_private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // The host may supply an existing state directory. Re-apply the privacy
        // invariant on every write rather than trusting its prior mode.
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

pub(crate) fn create_private_temp_file(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anonymous_embedded_action_uses_the_cursor_session_default() {
        assert_eq!(
            session_for_action(&serde_json::json!({"pid": 10}), Some("cmux-codex-42")),
            Some("cmux-codex-42".to_owned())
        );
        assert_eq!(
            session_for_action(
                &serde_json::json!({"session": " explicit ", "_session_id": "proxy"}),
                Some("embedded-default"),
            ),
            Some("explicit".to_owned()),
            "an explicit session must retain precedence over proxy/default identity"
        );
    }

    #[test]
    fn update_atomically_replaces_the_process_file() {
        let dir = tempfile::tempdir().unwrap();
        let writer = StateFile::new(dir.path().to_owned(), 4242);
        writer
            .update(
                &serde_json::json!({"session":"first","pid":10,"window_id":20}),
                Some("Notes".to_owned()),
            )
            .unwrap();
        writer
            .update(
                &serde_json::json!({
                    "session":"second",
                    "pid":11,
                    "window_id":21,
                    "_state_writer_pid": 3131,
                }),
                Some("Safari".to_owned()),
            )
            .unwrap();

        let state: DriverProcessState =
            serde_json::from_slice(&std::fs::read(writer.path()).unwrap()).unwrap();
        assert_eq!(state.driver_pid, 4242);
        assert_eq!(state.writer_pid, Some(3131));
        assert_eq!(state.session.as_deref(), Some("second"));
        assert_eq!(state.target_app.as_deref(), Some("Safari"));
        assert_eq!(state.target_pid, Some(11));
        assert_eq!(state.target_window_id, Some(21));
        assert_eq!(state.schema, 2);
        assert!(time::OffsetDateTime::parse(
            &state.last_action_at,
            &time::format_description::well_known::Rfc3339,
        )
        .is_ok());
        assert_eq!(
            std::fs::read_dir(dir.path())
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp-"))
                .count(),
            0,
            "rename must leave no temporary file behind",
        );
    }

    #[test]
    fn malformed_state_directory_is_an_error_not_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        let not_a_dir = dir.path().join("plain-file");
        std::fs::write(&not_a_dir, b"occupied").unwrap();
        let writer = StateFile::new(not_a_dir, 4343);
        assert!(writer
            .update(&serde_json::json!({"pid": 10}), None)
            .is_err());
    }

    #[cfg(unix)]
    #[test]
    fn update_repairs_existing_directory_and_writes_private_state_file() {
        use std::os::unix::fs::PermissionsExt;

        let parent = tempfile::tempdir().unwrap();
        let state_dir = parent.path().join("state");
        std::fs::create_dir(&state_dir).unwrap();
        std::fs::set_permissions(&state_dir, std::fs::Permissions::from_mode(0o755)).unwrap();

        let writer = StateFile::new(state_dir.clone(), 4545);
        writer
            .update(&serde_json::json!({"session": "private"}), None)
            .unwrap();

        assert_eq!(
            std::fs::metadata(&state_dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(writer.path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn drop_removes_the_process_file_on_clean_shutdown() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("4444.json");
        {
            let writer = StateFile::new(dir.path().to_owned(), 4444);
            writer
                .update(&serde_json::json!({"pid": 10}), Some("Notes".to_owned()))
                .unwrap();
            assert!(path.exists());
        }
        assert!(!path.exists());
    }
}
