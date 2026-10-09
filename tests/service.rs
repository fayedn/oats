use serde_json::{Value, json};
use std::{
    fs,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

#[test]
fn user_agent_handles_overdue_jobs_and_cleanup_without_admin() {
    let home = tempfile::tempdir().unwrap();
    let root = home.path().join("Library/Application Support/oats");
    fs::create_dir_all(root.join("logs")).unwrap();
    let job = |id: &str, at: chrono::DateTime<chrono::Utc>, grace| {
        json!({
            "id":id,"at":at,"timeout":5,"grace":grace,"cwd":"/tmp",
            "command":["/usr/bin/true"],"state":"queued","started":null,
            "finished":null,"exit_code":null,"error":null,"cancel":false
        })
    };
    let now = chrono::Utc::now();
    fs::write(
        root.join("state.json"),
        serde_json::to_vec(&json!({"jobs":[
        job("00000000-0000-4000-8000-000000000001", now-chrono::Duration::hours(1), 60),
        job("00000000-0000-4000-8000-000000000002", now+chrono::Duration::hours(1), 60)
    ],"heartbeat":null}))
        .unwrap(),
    )
    .unwrap();
    let cli = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_oats"))
            .env("HOME", home.path())
            .args(args)
            .output()
            .unwrap()
    };
    let mut daemon = Command::new(env!("CARGO_BIN_EXE_oats"))
        .env("HOME", home.path())
        .arg("daemon")
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(8);
    let observed = loop {
        let result = cli(&["status"]);
        let status: Value = serde_json::from_slice(&result.stdout).unwrap();
        if status["jobs"][0]["state"] == "missed" {
            break true;
        }
        if Instant::now() >= deadline {
            break false;
        }
        thread::sleep(Duration::from_millis(100));
    };
    unsafe {
        libc::kill(daemon.id() as i32, libc::SIGTERM);
    }
    assert!(daemon.wait().unwrap().success());
    assert!(observed, "overdue job was not marked missed");
    let result = cli(&["cleanup"]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let status: Value = serde_json::from_slice(&cli(&["status"]).stdout).unwrap();
    assert_eq!(status["daemon_healthy"], false);
    assert_eq!(status["jobs"][1]["state"], "cancelled");
    assert!(!root.join("config.json").exists());
}
