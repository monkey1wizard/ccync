use std::collections::BTreeMap;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeRequest {
    pub program: PathBuf,
    pub argv: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub working_directory: PathBuf,
    pub timeout: Duration,
    pub output_byte_cap: usize,
}

impl ProbeRequest {
    pub fn redacted_argv(&self) -> Vec<String> {
        let mut result = Vec::with_capacity(self.argv.len());
        let mut redact_next = false;
        for arg in &self.argv {
            if redact_next {
                result.push("<redacted>".into());
                redact_next = false;
                continue;
            }
            if is_secret_flag(arg) {
                result.push(arg.clone());
                redact_next = true;
            } else {
                result.push(redact_inline(arg));
            }
        }
        result
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProbeOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub spawn_error: Option<String>,
}

pub trait ProbeRunner {
    fn run(&self, request: &ProbeRequest) -> ProbeOutput;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct SystemProbeRunner;

impl ProbeRunner for SystemProbeRunner {
    fn run(&self, request: &ProbeRequest) -> ProbeOutput {
        let mut command = Command::new(&request.program);
        command
            .args(&request.argv)
            .envs(&request.env)
            .current_dir(&request.working_directory)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                return ProbeOutput {
                    spawn_error: Some(error.to_string()),
                    ..Default::default()
                }
            }
        };
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let cap = request.output_byte_cap;
        let out_thread = thread::spawn(move || drain_capped(stdout, cap));
        let err_thread = thread::spawn(move || drain_capped(stderr, cap));
        let start = Instant::now();
        let mut timed_out = false;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) if start.elapsed() >= request.timeout => {
                    timed_out = true;
                    let _ = child.kill();
                    break child.wait().ok();
                }
                Ok(None) => thread::sleep(Duration::from_millis(5)),
                Err(_) => {
                    let _ = child.kill();
                    break child.wait().ok();
                }
            }
        };
        let (stdout, stdout_truncated) = out_thread.join().unwrap_or_default();
        let (stderr, stderr_truncated) = err_thread.join().unwrap_or_default();
        ProbeOutput {
            stdout,
            stderr,
            exit_code: status.and_then(|s| s.code()),
            timed_out,
            stdout_truncated,
            stderr_truncated,
            spawn_error: None,
        }
    }
}

fn drain_capped<R: Read>(mut reader: R, cap: usize) -> (Vec<u8>, bool) {
    let mut kept = Vec::with_capacity(cap.min(8192));
    let mut truncated = false;
    let mut chunk = [0u8; 8192];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(count) => {
                let remaining = cap.saturating_sub(kept.len());
                kept.extend_from_slice(&chunk[..count.min(remaining)]);
                truncated |= count > remaining;
            }
        }
    }
    (kept, truncated)
}

fn is_secret_flag(s: &str) -> bool {
    matches!(
        s.to_ascii_lowercase().as_str(),
        "--token" | "--password" | "--secret" | "--auth"
    )
}

fn secret_key(s: &str) -> bool {
    let key = s.to_ascii_lowercase();
    ["token", "password", "secret", "auth"]
        .iter()
        .any(|part| key.contains(part))
}

fn redact_inline(input: &str) -> String {
    if !input.contains("://") && !input.contains('?') {
        if let Some((key, _)) = input.split_once('=') {
            if secret_key(key) {
                return format!("{key}=<redacted>");
            }
        }
    }
    let mut value = input.to_string();
    if let Some(scheme) = value.find("://") {
        let authority_start = scheme + 3;
        let authority_end = value[authority_start..]
            .find(&['/', '?', '#'][..])
            .map(|i| authority_start + i)
            .unwrap_or(value.len());
        if let Some(at) = value[authority_start..authority_end].rfind('@') {
            value.replace_range(authority_start..authority_start + at, "<redacted>");
        }
    }
    if let Some(query_at) = value.find('?') {
        let end = value[query_at..]
            .find('#')
            .map(|i| query_at + i)
            .unwrap_or(value.len());
        let query = value[query_at + 1..end]
            .split('&')
            .map(|part| {
                if let Some((key, _)) = part.split_once('=') {
                    if secret_key(key) {
                        return format!("{key}=<redacted>");
                    }
                }
                part.to_string()
            })
            .collect::<Vec<_>>()
            .join("&");
        value.replace_range(query_at + 1..end, &query);
    }
    value
}

#[cfg(test)]
#[derive(Debug, Default)]
pub struct FixtureProbeRunner {
    outputs: std::sync::Mutex<Vec<ProbeOutput>>,
    requests: std::sync::Mutex<Vec<ProbeRequest>>,
}

