//! Machine consent from the verified cmux host (feature `cmux-host-consent`).
//!
//! The build inside cmux.app holds the person's macOS grants, so it trusts
//! no file or environment variable a caller could redirect. The cmux app
//! instead holds one authenticated connection to the `serve` daemon
//! (`machine_consent_host`, checked by the daemon: host token and the
//! peer's code signature). On that connection the daemon asks
//!
//!   daemon -> host: `{"type":"machine_consent","id":N}`
//!   host -> daemon: `{"type":"machine_consent","id":N,"ok":true,"result":{...}}`
//!   daemon -> host: `{"type":"consent_refused","surface":S}`
//!
//! and the host answers from its own acpmux daemon
//! (`_acpmux/machine_consent_get`). Every [`ConsentSource::status`] asks
//! fresh. No host, no reply within [`DEADLINE`], an error or a malformed
//! result is [`ConsentStatus::UNKNOWN`]: joined, nothing granted.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

use crate::machine_consent::{ConsentSource, ConsentStatus, Surface};

/// Below acpmux's bridge deadline (2 s); the host's own daemon query is
/// bounded below this.
pub const DEADLINE: Duration = Duration::from_millis(1500);

type Reply = std::sync::mpsc::Sender<Option<Value>>;

#[derive(Default)]
struct State {
    /// The connected host: its generation and the queue its writer drains.
    host: Option<(u64, tokio::sync::mpsc::UnboundedSender<String>)>,
    next_id: u64,
    next_generation: u64,
    /// Open questions by id, with the generation of the host they went to:
    /// only that host's connection may answer one.
    pending: HashMap<u64, (u64, Reply)>,
}

impl State {
    /// Fails every open question not asked of `generation`'s host.
    fn fail_pending_except(&mut self, generation: Option<u64>) {
        self.pending.retain(|_, (asked, waiter)| {
            let keep = Some(*asked) == generation;
            if !keep {
                let _ = waiter.send(None);
            }
            keep
        });
    }
}

/// The consent the verified host serves. One per process ([`shared`]);
/// tests make their own.
#[derive(Default)]
pub struct HostConsent {
    state: Mutex<State>,
    deadline: Option<Duration>,
}

/// The process's host consent source.
pub fn shared() -> Arc<dyn ConsentSource> {
    shared_host().clone()
}

/// The process's host consent, for the daemon to attach a host to.
pub fn shared_host() -> &'static Arc<HostConsent> {
    static SHARED: OnceLock<Arc<HostConsent>> = OnceLock::new();
    SHARED.get_or_init(|| Arc::new(HostConsent::default()))
}

