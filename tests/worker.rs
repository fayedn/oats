use std::{
    io::Read,
    process::{Command, Stdio},
    time::{Duration, Instant},
};
fn worker(command: &[&str], timeout: u64) -> std::process::Child {
    Command::new(env!("CARGO_BIN_EXE_oats"))
        .args([
            "worker",
            &serde_json::json!({"command":command,"timeout":timeout}).to_string(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}
fn wait(mut child: std::process::Child) -> (i32, String) {
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            let mut text = String::new();
            child
                .stdout
                .take()
                .unwrap()
                .read_to_string(&mut text)
                .unwrap();
            return (status.code().unwrap_or(-1), text);
        }
        assert!(start.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(30));
    }
}
#[test]
fn executes_literal_arguments_and_propagates_status() {
    let child = worker(&["/bin/echo", "$(not-a-command); literal"], 5);
    let (code, text) = wait(child);
    assert_eq!(code, 0);
    assert_eq!(text.trim(), "$(not-a-command); literal");
}
#[test]
fn enforces_runtime_limit() {
    let child = worker(&["/bin/sleep", "30"], 1);
    assert_eq!(wait(child).0, 124);
}
#[test]
fn daemon_pipe_loss_stops_job() {
    let mut child = worker(&["/bin/sleep", "30"], 30);
    drop(child.stdin.take());
    assert_eq!(wait(child).0, -1); // group termination includes the supervisor
}
#[test]
fn propagates_command_failure() {
    assert_eq!(wait(worker(&["/usr/bin/false"], 5)).0, 1);
}

#[test]
fn noisy_commands_are_drained_but_logs_are_capped() {
    let mut child = worker(&["/usr/bin/yes", "x"], 1);
    // Drain concurrently so the test harness does not introduce stdout backpressure.
    let mut stdout = child.stdout.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut data = Vec::new();
        stdout.read_to_end(&mut data).unwrap();
        data
    });
    // Child::wait closes its owned stdin; retain our daemon-liveness pipe separately.
    let _keepalive = child.stdin.take();
    let status = child.wait().unwrap();
    assert_eq!(status.code(), Some(124));
    assert_eq!(reader.join().unwrap().len(), 1024 * 1024);
}

#[test]
fn audit_supervisor_reports_completion_independent_of_poll_time() {
    let mut child = worker(&["/usr/bin/true"], 1);
    let _keepalive = child.stdin.take();
    std::thread::sleep(Duration::from_secs(2));
    let result = child.wait_with_output().unwrap();
    let report: serde_json::Value = serde_json::from_slice(&result.stderr).unwrap();
    assert_eq!(report["state"], "succeeded");
    assert_eq!(report["exit_code"], 0);
}
#[test]
fn audit_timeout_and_command_exit_124_are_distinct() {
    let mut timed = worker(&["/bin/sleep", "30"], 1);
    let _keepalive = timed.stdin.take();
    let timed = timed.wait_with_output().unwrap();
    let report: serde_json::Value = serde_json::from_slice(&timed.stderr).unwrap();
    assert_eq!(report["state"], "timed_out");
    let mut command = worker(&["/bin/sh", "-c", "exit 124"], 5);
    let _keepalive2 = command.stdin.take();
    let command = command.wait_with_output().unwrap();
    let report: serde_json::Value = serde_json::from_slice(&command.stderr).unwrap();
    assert_eq!(report["state"], "failed");
    assert_eq!(report["exit_code"], 124);
}
