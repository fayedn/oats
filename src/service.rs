use crate::Request;
use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    os::unix::{fs::OpenOptionsExt, process::CommandExt},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
struct Job {
    id: String,
    at: DateTime<Utc>,
    timeout: u64,
    grace: u64,
    cwd: String,
    command: Vec<String>,
    state: String,
    started: Option<DateTime<Utc>>,
    finished: Option<DateTime<Utc>>,
    exit_code: Option<i32>,
    error: Option<String>,
    cancel: bool,
}
#[derive(Serialize, Deserialize, Default, PartialEq)]
struct State {
    jobs: Vec<Job>,
    heartbeat: Option<DateTime<Utc>>,
}
struct Store {
    root: PathBuf,
}
impl Store {
    fn new() -> Result<Self> {
        let root = crate::root()?;
        ensure!(root.join("logs").is_dir(), "run scripts/install.sh first");
        Ok(Self { root })
    }
    fn lock(&self, name: &str) -> Result<File> {
        let f = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(self.root.join(name))?;
        f.lock_exclusive()?;
        Ok(f)
    }
    fn read(&self) -> Result<State> {
        match fs::read(self.root.join("state.json")) {
            Ok(data) => Ok(serde_json::from_slice(&data)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
            Err(e) => Err(e.into()),
        }
    }
    fn save(&self, state: &State) -> Result<()> {
        let tmp = self.root.join("state.tmp");
        let mut f = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(&serde_json::to_vec_pretty(state)?)?;
        f.sync_all()?;
        fs::rename(tmp, self.root.join("state.json"))?;
        File::open(&self.root)?.sync_all()?;
        Ok(())
    }
    fn transaction<T>(&self, f: impl FnOnce(&mut State) -> Result<T>) -> Result<T> {
        let _lock = self.lock("state.lock")?;
        let mut s = self.read()?;
        let before = serde_json::to_vec(&s)?;
        let result = f(&mut s)?;
        if serde_json::to_vec(&s)? != before {
            self.save(&s)?;
        }
        Ok(result)
    }
    fn log(&self, id: &str) -> PathBuf {
        self.root.join("logs").join(format!("{id}.log"))
    }
}
pub fn system(program: &str, args: &[&str]) -> Result<String> {
    let mut child = Command::new(program)
        .args(args)
        .env_clear()
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let (tx, rx) = std::sync::mpsc::channel();
    let tx_out = tx.clone();
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = stdout.take(65537).read_to_end(&mut bytes).map(|_| bytes);
        let _ = tx_out.send((true, result));
    });
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = stderr.take(65537).read_to_end(&mut bytes).map(|_| bytes);
        let _ = tx.send((false, result));
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(25)),
            result => {
                let _ = child.kill();
                let _ = child.wait();
                if let Err(e) = result {
                    return Err(e.into());
                }
                anyhow::bail!("{program} exceeded the 5-second control-operation deadline");
            }
        }
    };
    let mut out = Vec::new();
    let mut err = Vec::new();
    for _ in 0..2 {
        let (is_out, bytes) = rx
            .recv_timeout(Duration::from_secs(1))
            .context("control output did not close")?;
        let bytes = bytes?;
        ensure!(bytes.len() <= 65536, "control output exceeded 64 KiB");
        if is_out {
            out = bytes;
        } else {
            err = bytes;
        }
    }
    ensure!(
        status.success(),
        "{program} failed: {}{}",
        String::from_utf8_lossy(&out),
        String::from_utf8_lossy(&err)
    );
    Ok(String::from_utf8_lossy(&out).into_owned())
}

fn on_ac(text: &str) -> bool {
    text.lines()
        .next()
        .is_some_and(|l| l.contains("'AC Power'"))
}
fn ac() -> Result<bool> {
    Ok(on_ac(&system("/usr/bin/pmset", &["-g", "batt"])?))
}
fn healthy(store: &Store) -> bool {
    fs::metadata(store.root.join("heartbeat"))
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age < Duration::from_secs(15))
}
fn heartbeat(store: &Store) -> Result<()> {
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(store.root.join("heartbeat"))?;
    Ok(())
}

