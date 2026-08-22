use std::collections::BTreeMap;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ingauge_gate::Admitter;

use crate::cli::AnalyzeOptions;
use crate::parsers;
use crate::report::{letter_for, Overall, Report, Status, ToolReport};
use crate::tool::{self, ToolId};

pub async fn execute(opts: &AnalyzeOptions) -> Result<Report, String> {
    let target = std::fs::canonicalize(&opts.target)
        .map_err(|e| format!("target path {:?} is not accessible: {e}", opts.target))?;

    let tools_dir = opts
        .tools_dir
        .clone()
        .unwrap_or_else(tool::default_tools_dir);

    let only: Vec<String> = opts.only.iter().map(|s| s.trim().to_lowercase()).collect();
    let skip: Vec<String> = opts.skip.iter().map(|s| s.trim().to_lowercase()).collect();

    for k in only.iter().chain(skip.iter()) {
        if ToolId::from_key(k).is_none() {
            eprintln!("uni: warning: unknown tool name {k:?} in --only/--skip, ignoring");
        }
    }

    let selected = |t: ToolId| -> bool {
        if !only.is_empty() {
            return only.iter().any(|k| k == t.key());
        }
        if skip.iter().any(|k| k == t.key()) {
            return false;
        }
        t.enabled_by_default() || (t == ToolId::Jeenome && opts.jeenome)
    };

    let timeout = Duration::from_secs(opts.timeout);
    let admitter = Arc::new(Admitter::from_env());

    let mut handles: Vec<tokio::task::JoinHandle<(ToolId, ToolReport)>> = Vec::new();
    let mut immediate: BTreeMap<&'static str, ToolReport> = BTreeMap::new();

    for tool in ToolId::ALL {
        if !selected(tool) {
            let note = if tool == ToolId::Jeenome && !opts.jeenome {
                "jeenome is opt-in (needs an strace trace, not just a project path); pass --jeenome to include it".to_string()
            } else {
                "excluded via --only/--skip".to_string()
            };
            immediate.insert(tool.key(), skipped_report(tool, note));
            continue;
        }

        if tool == ToolId::Jeenome {
            let target = target.clone();
            let trace_override = opts.jeenome_trace.clone();
            let admitter = Arc::clone(&admitter);
            let tools_dir = tools_dir.clone();
            handles.push(tokio::spawn(async move {
                (
                    tool,
                    run_jeenome(target, trace_override, timeout, admitter, tools_dir).await,
                )
            }));
            continue;
        }

        if tool == ToolId::Vamos {
            let target = target.clone();
            let tools_dir = tools_dir.clone();
            handles.push(tokio::spawn(async move {
                (tool, run_vamos(target, timeout, tools_dir).await)
            }));
            continue;
        }

        let bin = match ensure_binary(tool, &tools_dir).await {
            Some(b) => b,
            None => {
                immediate.insert(
                    tool.key(),
                    unavailable_report(
                        tool,
                        format!(
                            "binary {:?} not found after attempting to clone {}/{} and install it with `baby --user`",
                            tool.key(),
                            tools_dir.display(),
                            tool.key()
                        ),
                    ),
                );
                continue;
            }
        };

        let target = target.clone();
        let admitter = Arc::clone(&admitter);
        handles.push(tokio::spawn(async move {
            (tool, run_one(tool, bin, target, timeout, admitter).await)
        }));
    }

    for h in handles {
        match h.await {
            Ok((tool, report)) => {
                immediate.insert(tool.key(), report);
            }
            Err(e) => {
                // The task panicked; we don't know which tool without the
                // join set telling us, but panics here would be a uni bug,
                // not a tool result, so surface it loudly.
                eprintln!("uni: internal error: a tool task panicked: {e}");
            }
        }
    }

    let tools: Vec<ToolReport> = ToolId::ALL
        .into_iter()
        .map(|t| {
            immediate
                .remove(t.key())
                .unwrap_or_else(|| error_report(t, "no result recorded".to_string(), None, None))
        })
        .collect();

    let overall: Overall = Report::compute_overall(&tools);

    Ok(Report {
        schema: "uni.report/v1",
        target: target.display().to_string(),
        generated_at: chrono::Utc::now().to_rfc3339(),
        tools_dir: tools_dir.display().to_string(),
        tools,
        overall,
    })
}