impl HostConsent {
    /// A source with its own deadline (tests).
    pub fn with_deadline(deadline: Duration) -> Self {
        Self {
            state: Mutex::default(),
            deadline: Some(deadline),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Serves one verified host connection until it closes: writes the
    /// daemon's questions and reads the host's answers. The caller has
    /// already checked the host. A newer host replaces this one.
    pub async fn serve_host<R, W>(&self, lines: &mut tokio::io::Lines<R>, writer: &mut W)
    where
        R: AsyncBufRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let generation = {
            let mut state = self.lock();
            state.next_generation += 1;
            let generation = state.next_generation;
            state.host = Some((generation, tx));
            // Questions the replaced host was asked fail now.
            state.fail_pending_except(Some(generation));
            generation
        };
        loop {
            tokio::select! {
                question = rx.recv() => {
                    let Some(question) = question else { break };
                    if writer.write_all(question.as_bytes()).await.is_err()
                        || writer.flush().await.is_err()
                    {
                        break;
                    }
                }
                line = lines.next_line() => {
                    let Ok(Some(line)) = line else { break };
                    self.answer(generation, &line);
                }
            }
        }
        let mut state = self.lock();
        if state.host.as_ref().is_some_and(|(current, _)| *current == generation) {
            state.host = None;
            // Questions this host will never answer fail now.
            state.fail_pending_except(None);
        }
    }

    fn answer(&self, generation: u64, line: &str) {
        let Ok(message) = serde_json::from_str::<Value>(line) else { return };
        if message["type"] != "machine_consent" {
            return;
        }
        let Some(id) = message["id"].as_u64() else { return };
        let result = (message["ok"] == Value::Bool(true))
            .then(|| message.get("result").cloned())
            .flatten();
        let mut state = self.lock();
        // An answer counts only from the host the question went to.
        if state.pending.get(&id).is_some_and(|(asked, _)| *asked == generation) {
            if let Some((_, waiter)) = state.pending.remove(&id) {
                let _ = waiter.send(result);
            }
        }
    }

    fn ask(&self) -> Option<Value> {
        let (reply, answer) = std::sync::mpsc::channel();
        let id = {
            let mut state = self.lock();
            let (generation, host) = state.host.as_ref()?;
            let (generation, host) = (*generation, host.clone());
            state.next_id += 1;
            let id = state.next_id;
            state.pending.insert(id, (generation, reply));
            let question = json!({"type": "machine_consent", "id": id}).to_string() + "\n";
            if host.send(question).is_err() {
                state.pending.remove(&id);
                return None;
            }
            id
        };
        let deadline = self.deadline.unwrap_or(DEADLINE);
        let received = wait_off_runtime(|| answer.recv_timeout(deadline).ok().flatten());
        self.lock().pending.remove(&id);
        received
    }
}

/// Runs a blocking wait without starving the async runtime: on a
/// multi-thread runtime the worker hands its tasks off first. A
/// current-thread runtime could not run the host's reader meanwhile, so
/// there the answer is none.
fn wait_off_runtime<T>(wait: impl FnOnce() -> Option<T>) -> Option<T> {
    match tokio::runtime::Handle::try_current().map(|handle| handle.runtime_flavor()) {
        Err(_) => wait(),
        Ok(tokio::runtime::RuntimeFlavor::MultiThread) => tokio::task::block_in_place(wait),
        Ok(_) => None,
    }
}

/// `_acpmux/machine_consent_get`'s result, read strictly:
/// `consentRequired` a JSON boolean and `consent.screen` and
/// `consent.control` each "none" or "granted". Anything else is none.
pub fn parse_status(result: &Value) -> Option<ConsentStatus> {
    let grant = |value: &Value| match value.as_str()? {
        "granted" => Some(true),
        "none" => Some(false),
        _ => None,
    };
    Some(ConsentStatus {
        consent_required: result.get("consentRequired")?.as_bool()?,
        screen: grant(result.get("consent")?.get("screen")?)?,
        control: grant(result.get("consent")?.get("control")?)?,
    })
}

impl ConsentSource for HostConsent {
    fn status(&self) -> ConsentStatus {
        self.ask()
            .as_ref()
            .and_then(parse_status)
            .unwrap_or(ConsentStatus::UNKNOWN)
    }

