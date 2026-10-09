//! Unprivileged job supervisor. A pipe ties its lifetime to the root daemon.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    io::{Read, Write},
    os::unix::process::ExitStatusExt,
    process::{Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
#[derive(Serialize, Deserialize)]
pub struct Spec {
    pub command: Vec<String>,
    pub timeout: u64,
}
#[derive(Serialize, Deserialize)]
pub struct Outcome {
    pub state: String,
    pub exit_code: Option<i32>,
}
const LOG_LIMIT: usize = 1024 * 1024;
pub fn run(json: &str) -> Result<()> {
    ensure!(
        unsafe { libc::geteuid() } != 0,
        "worker refuses to run as root"
    );
    let spec: Spec = serde_json::from_str(json)?;
    ensure!(
        !spec.command.is_empty()
            && spec.command[0].starts_with('/')
            && (1..=14400).contains(&spec.timeout),
        "invalid worker specification"
    );
    // Also isolate direct invocations of this internal command from the caller's group.
    ensure!(
        unsafe { libc::setpgid(0, 0) } == 0,
        "could not isolate worker group"
    );
    let stopped = Arc::new(AtomicBool::new(false));
    for signal in [signal_hook::consts::SIGTERM, signal_hook::consts::SIGINT] {
        signal_hook::flag::register(signal, Arc::clone(&stopped))?;
    }
    let eof = Arc::clone(&stopped);
    thread::spawn(move || {
        let mut byte = [0];
        loop {
            match std::io::stdin().read(&mut byte) {
                Ok(0) | Err(_) => {
                    eof.store(true, Ordering::Relaxed);
                    break;
                }
                _ => {}
            }
        }
    });
    let mut child = Command::new(&spec.command[0])
        .args(&spec.command[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let count = Arc::new(Mutex::new(0usize));
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    drain(
        child.stdout.take().unwrap(),
        Arc::clone(&count),
        done_tx.clone(),
    );
    drain(child.stderr.take().unwrap(), count, done_tx);
    let started = Instant::now();
    let mut outcome = Outcome {
        state: "failed".into(),
        exit_code: None,
    };
    let code = loop {
        if stopped.load(Ordering::Relaxed) {
            // If the daemon died there is nobody else to clean the group, including us.
            unsafe {
                libc::kill(-libc::getpgrp(), libc::SIGKILL);
            }
            std::process::exit(125);
        }
        if started.elapsed().as_secs() >= spec.timeout {
            outcome.state = "timed_out".into();
            break 124;
        }
        // Observe exit without reaping, keeping the PID reserved for group cleanup.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let rc = unsafe {
            libc::waitid(
                libc::P_PID,
                child.id(),
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if rc != 0 {
            break 1;
        }
        if info.si_pid != 0 {
            if info.si_code == libc::CLD_EXITED {
                outcome.exit_code = Some(info.si_status);
                outcome.state = if info.si_status == 0 {
                    "succeeded"
                } else {
                    "failed"
                }
                .into();
            }
            break outcome.exit_code.unwrap_or(1);
        }
        thread::sleep(Duration::from_millis(100));
    };
    // Kill the direct child; the daemon cleans the entire reserved worker group.
    let _ = child.kill();
    let _ = child.wait();
    // Pipe readers get EOF after command descendants exit; bounded wait prevents detached
    // processes that inherited descriptors from keeping this supervisor alive.
    for _ in 0..2 {
        let _ = done_rx.recv_timeout(Duration::from_millis(500));
    }
    eprintln!("{}", serde_json::to_string(&outcome)?);
    std::process::exit(code);
}
fn drain(
    mut input: impl Read + Send + 'static,
    count: Arc<Mutex<usize>>,
    done: std::sync::mpsc::Sender<()>,
) {
    thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            let n = match input.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            let mut written = count.lock().unwrap();
            let take = n.min(LOG_LIMIT.saturating_sub(*written));
            if take > 0 {
                let _ = std::io::stdout().write_all(&buf[..take]);
                *written += take;
            }
        }
        let _ = done.send(());
    });
}

/// Observe a child without releasing its PID before process-group cleanup.
pub fn peek_exit(pid: u32) -> std::io::Result<Option<std::process::ExitStatus>> {
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    if unsafe {
        libc::waitid(
            libc::P_PID,
            pid,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error());
    }
    if info.si_pid == 0 {
        return Ok(None);
    }
    let raw = if info.si_code == libc::CLD_EXITED {
        info.si_status << 8
    } else {
        info.si_status & 0x7f
    };
    Ok(Some(std::process::ExitStatus::from_raw(raw)))
}
