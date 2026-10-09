use crate::{ROOT, Request};
use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Local, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    ffi::CString,
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    os::unix::{fs::OpenOptionsExt, process::CommandExt},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

#[derive(Serialize, Deserialize)]
struct Config {
    uid: u32,
    gid: u32,
    user: String,
    home: String,
}
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
struct Job {
    id: String,
    at: DateTime<Utc>,
    wake_time: String,
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
    #[serde(default)]
    wake_cleanup: bool,
}
#[derive(Serialize, Deserialize, Default, PartialEq)]
struct State {
    jobs: Vec<Job>,
    restore_sleep: Option<bool>,
    heartbeat: Option<DateTime<Utc>>,
}
struct Store {
    root: PathBuf,
}
impl Store {
    fn new() -> Self {
        Self {
            root: PathBuf::from(ROOT),
        }
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

fn config() -> Result<Config> {
    ensure!(
        unsafe { libc::geteuid() } == 0,
        "privileged helper must run as root"
    );
    let c: Config = serde_json::from_slice(&fs::read(Path::new(ROOT).join("config.json"))?)?;
    ensure!(
        c.uid > 0 && c.gid > 0 && c.home.starts_with('/'),
        "invalid installed account"
    );
    Ok(c)
}
fn on_ac(text: &str) -> bool {
    text.lines()
        .next()
        .is_some_and(|l| l.contains("'AC Power'"))
}
fn ac() -> Result<bool> {
    Ok(on_ac(&system("/usr/bin/pmset", &["-g", "batt"])?))
}
fn sleep_disabled() -> Result<bool> {
    let text = system("/usr/bin/pmset", &["-g"])?;
    Ok(text.lines().any(|line| {
        let p: Vec<_> = line.split_whitespace().collect();
        p.first() == Some(&"SleepDisabled") && p.get(1) == Some(&"1")
    }))
}
fn set_sleep(disabled: bool) -> Result<()> {
    system(
        "/usr/bin/pmset",
        &["-a", "disablesleep", if disabled { "1" } else { "0" }],
    )?;
    Ok(())
}
fn wake(job: &Job, cancel: bool) -> Result<()> {
    // Execute native scheduling in a bounded child, just like pmset queries.
    system(
        crate::BIN,
        &[
            "power-event",
            &job.at.timestamp().to_string(),
            &job.id,
            if cancel { "true" } else { "false" },
        ],
    )?;
    Ok(())
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
    let c = config()?;
    let caller = std::env::var("SUDO_UID")
        .context("request must originate through sudo")?
        .parse::<u32>()?;
    ensure!(
        caller == c.uid,
        "this installation belongs to a different user"
    );
    let store = Store::new();
    match r {
        Request::Schedule{at,timeout,grace,cwd,command} => store.transaction(|s| {
            ensure!(healthy(&store),"daemon is not healthy; inspect launchctl print system/dev.oats");
            ensure!(ac()?,"V1 only accepts jobs while connected to AC power");
            ensure!(s.jobs.len()<1000,"history quota reached (1000 jobs); archive via uninstall before adding jobs");
            ensure!(s.jobs.iter().filter(|j|j.state=="queued").count()<32,"maximum 32 pending jobs");
            let job=Job{id:uuid::Uuid::new_v4().to_string(),at,
                wake_time:at.with_timezone(&Local).format("%m/%d/%y %H:%M:%S").to_string(),
                timeout,grace,cwd,command,state:"queued".into(),started:None,finished:None,exit_code:None,error:None,cancel:false,wake_cleanup:true};
            // Persist an intent while holding the state lock, before touching hardware.
            let mut job = job;
            job.state = "arming".into();
            s.jobs.push(job.clone());
            store.save(s)?;
            wake(&job,false)?;
            job.state = "queued".into();
            job.wake_cleanup = false;
            *s.jobs.last_mut().unwrap() = job.clone();
            let result=json!({"job":job,"warning":"Closed-lid wake is best-effort until verified with probe on this Mac."});
            Ok(result)
        }),
        Request::Status {} => {
            let _lock=store.lock("state.lock")?; let s=store.read()?;
            Ok(json!({"daemon_healthy":healthy(&store),"heartbeat":fs::metadata(store.root.join("heartbeat")).and_then(|m|m.modified()).ok().map(DateTime::<Utc>::from),"restore_pending":s.restore_sleep.is_some(),"jobs":s.jobs}))
        }
        Request::Cancel{id} => {
            let job = store.transaction(|s| {
                let j=s.jobs.iter_mut().find(|j|j.id==id).context("unknown job")?;
                if j.state=="queued" || j.state=="arming" {
                    j.state="cancelled".into(); j.finished=Some(Utc::now()); j.wake_cleanup=true;
                } else if j.state=="running" {j.cancel=true;}
                Ok(j.clone())
            })?;
            let warning = reconcile(&store).err().map(|e|format!("wake removal pending: {e:#}"));
            Ok(json!({"job":job,"warning":warning}))
        },
        Request::Logs{id} => {
            let _lock=store.lock("state.lock")?;
            ensure!(store.read()?.jobs.iter().any(|j|j.id==id),"unknown job");
            let path=store.log(&id);
            if !path.exists() {return Ok(json!({"output":""}));}
            let mut f=File::open(path)?; let len=f.metadata()?.len();
            f.seek(SeekFrom::Start(len.saturating_sub(65536)))?;
            let mut buf=Vec::new(); f.take(65536).read_to_end(&mut buf)?;
            Ok(json!({"output":String::from_utf8_lossy(&buf),"truncated":len>65536}))
        }
    }
}
// Every operation here is idempotent, including after a successful cancellation
// followed by a failed state save. The state lock excludes in-flight registration.
fn reconcile(store: &Store) -> Result<()> {
    store.transaction(|s| {
        for job in &mut s.jobs {
            if job.state == "arming" {
                job.state = "failed".into();
                job.finished = Some(Utc::now());
                job.error = Some("registration interrupted; command was not started".into());
                job.wake_cleanup = true;
            }
            if job.wake_cleanup {
                wake(job, true)?;
                job.wake_cleanup = false;
            }
        }
        Ok(())
    })
}
fn restore(store: &Store) -> Result<()> {
    restore_with(store, set_sleep)
}
fn restore_with(store: &Store, set: impl Fn(bool) -> Result<()>) -> Result<()> {
    store.transaction(|s| {
        if let Some(previous) = s.restore_sleep {
            set(previous)?;
            s.restore_sleep = None;
        }
        Ok(())
    })
}
fn acquire(store: &Store) -> Result<()> {
    acquire_with(store, sleep_disabled, set_sleep)
}
fn acquire_with(
    store: &Store,
    read: impl Fn() -> Result<bool>,
    set: impl Fn(bool) -> Result<()>,
) -> Result<()> {
    store.transaction(|s| {
        ensure!(
            s.restore_sleep.is_none(),
            "previous sleep restoration is pending"
        );
        s.restore_sleep = Some(read()?);
        Ok(())
    })?;
    // Durable recovery journal precedes the global setting change.
    set(true)
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
fn spawn(job: &Job, c: &Config, store: &Store) -> Result<Child> {
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
    let mut cmd = Command::new(crate::BIN);
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
    .env("HOME", &c.home)
    .env("USER", &c.user)
    .env("LOGNAME", &c.user)
    .env("TMPDIR", "/tmp")
    .stdin(Stdio::piped())
    .stdout(log)
    .stderr(outcome);
    let cwd = CString::new(job.cwd.as_str())?;
    let uid = c.uid;
    let gid = c.gid;
    // No shell interpolation. Drop all root credentials before resolving the working directory or exec.
    unsafe {
        cmd.pre_exec(move || {
            if libc::setpgid(0, 0) != 0
                || libc::setgroups(0, std::ptr::null()) != 0
                || libc::setgid(gid) != 0
                || libc::setuid(uid) != 0
                || libc::chdir(cwd.as_ptr()) != 0
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
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
    let c = config()?;
    let store = Store::new();
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
    restore(&store)?;
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
    let mut lease_held = false;
    let result = (|| -> Result<()> {
        while !stop.load(Ordering::Relaxed) {
            heartbeat(&store)?;
            reconcile(&store)?;
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
                    // Keep the lease until the next due job is selected, avoiding a
                    // lid-sleep gap between jobs with overlapping schedules.
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
                    let attempt = (if lease_held { Ok(()) } else { acquire(&store) })
                        .and_then(|_| spawn(&j, &c, &store));
                    match attempt {
                        Ok(child) => {
                            lease_held = true;
                            running = Some(Running {
                                child,
                                started: Instant::now(),
                                job: j,
                                stopped: false,
                            })
                        }
                        Err(e) => {
                            finish(&store, &j.id, "failed", None, Some(format!("{e:#}")))?;
                            restore(&store)?;
                            lease_held = false;
                        }
                    }
                } else if lease_held {
                    restore(&store)?;
                    lease_held = false;
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
    restore(&store)?;
    finish_result?;
    let _ = fs::remove_file(store.root.join("heartbeat"));
    store.transaction(|s| {
        s.heartbeat = None;
        Ok(())
    })?;
    result
}
pub fn cleanup() -> Result<()> {
    config()?;
    let store = Store::new();
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(store.root.join("daemon.lock"))?;
    lock.try_lock_exclusive()
        .context("stop the daemon before cleanup")?;
    restore(&store)?;
    store.transaction(|s| {
        for j in &mut s.jobs {
            if j.state == "queued" || j.state == "arming" {
                j.state = "cancelled".into();
                j.finished = Some(Utc::now());
                j.wake_cleanup = true;
            }
        }
        s.heartbeat = None;
        Ok(())
    })?;
    reconcile(&store)?;
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
            wake_time: String::new(),
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
            wake_cleanup: false,
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
                s.restore_sleep = Some(false);
                s.jobs.push(job());
                Ok(())
            })
            .unwrap();
        let state = store.read().unwrap();
        assert_eq!(state.restore_sleep, Some(false));
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
    fn restoration_failure_keeps_recovery_journal() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store {
            root: tmp.path().into(),
        };
        acquire_with(
            &store,
            || Ok(false),
            |value| {
                assert!(value);
                assert_eq!(store.read()?.restore_sleep, Some(false));
                Ok(())
            },
        )
        .unwrap();
        assert!(restore_with(&store, |_| anyhow::bail!("simulated pmset failure")).is_err());
        assert_eq!(store.read().unwrap().restore_sleep, Some(false));
        restore_with(&store, |value| {
            assert!(!value);
            Ok(())
        })
        .unwrap();
        assert_eq!(store.read().unwrap().restore_sleep, None);
    }
    #[test]
    fn preserves_preexisting_sleep_prevention() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store {
            root: tmp.path().into(),
        };
        acquire_with(&store, || Ok(true), |_| Ok(())).unwrap();
        restore_with(&store, |value| {
            assert!(value);
            Ok(())
        })
        .unwrap();
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
    fn audit_registration_intent_survives_side_effect_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store {
            root: tmp.path().into(),
        };
        let result: Result<()> = store.transaction(|s| {
            let mut j = job();
            j.state = "arming".into();
            j.wake_cleanup = true;
            s.jobs.push(j);
            store.save(s)?;
            anyhow::bail!("simulated power service failure");
        });
        assert!(result.is_err());
        assert_eq!(store.read().unwrap().jobs[0].state, "arming");
        assert!(store.read().unwrap().jobs[0].wake_cleanup);
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