pub fn handle(r: Request) -> Result<Value> {
    let store = Store::new()?;
    match r {
        Request::Schedule {
            at,
            timeout,
            grace,
            cwd,
            command,
        } => store.transaction(|s| {
            ensure!(
                healthy(&store),
                "daemon is not healthy; inspect the user LaunchAgent dev.oats"
            );
            ensure!(ac()?, "V1 only accepts jobs while connected to AC power");
            ensure!(
                s.jobs.len() < 1000,
                "history quota reached (1000 jobs); archive via uninstall before adding jobs"
            );
            ensure!(
                s.jobs.iter().filter(|j| j.state == "queued").count() < 32,
                "maximum 32 pending jobs"
            );
            let job = Job {
                id: uuid::Uuid::new_v4().to_string(),
                at,
                timeout,
                grace,
                cwd,
                command,
                state: "queued".into(),
                started: None,
                finished: None,
                exit_code: None,
                error: None,
                cancel: false,
            };
            s.jobs.push(job.clone());
            Ok(json!({"job":job}))
        }),
        Request::Status {} => {
            let _lock = store.lock("state.lock")?;
            let s = store.read()?;
            Ok(
                json!({"daemon_healthy":healthy(&store),"heartbeat":fs::metadata(store.root.join("heartbeat")).and_then(|m|m.modified()).ok().map(DateTime::<Utc>::from),"jobs":s.jobs}),
            )
        }
        Request::Cancel { id } => {
            let job = store.transaction(|s| {
                let j = s
                    .jobs
                    .iter_mut()
                    .find(|j| j.id == id)
                    .context("unknown job")?;
                if j.state == "queued" {
                    j.state = "cancelled".into();
                    j.finished = Some(Utc::now());
                } else if j.state == "running" {
                    j.cancel = true;
                }
                Ok(j.clone())
            })?;
            Ok(json!({"job":job}))
        }
        Request::Logs { id } => {
            let _lock = store.lock("state.lock")?;
            ensure!(store.read()?.jobs.iter().any(|j| j.id == id), "unknown job");
            let path = store.log(&id);
            if !path.exists() {
                return Ok(json!({"output":""}));
            }
            let mut f = File::open(path)?;
            let len = f.metadata()?.len();
            f.seek(SeekFrom::Start(len.saturating_sub(65536)))?;
            let mut buf = Vec::new();
            f.take(65536).read_to_end(&mut buf)?;
            Ok(json!({"output":String::from_utf8_lossy(&buf),"truncated":len>65536}))
        }
    }
}
fn finish(
    store: &Store,
    id: &str,
    state: &str,
    code: Option<i32>,
    error: Option<String>,
) -> Result<()> {
    store.transaction(|s| {
        let j = s
            .jobs
            .iter_mut()
            .find(|j| j.id == id)
            .context("job disappeared")?;
        j.state = state.into();
        j.finished = Some(Utc::now());
        j.exit_code = code;
        j.error = error;
        Ok(())
    })
}
fn disposition(j: &Job, now: DateTime<Utc>, ac: bool) -> Option<&'static str> {
    if j.state != "queued" || j.at > now {
        None
    } else if (now - j.at).num_seconds() > j.grace as i64 {
        Some("missed")
    } else if !ac {
        Some("skipped_battery")
    } else {
        Some("running")
    }
}
fn spawn(job: &Job, store: &Store) -> Result<Child> {
    let log = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(store.log(&job.id))?;
    let outcome = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(store.root.join(format!("{}.result", job.id)))?;
    let mut cmd = Command::new(std::env::current_exe()?);
    cmd.args([
        "worker",
        &serde_json::to_string(&crate::worker::Spec {
            command: job.command.clone(),
            timeout: job.timeout,
        })?,
    ])
    .env_clear()
    .env(
        "PATH",
        "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin",
    )
    .env("HOME", std::env::var_os("HOME").context("HOME is not set")?)
    .env("TMPDIR", "/tmp")
    .stdin(Stdio::piped())
    .stdout(log)
    .stderr(outcome);
    cmd.current_dir(&job.cwd).process_group(0);
    cmd.spawn().context("could not start user command")
}
struct Running {
    child: Child,
    started: Instant,
    job: Job,
    stopped: bool,
}
impl Running {
    fn stop(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        // EOF asks the unprivileged supervisor to kill its command tree.
        self.child.stdin.take();
        for _ in 0..30 {
            if crate::worker::peek_exit(self.child.id())
                .ok()
                .flatten()
                .is_some()
            {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
        // The leader is not reaped yet: its PID/PGID cannot have been reused.
        unsafe {
            libc::kill(-(self.child.id() as i32), libc::SIGKILL);
        }
        let _ = self.child.wait();
    }
}
impl Drop for Running {
    fn drop(&mut self) {
        self.stop();
    }
}

pub fn daemon() -> Result<()> {
    let store = Store::new()?;
    let daemon_lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(store.root.join("daemon.lock"))?;
    daemon_lock
        .try_lock_exclusive()
        .context("daemon already running")?;
    let stop = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&stop))?;
    signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&stop))?;
    store.transaction(|s| {
        for j in &mut s.jobs {
            if j.state == "running" {
                j.state = "interrupted".into();
                j.finished = Some(Utc::now());
                j.error = Some("service restarted; job was not retried".into());
            }
        }
        Ok(())
    })?;
    let mut running: Option<Running> = None;
    let result = (|| -> Result<()> {
        while !stop.load(Ordering::Relaxed) {
            heartbeat(&store)?;
            // A failed power query is treated as loss of AC, not permission to continue.
            let power = ac().unwrap_or(false);
            if let Some(r) = running.as_mut() {
                let cancelled = store.transaction(|s| {
                    Ok(s.jobs
                        .iter()
                        .find(|j| j.id == r.job.id)
                        .is_some_and(|j| j.cancel))
                })?;
                let status = crate::worker::peek_exit(r.child.id())?;
                let outcome = if status.is_some() {
                    File::open(store.root.join(format!("{}.result", r.job.id)))
                        .ok()
                        .and_then(|file| {
                            let mut bytes = Vec::new();
                            file.take(4097).read_to_end(&mut bytes).ok()?;
                            if bytes.len() > 4096 {
                                return None;
                            }
                            serde_json::from_slice::<crate::worker::Outcome>(&bytes).ok()
                        })
                } else {
                    None
                };
                let reason = if let Some(status) = status {
                    Some(match outcome.as_ref().map(|x| x.state.as_str()) {
                        Some("succeeded") => "succeeded",
                        Some("timed_out") => "timed_out",
                        Some("failed") => "failed",
                        _ if status.success() => "succeeded",
                        _ => "failed",
                    })
                } else if cancelled {
                    Some("cancelled")
                } else if !power {
                    Some("stopped_battery")
                } else if r.started.elapsed().as_secs() >= r.job.timeout {
                    Some("timed_out")
                } else {
                    None
                };
                if let Some(reason) = reason {
                    r.stop();
                    finish(
                        &store,
                        &r.job.id,
                        reason,
                        outcome
                            .as_ref()
                            .map(|o| o.exit_code)
                            .unwrap_or_else(|| status.and_then(|x| x.code())),
                        None,
                    )?;
                    running = None;
                }
            }
            if running.is_none() {
                let next = store.transaction(|s| {
                    s.jobs.sort_by_key(|j| j.at);
                    for j in &mut s.jobs {
                        if let Some(state) = disposition(j, Utc::now(), power) {
                            j.state = state.into();
                            if state == "running" {
                                j.started = Some(Utc::now());
                                return Ok(Some(j.clone()));
                            }
                            j.finished = Some(Utc::now());
                        }
                    }
                    Ok(None)
                })?;
                if let Some(j) = next {
                    let attempt = spawn(&j, &store);
                    match attempt {
                        Ok(child) => {
                            running = Some(Running {
                                child,
                                started: Instant::now(),
                                job: j,
                                stopped: false,
                            })
                        }
                        Err(e) => {
                            finish(&store, &j.id, "failed", None, Some(format!("{e:#}")))?;
                        }
                    }
                }
            }
            thread::sleep(Duration::from_secs(2));
        }
        Ok(())
    })();
    let finish_result = if let Some(mut r) = running {
        r.stop();
        finish(
            &store,
            &r.job.id,
            "interrupted",
            None,
            Some("service stopping".into()),
        )
    } else {
        Ok(())
    };
    finish_result?;
    let _ = fs::remove_file(store.root.join("heartbeat"));
    store.transaction(|s| {
        s.heartbeat = None;
        Ok(())
    })?;
    result
}
pub fn cleanup() -> Result<()> {
    let store = Store::new()?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(store.root.join("daemon.lock"))?;
    lock.try_lock_exclusive()
        .context("stop the daemon before cleanup")?;
    store.transaction(|s| {
        for j in &mut s.jobs {
            if j.state == "queued" {
                j.state = "cancelled".into();
                j.finished = Some(Utc::now());
            }
        }
        s.heartbeat = None;
        Ok(())
    })?;
    let _ = fs::remove_file(store.root.join("heartbeat"));
    println!("{}", json!({"cleaned":true}));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn job() -> Job {
        Job {
            id: uuid::Uuid::new_v4().to_string(),
            at: Utc::now() - chrono::Duration::seconds(5),
            timeout: 10,
            grace: 60,
            cwd: "/tmp".into(),
            command: vec!["/bin/echo".into()],
            state: "queued".into(),
            started: None,
            finished: None,
            exit_code: None,
            error: None,
            cancel: false,
        }
    }
    #[test]
    fn power_detection_fails_closed() {
        assert!(on_ac("Now drawing from 'AC Power'\n"));
        assert!(!on_ac("Now drawing from 'Battery Power'\n"));
        assert!(!on_ac(""));
    }
    #[test]
    fn missed_and_battery_jobs_never_run() {
        let mut j = job();
        assert_eq!(disposition(&j, Utc::now(), true), Some("running"));
        assert_eq!(disposition(&j, Utc::now(), false), Some("skipped_battery"));
        j.at -= chrono::Duration::hours(1);
        assert_eq!(disposition(&j, Utc::now(), true), Some("missed"));
    }
    #[test]
    fn finished_jobs_are_not_replayed() {
        let mut j = job();
        j.state = "succeeded".into();
        assert_eq!(disposition(&j, Utc::now(), true), None);
    }
    #[test]
    fn future_jobs_do_not_hold_awake() {
        let mut j = job();
        j.at += chrono::Duration::hours(1);
        assert_eq!(disposition(&j, Utc::now(), true), None);
    }
    #[test]
    fn journal_survives_reopen() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store {
            root: tmp.path().into(),
        };
        store
            .transaction(|s| {
                s.jobs.push(job());
                Ok(())
            })
            .unwrap();
        let state = store.read().unwrap();
        assert_eq!(state.jobs.len(), 1);
    }
    #[test]
    fn failed_transaction_preserves_state() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store {
            root: tmp.path().into(),
        };
        store
            .transaction(|s| {
                s.jobs.push(job());
                Ok(())
            })
            .unwrap();
        let result: Result<()> = store.transaction(|s| {
            s.jobs.clear();
            anyhow::bail!("simulated error")
        });
        assert!(result.is_err());
        assert_eq!(store.read().unwrap().jobs.len(), 1);
    }
    #[test]
    fn audit_control_command_deadline_is_bounded() {
        let start = Instant::now();
        assert!(
            system("/bin/sleep", &["30"])
                .unwrap_err()
                .to_string()
                .contains("deadline")
        );
        assert!(start.elapsed() < Duration::from_secs(7));
    }
    #[test]
    fn audit_idle_transaction_does_not_rewrite_database() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store {
            root: tmp.path().into(),
        };
        store
            .transaction(|s| {
                s.jobs.push(job());
                Ok(())
            })
            .unwrap();
        let before = fs::metadata(store.root.join("state.json"))
            .unwrap()
            .modified()
            .unwrap();
        thread::sleep(Duration::from_millis(30));
        store.transaction(|_| Ok(())).unwrap();
        assert_eq!(
            before,
            fs::metadata(store.root.join("state.json"))
                .unwrap()
                .modified()
                .unwrap()
        );
    }
    #[test]
    fn audit_stop_cleans_group_after_supervisor_is_killed() {
        let tmp = tempfile::tempdir().unwrap();
        let pidfile = tmp.path().join("child.pid");
        let child = Command::new("/bin/sh")
            .args(["-c", "/bin/sleep 30 & echo $! > \"$1\"; wait", "audit"])
            .arg(&pidfile)
            .process_group(0)
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        let mut running = Running {
            child,
            started: Instant::now(),
            job: job(),
            stopped: false,
        };
        let deadline = Instant::now() + Duration::from_secs(2);
        while !pidfile.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        let pid = fs::read_to_string(pidfile).unwrap();
        unsafe {
            libc::kill(running.child.id() as i32, libc::SIGKILL);
        }
        running.stop();
        let status = Command::new("/bin/ps")
            .args(["-o", "stat=", "-p", pid.trim()])
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&status.stdout);
        assert!(
            text.trim().is_empty() || text.trim().starts_with('Z'),
            "descendant still running: {text}"
        );
    }
}
