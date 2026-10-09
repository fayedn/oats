mod power;
mod service;
mod worker;
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use std::{
    io::{Read, Write},
    process::{Command, Stdio},
};

const BIN: &str = "/Library/PrivilegedHelperTools/dev.oats";
const ROOT: &str = "/Library/Application Support/oats";

#[derive(Parser)]
#[command(
    version,
    about = "A fast computer scheduler and toolkit. All in one.",
    long_about = "oats is a fast computer scheduler and toolkit. All in one.\n\nmacOS preview: one-time schedules, AC-only execution, and JSON output."
)]
struct Cli {
    #[command(subcommand)]
    command: Action,
}
#[derive(Subcommand)]
enum Action {
    /// Queue a command. Use an RFC3339 time with a timezone offset.
    Schedule {
        #[arg(long)]
        at: String,
        #[arg(long, default_value_t = 900)]
        timeout: u64,
        #[arg(long, default_value_t = 300)]
        grace: u64,
        #[arg(long)]
        cwd: Option<String>,
        #[arg(required = true, last = true)]
        command: Vec<String>,
    },
    /// Show jobs and service health.
    Status,
    /// Cancel a queued or running job.
    Cancel { id: String },
    /// Read the last 64 KiB of a job's output.
    Logs { id: String },
    /// Read-only power and installation diagnostics.
    Doctor,
    /// Schedule a harmless date command to test a closed-lid wake.
    Probe {
        #[arg(long, default_value_t = 120)]
        after: u64,
    },
    #[command(name = "--request", hide = true)]
    Request,
    #[command(hide = true)]
    Daemon,
    #[command(hide = true)]
    Cleanup,
    #[command(hide = true)]
    Worker { spec: String },
    #[command(hide = true)]
    PowerEvent {
        timestamp: i64,
        id: String,
        #[arg(action=clap::ArgAction::Set)]
        cancel: bool,
    },
}
#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    Schedule {
        at: DateTime<Utc>,
        timeout: u64,
        grace: u64,
        cwd: String,
        command: Vec<String>,
    },
    Status {},
    Cancel {
        id: String,
    },
    Logs {
        id: String,
    },
}
fn validate(r: &Request, now: DateTime<Utc>) -> Result<()> {
    match r {
        Request::Schedule {
            at,
            timeout,
            grace,
            cwd,
            command,
        } => {
            let delay = (*at - now).num_seconds();
            anyhow::ensure!(
                (30..=366 * 86400).contains(&delay),
                "time must be 30 seconds to 366 days in the future"
            );
            anyhow::ensure!(
                (1..=14400).contains(timeout),
                "timeout must be 1–14400 seconds"
            );
            anyhow::ensure!(*grace <= 3600, "grace must be at most 3600 seconds");
            anyhow::ensure!(
                cwd.starts_with('/') && !cwd.contains('\0'),
                "cwd must be an absolute path"
            );
            anyhow::ensure!(
                !command.is_empty() && command[0].starts_with('/'),
                "executable must be an absolute path"
            );
            anyhow::ensure!(
                command.iter().all(|s| !s.contains('\0'))
                    && command.iter().map(String::len).sum::<usize>() <= 32768,
                "invalid or oversized command"
            );
        }
        Request::Cancel { id } | Request::Logs { id } => {
            uuid::Uuid::parse_str(id).context("invalid job ID")?;
        }
        _ => {}
    }
    Ok(())
}
fn output(value: &impl Serialize) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}
fn rpc(r: Request) -> Result<()> {
    validate(&r, Utc::now())?;
    let bytes = serde_json::to_vec(&r)?;
    anyhow::ensure!(bytes.len() <= 65536, "serialized request exceeds 64 KiB");
    let mut p = Command::new("/usr/bin/sudo")
        .args(["-n", BIN, "--request"])
        .stdin(Stdio::piped())
        .spawn()
        .context("could not start installed helper; run scripts/install.sh first")?;
    let write_result = p.stdin.take().unwrap().write_all(&bytes);
    let status = p.wait()?;
    if !status.success() {
        // The helper/sudo already emitted the actual error. Preserve it once.
        std::process::exit(status.code().unwrap_or(1));
    }
    write_result?;
    Ok(())
}
fn main() {
    if let Err(e) = run() {
        eprintln!("{}", serde_json::json!({"error":format!("{e:#}")}));
        std::process::exit(1);
    }
}
fn run() -> Result<()> {
    // The sudoers rule permits only this exact argument, with a bounded JSON request on stdin.
    if std::env::args()
        .collect::<Vec<_>>()
        .get(1)
        .map(String::as_str)
        == Some("--request")
    {
        anyhow::ensure!(std::env::args().count() == 2, "unexpected arguments");
        let mut bytes = Vec::new();
        std::io::stdin().take(65537).read_to_end(&mut bytes)?;
        anyhow::ensure!(bytes.len() <= 65536, "request too large");
        let request: Request = serde_json::from_slice(&bytes)?;
        validate(&request, Utc::now())?;
        return output(&service::handle(request)?);
    }
    match Cli::parse().command {
        Action::Schedule {
            at,
            timeout,
            grace,
            cwd,
            command,
        } => rpc(Request::Schedule {
            at: DateTime::parse_from_rfc3339(&at)?.with_timezone(&Utc),
            timeout,
            grace,
            cwd: cwd.unwrap_or(std::env::current_dir()?.to_string_lossy().into_owned()),
            command,
        }),
        Action::Status => rpc(Request::Status {}),
        Action::Cancel { id } => rpc(Request::Cancel { id }),
        Action::Logs { id } => rpc(Request::Logs { id }),
        Action::Probe { after } => {
            anyhow::ensure!(
                (60..=86400).contains(&after),
                "after must be 60–86400 seconds"
            );
            rpc(Request::Schedule {
                at: Utc::now() + chrono::Duration::seconds(after as i64),
                timeout: 15,
                grace: 60,
                cwd: "/tmp".into(),
                command: vec!["/bin/date".into(), "-u".into()],
            })
        }
        Action::Doctor => output(&serde_json::json!({
            "installed": std::path::Path::new(BIN).exists(),
            "power": service::system("/usr/bin/pmset", &["-g", "batt"] )?,
            "scheduled_wakes": service::system("/usr/bin/pmset", &["-g", "sched"] )?,
            "sleep_settings": service::system("/usr/bin/pmset", &["-g"] )?,
            "closed_lid_verified": false,
            "note": "Run probe, close the lid on AC, then inspect status and logs. Firmware may ignore closed-lid wakes."
        })),
        Action::Daemon => service::daemon(),
        Action::Cleanup => service::cleanup(),
        Action::Worker { spec } => worker::run(&spec),
        Action::PowerEvent {
            timestamp,
            id,
            cancel,
        } => power::event(timestamp, &id, cancel),
        Action::Request => bail!("use --request"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn request() -> Request {
        Request::Schedule {
            at: Utc::now() + chrono::Duration::minutes(5),
            timeout: 30,
            grace: 60,
            cwd: "/tmp".into(),
            command: vec!["/bin/echo".into(), "$(touch /tmp/never)".into()],
        }
    }
    #[test]
    fn validates_bounds_and_absolute_executable() {
        let mut r = request();
        assert!(validate(&r, Utc::now()).is_ok());
        if let Request::Schedule { timeout, .. } = &mut r {
            *timeout = 14401;
        }
        assert!(validate(&r, Utc::now()).is_err());
        let mut r = request();
        if let Request::Schedule { command, .. } = &mut r {
            command[0] = "echo".into();
        }
        assert!(validate(&r, Utc::now()).is_err());
    }
    #[test]
    fn rejects_past_dates_and_path_ids() {
        let mut r = request();
        if let Request::Schedule { at, .. } = &mut r {
            *at = Utc::now();
        }
        assert!(validate(&r, Utc::now()).is_err());
        assert!(
            validate(
                &Request::Logs {
                    id: "../../etc/passwd".into()
                },
                Utc::now()
            )
            .is_err()
        );
    }
    #[test]
    fn rejects_unknown_request_fields() {
        assert!(serde_json::from_str::<Request>(r#"{"action":"status","uid":0}"#).is_err());
    }
}