#[cfg(test)]
impl FixtureProbeRunner {
    pub fn new(outputs: impl IntoIterator<Item = ProbeOutput>) -> Self {
        Self {
            outputs: std::sync::Mutex::new(outputs.into_iter().collect()),
            requests: Default::default(),
        }
    }
    pub fn requests(&self) -> Vec<ProbeRequest> {
        self.requests.lock().unwrap().clone()
    }
}

#[cfg(test)]
impl ProbeRunner for FixtureProbeRunner {
    fn run(&self, request: &ProbeRequest) -> ProbeOutput {
        self.requests.lock().unwrap().push(request.clone());
        let mut outputs = self.outputs.lock().unwrap();
        if outputs.is_empty() {
            ProbeOutput::default()
        } else {
            outputs.remove(0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn request(program: &str, argv: &[&str], cap: usize, timeout: Duration) -> ProbeRequest {
        ProbeRequest {
            program: PathBuf::from(program),
            argv: argv.iter().map(|s| (*s).into()).collect(),
            env: BTreeMap::new(),
            working_directory: std::env::current_dir().unwrap(),
            timeout,
            output_byte_cap: cap,
        }
    }
    #[test]
    fn cargo_version_succeeds() {
        let out = SystemProbeRunner.run(&request(
            "cargo",
            &["--version"],
            4096,
            Duration::from_secs(10),
        ));
        assert_eq!(out.exit_code, Some(0), "{out:?}");
        assert!(!out.stdout.is_empty());
    }
    #[test]
    fn output_is_capped_and_truncation_reported() {
        #[cfg(windows)]
        let r = request(
            "cmd",
            &["/C", "echo", "123456789"],
            3,
            Duration::from_secs(5),
        );
        #[cfg(not(windows))]
        let r = request("printf", &["123456789"], 3, Duration::from_secs(5));
        let out = SystemProbeRunner.run(&r);
        assert_eq!(out.stdout.len(), 3);
        assert!(out.stdout_truncated);
    }
    #[test]
    fn preserves_nonzero_exit() {
        #[cfg(windows)]
        let r = request("cmd", &["/C", "exit", "7"], 32, Duration::from_secs(5));
        #[cfg(not(windows))]
        let r = request("sh", &["-c", "exit 7"], 32, Duration::from_secs(5));
        assert_eq!(SystemProbeRunner.run(&r).exit_code, Some(7));
    }
    #[test]
    fn timeout_kills_child() {
        #[cfg(windows)]
        let r = request(
            "ping",
            &["-n", "30", "127.0.0.1"],
            64,
            Duration::from_millis(100),
        );
        #[cfg(not(windows))]
        let r = request("sleep", &["30"], 64, Duration::from_millis(100));
        let out = SystemProbeRunner.run(&r);
        assert!(out.timed_out);
        assert!(out.exit_code.is_none() || out.exit_code != Some(0));
    }
    #[test]
    fn redacts_credentials() {
        let r = request(
            "/bin/true",
            &[
                "--token",
                "token_val",
                "--password",
                "pass_val",
                "--secret",
                "secret_val",
                "--auth",
                "auth_val",
                "--token=token_inline",
                "--password=pass_inline",
                "--secret=secret_inline",
                "--auth=auth_inline",
                "TOKEN=value",
                "MY_SECRET_KEY=val1",
                "AUTH_HEADER=val2",
                "USER_PASSWORD=val3",
                "SAFE_ENV=ok",
                "https://user:pass@example.test/p?access_token=tok1&client_secret=sec2&auth_code=tok3&user_password=pw4&public_id=123",
            ],
            1,
            Duration::from_secs(1),
        );
        assert_eq!(
            r.redacted_argv(),
            [
                "--token",
                "<redacted>",
                "--password",
                "<redacted>",
                "--secret",
                "<redacted>",
                "--auth",
                "<redacted>",
                "--token=<redacted>",
                "--password=<redacted>",
                "--secret=<redacted>",
                "--auth=<redacted>",
                "TOKEN=<redacted>",
                "MY_SECRET_KEY=<redacted>",
                "AUTH_HEADER=<redacted>",
                "USER_PASSWORD=<redacted>",
                "SAFE_ENV=ok",
                "https://<redacted>@example.test/p?access_token=<redacted>&client_secret=<redacted>&auth_code=<redacted>&user_password=<redacted>&public_id=123",
            ]
        );
    }
    #[test]
    fn fixture_records_requests_and_returns_scripted_output() {
        let expected = ProbeOutput {
            exit_code: Some(4),
            ..Default::default()
        };
        let fixture = FixtureProbeRunner::new([expected.clone()]);
        let req = request("tool", &["arg"], 2, Duration::from_secs(1));
        assert_eq!(fixture.run(&req), expected);
        assert_eq!(fixture.requests(), [req]);
    }
}