/// Resolve an installed tool, or bootstrap its associated elci-group checkout
/// and install it through Baby. Bootstrap is deliberately attempted only for
/// selected tools: skipped/opt-in tools never cause network or filesystem work.
async fn ensure_binary(tool: ToolId, tools_dir: &Path) -> Option<PathBuf> {
    if let Some(bin) = tool::resolve_binary(tool, tools_dir) {
        return Some(bin);
    }

    let repo = tools_dir.join(tool.repo_dir());
    if !repo.is_dir() {
        if let Err(e) = std::fs::create_dir_all(tools_dir) {
            eprintln!(
                "uni: could not create tools directory {} for {}: {e}",
                tools_dir.display(),
                tool.key()
            );
            return None;
        }

        let git = match tool::resolve_binary_by_name("git") {
            Some(bin) => bin,
            None => {
                eprintln!("uni: cannot bootstrap {}: git is not installed", tool.key());
                return None;
            }
        };
        let url = tool.repo_url();
        let mut cmd = tokio::process::Command::new(git);
        cmd.args(["clone", "--depth", "1", url])
            .arg(&repo)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .stdin(Stdio::null());
        let output = run_bootstrap_stage(
            cmd,
            tool,
            "clone",
            format!("cloning {} from {url}", tool.key()),
        )
        .await;
        match output {
            Ok(out) if out.status.success() => {}
            Ok(out) => {
                eprintln!(
                    "uni: clone failed for {}: {}",
                    tool.key(),
                    String::from_utf8_lossy(&out.stderr)
                        .chars()
                        .take(400)
                        .collect::<String>()
                );
                return None;
            }
            Err(e) => {
                eprintln!("uni: failed to start git for {}: {e}", tool.key());
                return None;
            }
        }
    }

    let baby = match tool::resolve_binary_by_name("baby") {
        Some(bin) => bin,
        None => {
            eprintln!(
                "uni: cannot install {} from {}: baby is not installed",
                tool.key(),
                repo.display()
            );
            return None;
        }
    };
    let mut cmd = tokio::process::Command::new(baby);
    cmd.arg("--user")
        .current_dir(&repo)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .stdin(Stdio::null());
    let output = run_bootstrap_stage(
        cmd,
        tool,
        "install",
        format!("installing {} with Baby", tool.key()),
    )
    .await;
    match output {
        Ok(out) if out.status.success() => tool::resolve_binary(tool, tools_dir),
        Ok(out) => {
            eprintln!(
                "uni: baby install failed for {}: {}",
                tool.key(),
                String::from_utf8_lossy(&out.stderr)
                    .chars()
                    .take(400)
                    .collect::<String>()
            );
            None
        }
        Err(e) => {
            eprintln!("uni: failed to start baby for {}: {e}", tool.key());
            None
        }
    }
}

/// Run one visible bootstrap stage. Interactive terminals get a compact
/// spinner; redirected/CI output gets one stable start line. Every path emits
/// local-only telemetry to stderr, including spawn failures.
async fn run_bootstrap_stage(
    mut cmd: tokio::process::Command,
    tool: ToolId,
    stage: &'static str,
    label: String,
) -> io::Result<Output> {
    const FRAMES: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];

    let animated = io::stderr().is_terminal();
    if !animated {
        eprintln!("uni: {label}...");
    }

    let started = Instant::now();
    let result = if animated {
        let future = cmd.output();
        tokio::pin!(future);
        let mut ticker = tokio::time::interval(Duration::from_millis(80));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut frame = 0usize;

        loop {
            tokio::select! {
                result = &mut future => break result,
                _ = ticker.tick() => {
                    eprint!("\r\x1b[2K{} {label}", FRAMES[frame % FRAMES.len()]);
                    let _ = io::stderr().flush();
                    frame += 1;
                }
            }
        }
    } else {
        cmd.output().await
    };
    let duration_ms = started.elapsed().as_millis();

    if animated {
        eprint!("\r\x1b[2K");
        let _ = io::stderr().flush();
    }

    let (outcome, exit_code) = match &result {
        Ok(output) if output.status.success() => ("success", output.status.code()),
        Ok(output) => ("failed", output.status.code()),
        Err(_) => ("spawn_error", None),
    };
    eprintln!(
        "{}",
        bootstrap_telemetry_line(tool, stage, outcome, exit_code, duration_ms)
    );
    result
}

fn bootstrap_telemetry_line(
    tool: ToolId,
    stage: &str,
    outcome: &str,
    exit_code: Option<i32>,
    duration_ms: u128,
) -> String {
    let exit_code = exit_code
        .map(|code| code.to_string())
        .unwrap_or_else(|| "none".to_string());
    format!(
        "uni telemetry (local only): tool={} stage={stage} outcome={outcome} exit_code={exit_code} duration_ms={duration_ms}",
        tool.key()
    )
}

