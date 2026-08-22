use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "uni",
    version,
    about = "Unified analysis snapshot: runs amber, ami, bart, chakra, ferret hunt, fract, isopod, lwoodz, tempcheq, traci, and vamos (plus jeenome, opt-in) concurrently against a project and compiles one deterministic, graded report.",
    long_about = "uni points amber, ami, bart, chakra, ferret hunt (`ferret track`), fract, isopod, lwoodz, tempcheq, traci, and vamos at a project concurrently, load-balances the two that call an LLM (jeenome, lwoodz) through the local ingauge admission gate so they don't saturate shared quota, and compiles the results into a single deterministic report with a grade per tool and an overall grade. If a selected application is missing, uni clones its associated https://github.com/elci-group repository under the tools directory and installs it with `baby --user`. jeenome is opt-in (--jeenome) since it audits an strace trace, not a project path. ami only runs `show-project` (fast, local, no network); its full `analyze` discovery pipeline is not run by uni. ferret uses an in-memory corpus so the snapshot does not modify the target. vamos reports Skipped (not Error) when a project has no vamos.toml yet — it hasn't adopted action-lifecycle tracking. Run `uni revise` to have flagged tools fix their own findings."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Project to analyze. Ignored when a subcommand is given.
    #[arg(default_value = ".")]
    pub target: PathBuf,

    /// Print the report as JSON instead of a human-readable table.
    #[arg(long)]
    pub json: bool,

    /// Also write the JSON report to this file (independent of --json).
    #[arg(long)]
    pub out: Option<PathBuf>,

    /// Run only these tools (comma-separated, e.g. amber,traci).
    #[arg(long, value_delimiter = ',')]
    pub only: Vec<String>,

    /// Skip these tools (comma-separated).
    #[arg(long, value_delimiter = ',')]
    pub skip: Vec<String>,

    /// Include jeenome. Requires GROQ_API_KEY and either --jeenome-trace or
    /// a Cargo project + `strace` on PATH so uni can generate one.
    #[arg(long)]
    pub jeenome: bool,

    /// Use this existing strace log for jeenome instead of generating one.
    #[arg(long)]
    pub jeenome_trace: Option<PathBuf>,

    /// Per-tool timeout in seconds.
    #[arg(long, default_value_t = 300)]
    pub timeout: u64,

    /// Root directory containing sibling tool checkouts (default: the
    /// parent directory of uni's own source tree).
    #[arg(long)]
    pub tools_dir: Option<PathBuf>,

    /// Exit with status 1 if the overall score falls below this threshold
    /// (0-100). Omit to always exit 0 for a successful run (uni is a
    /// snapshot tool by default, not a gate).
    #[arg(long)]
    pub fail_under: Option<f64>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Run the concurrent analysis snapshot (same as passing no subcommand).
    Analyze(AnalyzeArgs),

    /// Diagnose issues, then run each flagged tool's own fix/remediation
    /// command (amber --propose, isopod harden, lwoodz --generate,
    /// tempcheq --fix). Dry-run by default; pass --apply to actually
    /// mutate the project.
    Revise(ReviseArgs),
}

#[derive(Parser, Debug)]
pub struct AnalyzeArgs {
    #[arg(default_value = ".")]
    pub target: PathBuf,

    #[arg(long)]
    pub json: bool,

    #[arg(long)]
    pub out: Option<PathBuf>,

    #[arg(long, value_delimiter = ',')]
    pub only: Vec<String>,

    #[arg(long, value_delimiter = ',')]
    pub skip: Vec<String>,

    #[arg(long)]
    pub jeenome: bool,

    #[arg(long)]
    pub jeenome_trace: Option<PathBuf>,

    #[arg(long, default_value_t = 300)]
    pub timeout: u64,

    #[arg(long)]
    pub tools_dir: Option<PathBuf>,

    #[arg(long)]
    pub fail_under: Option<f64>,
}

#[derive(Parser, Debug)]
pub struct ReviseArgs {
    /// Project to revise.
    #[arg(default_value = ".")]
    pub target: PathBuf,

    /// Actually run each tool's fix command. Without this, uni only shows
    /// what it would run (every underlying fix command is itself dry-run
    /// or plan-only by default: amber --propose never touches your source,
    /// isopod harden and lwoodz --generate need their own --apply, and
    /// tempcheq --fix needs --yes — uni passes those through only here).
    #[arg(long)]
    pub apply: bool,

    /// Only consider these tools for remediation (comma-separated).
    #[arg(long, value_delimiter = ',')]
    pub only: Vec<String>,

    /// Skip these tools during remediation (comma-separated).
    #[arg(long, value_delimiter = ',')]
    pub skip: Vec<String>,

    /// Per-tool timeout in seconds, for both the diagnostic pass and each
    /// remediation command.
    #[arg(long, default_value_t = 300)]
    pub timeout: u64,

    /// Root directory containing sibling tool checkouts.
    #[arg(long)]
    pub tools_dir: Option<PathBuf>,

    /// Print the revise plan/result as JSON instead of human-readable text.
    #[arg(long)]
    pub json: bool,
}

/// The subset of options `run::execute` needs, shared by the bare
/// `uni <target>` invocation and `uni analyze`.
pub struct AnalyzeOptions {
    pub target: PathBuf,
    pub only: Vec<String>,
    pub skip: Vec<String>,
    pub jeenome: bool,
    pub jeenome_trace: Option<PathBuf>,
    pub timeout: u64,
    pub tools_dir: Option<PathBuf>,
}

impl Cli {
    pub fn analyze_options(&self) -> AnalyzeOptions {
        AnalyzeOptions {
            target: self.target.clone(),
            only: self.only.clone(),
            skip: self.skip.clone(),
            jeenome: self.jeenome,
            jeenome_trace: self.jeenome_trace.clone(),
            timeout: self.timeout,
            tools_dir: self.tools_dir.clone(),
        }
    }
}

impl AnalyzeArgs {
    pub fn analyze_options(&self) -> AnalyzeOptions {
        AnalyzeOptions {
            target: self.target.clone(),
            only: self.only.clone(),
            skip: self.skip.clone(),
            jeenome: self.jeenome,
            jeenome_trace: self.jeenome_trace.clone(),
            timeout: self.timeout,
            tools_dir: self.tools_dir.clone(),
        }
    }
}

impl ReviseArgs {
    /// The diagnostic pass revise runs first, to see which tools are
    /// currently flagged. jeenome is never included: it has no
    /// remediation action, and forcing it on would mean generating an
    /// strace trace just to throw the result away.
    pub fn analyze_options(&self) -> AnalyzeOptions {
        AnalyzeOptions {
            target: self.target.clone(),
            only: self.only.clone(),
            skip: self.skip.clone(),
            jeenome: false,
            jeenome_trace: None,
            timeout: self.timeout,
            tools_dir: self.tools_dir.clone(),
        }
    }
}
