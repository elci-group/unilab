//! `uni revise`: diagnose a project with the same tools `uni analyze` uses,
//! then hand any flagged issue to the one elci-suite tool that actually
//! knows how to fix it. Only four of the eleven tools have a real
//! remediation command:
//!
//! - amber `--propose`  — generates replacement modules for flagged
//!   dependencies. Always non-destructive (writes only to
//!   `amber_proposals/`, never touches `Cargo.toml` or source).
//! - isopod `harden --apply` — creates missing compliance evidence files.
//! - lwoodz `--generate` — writes a missing `LICENSE`/`NOTICE`/etc. Only
//!   offered when the diagnosis shows no license file at all; it has no
//!   fix for header coverage or compatibility warnings.
//! - tempcheq `--fix --yes` — rewrites high-confidence temperature
//!   deviations in place.
//!
//! ami, bart, chakra, ferret, fract, jeenome, traci, and vamos have no
//! remediation command in this ecosystem (fract's LLM refactor engine
//! lives behind its daemon, not a one-shot fix, and is deliberately out
//! of scope here). Remediation runs sequentially, not concurrently —
//! several of these mutate files, and predictable ordering beats speed
//! when uni is about to change the working tree.
//!
//! Every remediation is planned, not run, unless `--apply` is passed:
//! uni always shows the exact command first.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::Value;

use crate::cli::ReviseArgs;
use crate::report::{Report, Status};
use crate::run;
use crate::tool::{self, ToolId};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// `--apply` was not passed; this is what would run.
    Planned,
    /// Ran and exited 0.
    Applied,
    /// Ran and failed, or couldn't be spawned/timed out.
    Failed,
    /// The tool's binary isn't installed.
    Unavailable,
}

#[derive(Debug, Serialize)]
pub struct Remediation {
    pub tool: &'static str,
    pub reason: String,
    pub command: String,
    pub outcome: Outcome,
    pub detail: Option<String>,
    pub duration_ms: Option<u128>,
}

#[derive(Debug, Serialize)]
pub struct ReviseReport {
    pub schema: &'static str,
    pub target: String,
    pub apply: bool,
    pub diagnosis: Report,
    pub remediations: Vec<Remediation>,
    /// Re-diagnosis after remediation, only present when at least one
    /// remediation actually ran (`--apply` and something applied).
    pub post: Option<Report>,
}

pub async fn execute(args: &ReviseArgs) -> Result<ReviseReport, String> {
    let opts = args.analyze_options();
    let diagnosis = run::execute(&opts).await?;
    let target = PathBuf::from(&diagnosis.target);

    let only: Vec<String> = args.only.iter().map(|s| s.trim().to_lowercase()).collect();
    let skip: Vec<String> = args.skip.iter().map(|s| s.trim().to_lowercase()).collect();
    let wants = |key: &str| -> bool {
        if !only.is_empty() {
            return only.iter().any(|k| k == key);
        }
        !skip.iter().any(|k| k == key)
    };

    let tools_dir = args
        .tools_dir
        .clone()
        .unwrap_or_else(tool::default_tools_dir);
    let timeout = Duration::from_secs(args.timeout);

    let plan = build_plan(&diagnosis, &target, wants);
    let mut applied_anything = false;
    let mut remediations = Vec::with_capacity(plan.len());

    for planned in plan {
        let command = describe(&planned);

        let bin = match tool::resolve_binary(planned.tool, &tools_dir) {
            Some(b) => b,
            None => {
                remediations.push(Remediation {
                    tool: planned.tool.key(),
                    reason: planned.reason,
                    command,
                    outcome: Outcome::Unavailable,
                    detail: Some(format!("binary {:?} not found", planned.tool.key())),
                    duration_ms: None,
                });
                continue;
            }
        };

        if !args.apply {
            remediations.push(Remediation {
                tool: planned.tool.key(),
                reason: planned.reason,
                command,
                outcome: Outcome::Planned,
                detail: None,
                duration_ms: None,
            });
            continue;
        }

        let mut cmd = tokio::process::Command::new(&bin);
        cmd.args(&planned.args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null());
        if let Some(cwd) = &planned.cwd {
            cmd.current_dir(cwd);
        }

        let start = Instant::now();
        let outcome = tokio::time::timeout(timeout, cmd.output()).await;
        let duration_ms = Some(start.elapsed().as_millis());

        let (result, detail) = match outcome {
            Ok(Ok(output)) if output.status.success() => {
                applied_anything = true;
                (Outcome::Applied, tail(&output.stdout))
            }
            Ok(Ok(output)) => (
                Outcome::Failed,
                Some(format!(
                    "exit {:?}: {}",
                    output.status.code(),
                    tail(&output.stderr).unwrap_or_default()
                )),
            ),
            Ok(Err(e)) => (Outcome::Failed, Some(format!("failed to spawn: {e}"))),
            Err(_) => (
                Outcome::Failed,
                Some(format!("timed out after {}s", timeout.as_secs())),
            ),
        };

        remediations.push(Remediation {
            tool: planned.tool.key(),
            reason: planned.reason,
            command,
            outcome: result,
            detail,
            duration_ms,
        });
    }

    let post = if applied_anything {
        Some(run::execute(&opts).await?)
    } else {
        None
    };

    Ok(ReviseReport {
        schema: "uni.revise/v1",
        target: diagnosis.target.clone(),
        apply: args.apply,
        diagnosis,
        remediations,
        post,
    })
}