fn build_command(tool: ToolId, bin: &Path, target: &Path) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(bin);
    match tool {
        ToolId::Amber => {
            cmd.arg(target).args(["--format", "json", "analyze"]);
        }
        ToolId::Ami => {
            // show-project only: fast, local, no network/Groq. The full
            // `ami analyze` discovery pipeline is out of scope for a
            // concurrent snapshot run (it makes real outbound requests).
            cmd.args(["show-project", "--path"]).arg(target);
            cmd.env("NO_COLOR", "1");
        }
        ToolId::Bart => {
            cmd.args(["--json", "-d", "3", "-n", "0"]).arg(target);
        }
        ToolId::Chakra => {
            cmd.arg(target).arg("--json");
        }
        ToolId::Ferret => unreachable!("ferret is capability-probed by run_one"),
        ToolId::Fract => {
            cmd.current_dir(target);
            cmd.args(["index", "--format", "json"]);
        }
        ToolId::Isopod => {
            cmd.arg("--base").arg(target).args(["check", "--json"]);
        }
        ToolId::Lwoodz => {
            cmd.current_dir(target);
            cmd.args(["--audit", "--json"]);
        }
        ToolId::Tempcheq => {
            cmd.arg(target).arg("--report");
        }
        ToolId::Traci => {
            cmd.arg("check").arg(target).args(["--format", "json"]);
        }
        ToolId::Jeenome => unreachable!("jeenome is built by run_jeenome"),
        ToolId::Vamos => unreachable!("vamos is built by run_vamos"),
    }
    cmd
}

