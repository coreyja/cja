//! Boot-child harness: each scenario re-executes the test binary so the real,
//! process-global `cja::setup::setup_tracing` runs against real env vars. The
//! workspace forbids unsafe code, so `std::env::set_var` is not available.
//!
//! The parent half spawns the child (`spawn`), watches the loopback mocks and
//! the child's stdout, and steps the child through its phases by writing lines
//! to its stdin (`Running::release`). The child half (`child`) boots tracing,
//! runs one closure per phase, then flushes and exits.
use super::{Captures, payload};
use std::{
    io::Write as _,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

const CHILD: &str = "CJA_POSTHOG_BOOT_CHILD";
/// Printed by the child once `setup_tracing` has returned `Ok`.
pub const BOOTED: &str = "boot-child-booted";
pub const EYES_ENV: [(&str, &str); 3] = [
    ("EYES_TRANSPORT", "http"),
    ("EYES_ORG_ID", "11111111-1111-4111-8111-111111111111"),
    ("EYES_APP_ID", "22222222-2222-4222-8222-222222222222"),
];
/// Bound on `TelemetryShutdownHandle::shutdown`: the 3 s budget plus slack.
pub const SHUTDOWN_BOUND: Duration = Duration::from_millis(3500);

pub fn is_child() -> bool {
    std::env::var_os(CHILD).is_some()
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
}

/// Runs async parent-side test code on a fresh runtime.
pub fn block_on<T>(test: impl Future<Output = T>) -> T {
    runtime().block_on(test)
}

fn wait_for_release() {
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).unwrap();
}

/// The child half. Boots the real tracing stack, then runs each phase and
/// waits for the parent's release after it. Emission in every phase must stay
/// under one second, and the final flush must fit [`SHUTDOWN_BOUND`]; the
/// flush duration is handed to `after_shutdown` for scenario-specific checks.
pub fn child(phases: &[&dyn Fn()], after_shutdown: impl FnOnce(Duration)) {
    runtime().block_on(async {
        let handle = cja::setup::setup_tracing("fixture").unwrap();
        println!("{BOOTED}");
        for phase in phases {
            let start = Instant::now();
            phase();
            assert!(
                start.elapsed() < Duration::from_secs(1),
                "emission exceeded one second"
            );
            wait_for_release();
        }
        let start = Instant::now();
        handle.shutdown().await.unwrap();
        let elapsed = start.elapsed();
        assert!(
            elapsed < SHUTDOWN_BOUND,
            "telemetry shutdown took {elapsed:?}, over the {SHUTDOWN_BOUND:?} kill-window bound"
        );
        after_shutdown(elapsed);
    });
}

/// The 25 distinct app markers plus a span with an event inside it.
pub fn emit_markers() {
    for i in 0..25 {
        tracing::info!(target:"fixture",message=%format!("boot-marker-{i:02}"));
    }
    let _span = tracing::info_span!(target:"fixture","boot-span").entered();
    tracing::info!(target:"fixture","span-marker");
}

pub fn has_markers(payload: &str) -> bool {
    (0..25).all(|i| payload.contains(&format!("boot-marker-{i:02}")))
        && payload.contains("span-marker")
}

pub struct Running {
    child: std::process::Child,
    dir: tempfile::TempDir,
}

pub struct Exit {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

/// Re-executes this test binary as the child for `test` (its exact libtest
/// name) with `envs` added. Output goes to files, not pipes, so a chatty
/// child can never block on a full pipe.
pub fn spawn(test: &str, envs: &[(&str, &str)]) -> Running {
    let dir = tempfile::tempdir().unwrap();
    let file = |name: &str| std::fs::File::create(dir.path().join(name)).unwrap();
    let mut command = Command::new(std::env::current_exe().unwrap());
    cja::testing::scrub_telemetry_env(&mut command);
    let child = command
        .args(["--exact", test, "--nocapture", "--test-threads=1"])
        .stdin(Stdio::piped())
        .stdout(file("stdout"))
        .stderr(file("stderr"))
        .env(CHILD, "1")
        .env("RUST_LOG", "info")
        .env_remove("JSON_LOGS")
        .envs(envs.iter().copied())
        .spawn()
        .unwrap();
    Running { child, dir }
}

impl Running {
    pub fn stdout(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("stdout")).unwrap_or_default()
    }

    /// Lets the child move past its current phase.
    pub fn release(&mut self) {
        if let Some(stdin) = self.child.stdin.as_mut() {
            // A child that already died is reported by `finish`.
            let _ = stdin.write_all(b"\n");
            let _ = stdin.flush();
        }
    }

    /// Releases every remaining phase and waits for the child to exit.
    pub fn finish(mut self) -> Exit {
        drop(self.child.stdin.take());
        let success = self.child.wait().unwrap().success();
        Exit {
            success,
            stdout: self.stdout(),
            stderr: std::fs::read_to_string(self.dir.path().join("stderr")).unwrap_or_default(),
        }
    }
}

impl Exit {
    pub fn output(&self) -> String {
        format!(
            "--- child stdout ---\n{}\n--- child stderr ---\n{}",
            self.stdout, self.stderr
        )
    }
}

/// Polls `condition` every 50 ms for up to ten seconds.
pub async fn eventually(condition: impl Fn() -> bool) -> bool {
    let start = Instant::now();
    loop {
        if condition() {
            return true;
        }
        if start.elapsed() > Duration::from_secs(10) {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Everything the Eyes mock has received so far.
pub fn eyes_payload(eyes: &Captures) -> String {
    payload(eyes, |_| true)
}