struct PlannedRemediation {
    tool: ToolId,
    reason: String,
    program: &'static str,
    args: Vec<String>,
    cwd: Option<PathBuf>,
}

fn build_plan(
    diagnosis: &Report,
    target: &Path,
    wants: impl Fn(&str) -> bool,
) -> Vec<PlannedRemediation> {
    let find = |key: &str| diagnosis.tools.iter().find(|t| t.tool == key);
    let flagged = |key: &str| {
        matches!(
            find(key).map(|t| t.status),
            Some(Status::Warn | Status::Fail)
        )
    };

    let mut plan = Vec::new();
    let target_str = target.display().to_string();

    if wants("amber") && flagged("amber") {
        plan.push(PlannedRemediation {
            tool: ToolId::Amber,
            reason: find("amber").unwrap().summary.clone(),
            program: "amber",
            args: vec![".".to_string(), "--propose".to_string()],
            cwd: Some(target.to_path_buf()),
        });
    }

    if wants("isopod") && flagged("isopod") {
        plan.push(PlannedRemediation {
            tool: ToolId::Isopod,
            reason: find("isopod").unwrap().summary.clone(),
            program: "isopod",
            args: vec![
                "--base".to_string(),
                target_str.clone(),
                "harden".to_string(),
                "--apply".to_string(),
            ],
            cwd: None,
        });
    }

    if wants("lwoodz") && flagged("lwoodz") {
        let t = find("lwoodz").unwrap();
        let has_license = t
            .raw
            .as_ref()
            .and_then(|v| v.get("has_license_file"))
            .and_then(Value::as_bool)
            .unwrap_or(true);
        if !has_license {
            plan.push(PlannedRemediation {
                tool: ToolId::Lwoodz,
                reason: format!("{} (no fix for header coverage/compatibility warnings — only the missing license file is addressable)", t.summary),
                program: "lwoodz",
                args: vec!["--generate".to_string()],
                cwd: Some(target.to_path_buf()),
            });
        }
    }

    if wants("tempcheq") && flagged("tempcheq") {
        plan.push(PlannedRemediation {
            tool: ToolId::Tempcheq,
            reason: find("tempcheq").unwrap().summary.clone(),
            program: "tempcheq",
            args: vec![target_str, "--fix".to_string(), "--yes".to_string()],
            cwd: None,
        });
    }

    plan
}

fn describe(p: &PlannedRemediation) -> String {
    match &p.cwd {
        Some(cwd) => format!("(cd {}; {} {})", cwd.display(), p.program, p.args.join(" ")),
        None => format!("{} {}", p.program, p.args.join(" ")),
    }
}

fn tail(bytes: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(bytes);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.chars().take(500).collect())
    }
}

pub fn human(report: &ReviseReport) -> String {
    let mut out = String::new();
    out.push_str(&format!("uni revise — {}\n", report.target));
    out.push_str(if report.apply {
        "mode: apply\n\n"
    } else {
        "mode: dry-run (pass --apply to actually run these)\n\n"
    });

    if report.remediations.is_empty() {
        out.push_str("nothing to revise — every tool with a remediation command is clean.\n");
        return out;
    }

    for r in &report.remediations {
        out.push_str(&format!("{} [{}]\n", r.tool, outcome_word(r.outcome)));
        out.push_str(&format!("  reason: {}\n", r.reason));
        out.push_str(&format!("  command: {}\n", r.command));
        if let Some(detail) = &r.detail {
            out.push_str(&format!("  detail: {detail}\n"));
        }
        out.push('\n');
    }

    if let Some(post) = &report.post {
        out.push_str("--- post-revise snapshot ---\n");
        out.push_str(&crate::render::human(post));
    }

    out
}

fn outcome_word(o: Outcome) -> &'static str {
    match o {
        Outcome::Planned => "planned",
        Outcome::Applied => "applied",
        Outcome::Failed => "failed",
        Outcome::Unavailable => "unavailable",
    }
}