    fn record_refusal(&self, surface: Surface) {
        let state = self.lock();
        if let Some((_, host)) = state.host.as_ref() {
            let line = json!({"type": "consent_refused", "surface": surface.as_str()}).to_string();
            let _ = host.send(line + "\n");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    const GRANTED: &str = r#"{"consentRequired":true,"consent":{"screen":"granted","control":"granted"}}"#;

    #[test]
    fn the_daemon_answer_is_read_strictly() {
        let parsed = |text: &str| parse_status(&serde_json::from_str(text).unwrap());
        assert_eq!(
            parsed(r#"{"consentRequired":true,"consent":{"screen":"granted","control":"none"},"lastRefusal":null}"#),
            Some(ConsentStatus { consent_required: true, screen: true, control: false })
        );
        for bad in [
            r#"{"consent":{"screen":"granted","control":"granted"}}"#,
            r#"{"consentRequired":"false","consent":{"screen":"granted","control":"granted"}}"#,
            r#"{"consentRequired":1,"consent":{"screen":"granted","control":"granted"}}"#,
            r#"{"consentRequired":true,"consent":{"screen":"GRANTED","control":"granted"}}"#,
            r#"{"consentRequired":true,"consent":{"screen":true,"control":true}}"#,
            r#"{"consentRequired":false}"#,
            r#"[]"#,
        ] {
            assert_eq!(parsed(bad), None, "{bad}");
        }
    }

    #[test]
    fn with_no_host_nothing_is_granted() {
        let consent = HostConsent::default();
        assert_eq!(consent.status(), ConsentStatus::UNKNOWN);
        consent.record_refusal(Surface::Screen);
    }

    /// A scripted host on the other end of a socket pair: answers each
    /// question from `answers` in turn and records refusals.
    async fn host(
        consent: Arc<HostConsent>,
        answers: Vec<Option<&'static str>>,
    ) -> (tokio::task::JoinHandle<()>, tokio::task::JoinHandle<Vec<Value>>) {
        let (daemon_end, host_end) = tokio::net::UnixStream::pair().unwrap();
        let serving = tokio::spawn(async move {
            let (reader, mut writer) = daemon_end.into_split();
            consent.serve_host(&mut BufReader::new(reader).lines(), &mut writer).await;
        });
        let script = tokio::spawn(async move {
            let (reader, mut writer) = host_end.into_split();
            let mut lines = BufReader::new(reader).lines();
            let mut answers = answers.into_iter().peekable();
            let mut seen = Vec::new();
            while let Ok(Some(line)) = lines.next_line().await {
                let message: Value = serde_json::from_str(&line).unwrap();
                seen.push(message.clone());
                if message["type"] != "machine_consent" {
                    continue;
                }
                assert_eq!(
                    message.as_object().unwrap().keys().collect::<Vec<_>>(),
                    ["id", "type"],
                    "the question carries nothing the answer depends on"
                );
                let reply = match answers.next() {
                    Some(Some(result)) => json!({"type": "machine_consent", "id": message["id"],
                        "ok": true, "result": serde_json::from_str::<Value>(result).unwrap()}),
                    Some(None) => json!({"type": "machine_consent", "id": message["id"], "ok": false}),
                    None => return seen,
                };
                writer.write_all((reply.to_string() + "\n").as_bytes()).await.unwrap();
                // Out of answers: the host goes away.
                if answers.peek().is_none() {
                    return seen;
                }
            }
            seen
        });
        (serving, script)
    }

    async fn attached(consent: &HostConsent) {
        for _ in 0..500 {
            if consent.lock().host.is_some() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the host never attached");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn every_status_asks_the_host_and_failures_grant_nothing() {
        let consent = Arc::new(HostConsent::with_deadline(Duration::from_secs(10)));
        let (serving, script) = host(
            consent.clone(),
            vec![
                Some(r#"{"consentRequired":true,"consent":{"screen":"none","control":"none"}}"#),
                Some(GRANTED),
                Some(r#"{"consentRequired":true,"consent":{"screen":"granted","control":"none"}}"#),
                None,
                Some(r#"{"error":"daemon unreachable"}"#),
                Some(r#"{"consentRequired":false,"consent":{"screen":"none","control":"none"}}"#),
            ],
        )
        .await;
        attached(&consent).await;
        // A state directory that says "not joined" (what an alternate
        // ACPMUX_HOME would show) is never read: the host's answer is the
        // only input.
        let alternate = tempfile::tempdir().unwrap();
        let files = crate::machine_consent::StateFiles::At(Some(alternate.path().to_owned()));
        assert!(!files.status().consent_required);
        let status = |screen, control| ConsentStatus { consent_required: true, screen, control };
        assert_eq!(consent.status(), status(false, false), "deny");
        consent.record_refusal(Surface::Screen);
        assert_eq!(consent.status(), status(true, true), "grant");
        assert_eq!(consent.status(), status(true, false), "revoke control");
        assert_eq!(consent.status(), ConsentStatus::UNKNOWN, "ok:false");
        assert_eq!(consent.status(), ConsentStatus::UNKNOWN, "malformed");
        assert!(!consent.status().consent_required, "unjoined, from the host");
        let seen = script.await.unwrap();
        assert_eq!(consent.status(), ConsentStatus::UNKNOWN, "the host went away");
        assert!(seen.iter().any(|m| m["type"] == "consent_refused" && m["surface"] == "screen"));
        drop(serving);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_host_that_goes_away_grants_nothing_at_once() {
        let consent = Arc::new(HostConsent::with_deadline(Duration::from_secs(30)));
        let (serving, script) = host(consent.clone(), vec![Some(GRANTED)]).await;
        attached(&consent).await;
        assert!(consent.status().granted(Surface::Control));
        // The script closes its end after its one answer.
        let _ = script.await;
        serving.await.unwrap();
        let started = std::time::Instant::now();
        assert_eq!(consent.status(), ConsentStatus::UNKNOWN);
        assert!(started.elapsed() < Duration::from_secs(5), "no wait for a host that is gone");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn only_the_host_a_question_went_to_may_answer_it() {
        let consent = Arc::new(HostConsent::with_deadline(Duration::from_secs(30)));
        let (old_daemon, old_host) = tokio::net::UnixStream::pair().unwrap();
        let old_serving = {
            let consent = consent.clone();
            tokio::spawn(async move {
                let (reader, mut writer) = old_daemon.into_split();
                consent.serve_host(&mut BufReader::new(reader).lines(), &mut writer).await;
            })
        };
        attached(&consent).await;
        // A question to the old host, which never answers in time.
        let asking = {
            let consent = consent.clone();
            tokio::task::spawn_blocking(move || consent.status())
        };
        let (old_reader, mut old_writer) = old_host.into_split();
        let mut old_lines = BufReader::new(old_reader).lines();
        let question: Value = serde_json::from_str(&old_lines.next_line().await.unwrap().unwrap()).unwrap();
        // A new host replaces it: the open question fails at once.
        let (new_daemon, _new_host) = tokio::net::UnixStream::pair().unwrap();
        let new_serving = {
            let consent = consent.clone();
            tokio::spawn(async move {
                let (reader, mut writer) = new_daemon.into_split();
                consent.serve_host(&mut BufReader::new(reader).lines(), &mut writer).await;
            })
        };
        let started = std::time::Instant::now();
        assert_eq!(asking.await.unwrap(), ConsentStatus::UNKNOWN);
        assert!(started.elapsed() < Duration::from_secs(10));
        // A late grant from the replaced host is ignored.
        let late = json!({"type": "machine_consent", "id": question["id"], "ok": true,
            "result": serde_json::from_str::<Value>(GRANTED).unwrap()});
        let _ = old_writer.write_all((late.to_string() + "\n").as_bytes()).await;
        assert!(consent.lock().pending.is_empty());
        old_serving.abort();
        new_serving.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_silent_host_times_out_closed() {
        let consent = Arc::new(HostConsent::with_deadline(Duration::from_millis(100)));
        let (daemon_end, _host_end) = tokio::net::UnixStream::pair().unwrap();
        let serving = {
            let consent = consent.clone();
            tokio::spawn(async move {
                let (reader, mut writer) = daemon_end.into_split();
                consent.serve_host(&mut BufReader::new(reader).lines(), &mut writer).await;
            })
        };
        attached(&consent).await;
        assert_eq!(consent.status(), ConsentStatus::UNKNOWN);
        serving.abort();
    }

    #[tokio::test]
    async fn a_current_thread_runtime_never_blocks_on_the_host() {
        let consent = HostConsent::with_deadline(Duration::from_secs(30));
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        consent.lock().host = Some((1, tx));
        assert_eq!(consent.status(), ConsentStatus::UNKNOWN);
    }
}