async fn run_one(
    tool: ToolId,
    bin: PathBuf,
    target: PathBuf,
    timeout: Duration,
    admitter: Arc<Admitter>,
) -> ToolReport {
    if let Some(provider) = tool.gate_provider() {
        let _ = admitter
            .wait_and_admit(provider, Option::<String>::None, None)
            .await;
    }

    let mut cmd = if tool == ToolId::Ferret {
        let Some(subcommand) = ferret_hunt_subcommand(&bin).await else {
            return unavailable_report(
                tool,
                "installed ferret supports neither `hunt` nor the newer project-review alias `track`; update ferret and rerun uni"
                    .to_string(),
            );
        };
        let mut cmd = tokio::process::Command::new(&bin);
        // Keep Ferret's learning corpus in memory so a uni snapshot never
        // writes ferret.db into the caller or the analyzed project.
        cmd.args(["--database", ":memory:", subcommand])
            .arg(&target);
        cmd.env("NO_COLOR", "1");
        cmd
    } else {
        build_command(tool, &bin, &target)
    };
    cmd.stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null());

    let start = Instant::now();
    let outcome = tokio::time::timeout(timeout, cmd.output()).await;
    let duration_ms = start.elapsed().as_millis();

    if let Some(provider) = tool.gate_provider() {
        admitter.complete(provider, Option::<String>::None).await;
    }

    let output = match outcome {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => {
            return error_report(
                tool,
                format!("failed to spawn: {e}"),
                None,
                Some(duration_ms),
            )
        }
        Err(_) => {
            return error_report(
                tool,
                format!("timed out after {}s", timeout.as_secs()),
                None,
                Some(duration_ms),
            )
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let exit_code = output.status.code();

    if stdout.trim().is_empty() {
        let stderr_snippet: String = stderr.chars().take(300).collect();
        return error_report(
            tool,
            format!("produced no stdout; stderr: {stderr_snippet}"),
            exit_code,
            Some(duration_ms),
        );
    }

    let parsed = parsers::parse(tool, &stdout, exit_code);
    ToolReport {
        tool: tool.key(),
        purpose: tool.purpose(),
        status: parsed.status,
        score: parsed.score,
        grade: parsed.score.map(letter_for),
        exit_code,
        duration_ms: Some(duration_ms),
        summary: parsed.summary,
        findings: parsed.findings,
        note: parsed.note,
        raw: parsed.raw,
    }
}

/// Ferret has shipped the project-wide review under both names: prefer the
/// requested `hunt` spelling and retain compatibility with builds that expose
/// it as `track`. A help probe is side-effect free and avoids guessing from a
/// version string.
async fn ferret_hunt_subcommand(bin: &Path) -> Option<&'static str> {
    for subcommand in ["hunt", "track"] {
        let mut cmd = tokio::process::Command::new(bin);
        cmd.args([subcommand, "--help"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .stdin(Stdio::null());
        if matches!(
            tokio::time::timeout(Duration::from_secs(5), cmd.status()).await,
            Ok(Ok(status)) if status.success()
        ) {
            return Some(subcommand);
        }
    }
    None
}

async fn run_jeenome(
    target: PathBuf,
    trace_override: Option<PathBuf>,
    timeout: Duration,
    admitter: Arc<Admitter>,
    tools_dir: PathBuf,
) -> ToolReport {
    if std::env::var_os("GROQ_API_KEY").is_none() {
        return skipped_report(
            ToolId::Jeenome,
            "GROQ_API_KEY is not set; jeenome streams behavioural analysis to Groq and cannot run without it".to_string(),
        );
    }

    let bin = match tool::resolve_binary(ToolId::Jeenome, &tools_dir) {
        Some(b) => b,
        None => {
            return unavailable_report(
                ToolId::Jeenome,
                "binary \"jeenome\" not found on PATH or under the tools dir".to_string(),
            )
        }
    };

    let (trace_path, generated) = match trace_override {
        Some(p) => (p, false),
        None => match generate_trace(&target, timeout).await {
            Ok(p) => (p, true),
            Err(note) => return skipped_report(ToolId::Jeenome, note),
        },
    };

    let _ = admitter
        .wait_and_admit("groq", Option::<String>::None, None)
        .await;

    let mut cmd = tokio::process::Command::new(&bin);
    cmd.arg("-i")
        .arg(&trace_path)
        .args(["--format", "json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null());

    let start = Instant::now();
    let outcome = tokio::time::timeout(timeout, cmd.output()).await;
    let duration_ms = start.elapsed().as_millis();

    admitter.complete("groq", Option::<String>::None).await;

    if generated {
        let _ = std::fs::remove_file(&trace_path);
    }

    let output = match outcome {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => {
            return error_report(
                ToolId::Jeenome,
                format!("failed to spawn: {e}"),
                None,
                Some(duration_ms),
            )
        }
        Err(_) => {
            return error_report(
                ToolId::Jeenome,
                format!("timed out after {}s", timeout.as_secs()),
                None,
                Some(duration_ms),
            )
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let exit_code = output.status.code();
    let parsed = parsers::parse(ToolId::Jeenome, &stdout, exit_code);

    ToolReport {
        tool: ToolId::Jeenome.key(),
        purpose: ToolId::Jeenome.purpose(),
        status: parsed.status,
        score: parsed.score,
        grade: parsed.score.map(letter_for),
        exit_code,
        duration_ms: Some(duration_ms),
        summary: parsed.summary,
        findings: parsed.findings,
        note: parsed.note,
        raw: parsed.raw,
    }
}

/// vamos has a real "not applicable yet" state that's structural, not an
/// error: no `vamos.toml` means the project hasn't adopted action-lifecycle
/// tracking. Checking for it up front (instead of letting `vamos stats`
/// fail with "run vamos init first") lets that report as Skipped rather
/// than Error — same reasoning as jeenome's precondition check.
async fn run_vamos(target: PathBuf, timeout: Duration, tools_dir: PathBuf) -> ToolReport {
    let manifest_path = target.join("vamos.toml");
    if !manifest_path.is_file() {
        return skipped_report(
            ToolId::Vamos,
            "no vamos.toml in project root; vamos hasn't been adopted here yet — run `vamos init` in the target to start tracking action lifecycles".to_string(),
        );
    }

    let bin = match tool::resolve_binary(ToolId::Vamos, &tools_dir) {
        Some(b) => b,
        None => {
            return unavailable_report(
                ToolId::Vamos,
                "binary \"vamos\" not found on PATH or under the tools dir".to_string(),
            )
        }
    };

    let mut cmd = tokio::process::Command::new(&bin);
    cmd.arg("--manifest")
        .arg(&manifest_path)
        .arg("--session")
        .arg(target.join(".vamos/session.json"))
        .arg("stats")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null());

    let start = Instant::now();
    let outcome = tokio::time::timeout(timeout, cmd.output()).await;
    let duration_ms = start.elapsed().as_millis();

    let output = match outcome {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => {
            return error_report(
                ToolId::Vamos,
                format!("failed to spawn: {e}"),
                None,
                Some(duration_ms),
            )
        }
        Err(_) => {
            return error_report(
                ToolId::Vamos,
                format!("timed out after {}s", timeout.as_secs()),
                None,
                Some(duration_ms),
            )
        }
    };

    let exit_code = output.status.code();
    if exit_code != Some(0) {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let snippet: String = stderr.chars().take(300).collect();
        return error_report(
            ToolId::Vamos,
            format!("vamos stats exited {exit_code:?}: {snippet}"),
            exit_code,
            Some(duration_ms),
        );
    }

    // Unlike the other tools, empty stdout is a legitimate result here
    // (vamos.toml exists but no instances recorded yet) — the parser
    // handles that itself rather than treating it as an error.
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let parsed = parsers::parse(ToolId::Vamos, &stdout, exit_code);

    ToolReport {
        tool: ToolId::Vamos.key(),
        purpose: ToolId::Vamos.purpose(),
        status: parsed.status,
        score: parsed.score,
        grade: parsed.score.map(letter_for),
        exit_code,
        duration_ms: Some(duration_ms),
        summary: parsed.summary,
        findings: parsed.findings,
        note: parsed.note,
        raw: parsed.raw,
    }
}

/// Best-effort: strace a `cargo build` in the target so jeenome has
/// something to analyze. Only attempted for Cargo projects, and only when
/// `strace` is available — jeenome has no meaning for a bare project path.
async fn generate_trace(target: &Path, timeout: Duration) -> Result<PathBuf, String> {
    if !target.join("Cargo.toml").is_file() {
        return Err("no Cargo.toml under target; jeenome needs an strace log and uni only knows how to generate one for `cargo build` (pass --jeenome-trace to supply your own)".to_string());
    }
    let strace_bin = tool::resolve_binary_by_name("strace")
        .ok_or_else(|| "strace is not on PATH and no --jeenome-trace was given".to_string())?;

    let trace_path = std::env::temp_dir().join(format!("uni-jeenome-{}.trace", std::process::id()));

    let mut cmd = tokio::process::Command::new(&strace_bin);
    cmd.args(["-f", "-o"])
        .arg(&trace_path)
        .arg("--")
        .arg("cargo")
        .arg("build")
        .arg("--manifest-path")
        .arg(target.join("Cargo.toml"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .stdin(Stdio::null());

    match tokio::time::timeout(timeout, cmd.status()).await {
        Ok(Ok(_)) if trace_path.is_file() => Ok(trace_path),
        Ok(Ok(status)) => Err(format!(
            "strace/cargo build exited {status} and produced no trace file"
        )),
        Ok(Err(e)) => Err(format!("failed to run strace: {e}")),
        Err(_) => Err(format!(
            "trace generation timed out after {}s",
            timeout.as_secs()
        )),
    }
}

fn skipped_report(tool: ToolId, note: String) -> ToolReport {
    ToolReport {
        tool: tool.key(),
        purpose: tool.purpose(),
        status: Status::Skipped,
        score: None,
        grade: None,
        exit_code: None,
        duration_ms: None,
        summary: "skipped".to_string(),
        findings: Vec::new(),
        note: Some(note),
        raw: None,
    }
}

fn unavailable_report(tool: ToolId, note: String) -> ToolReport {
    ToolReport {
        tool: tool.key(),
        purpose: tool.purpose(),
        status: Status::Unavailable,
        score: None,
        grade: None,
        exit_code: None,
        duration_ms: None,
        summary: "unavailable".to_string(),
        findings: Vec::new(),
        note: Some(note),
        raw: None,
    }
}

fn error_report(
    tool: ToolId,
    note: String,
    exit_code: Option<i32>,
    duration_ms: Option<u128>,
) -> ToolReport {
    ToolReport {
        tool: tool.key(),
        purpose: tool.purpose(),
        status: Status::Error,
        score: None,
        grade: None,
        exit_code,
        duration_ms,
        summary: "error".to_string(),
        findings: Vec::new(),
        note: Some(note),
        raw: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bootstrap_telemetry_is_explicit_and_local_only() {
        let line = bootstrap_telemetry_line(ToolId::Ferret, "install", "success", Some(0), 42);
        assert_eq!(
            line,
            "uni telemetry (local only): tool=ferret stage=install outcome=success exit_code=0 duration_ms=42"
        );
    }

    #[test]
    fn bootstrap_telemetry_handles_spawn_errors_without_fake_exit_codes() {
        let line = bootstrap_telemetry_line(ToolId::Tempcheq, "clone", "spawn_error", None, 7);
        assert!(line.contains("outcome=spawn_error"));
        assert!(line.contains("exit_code=none"));
    }
}
