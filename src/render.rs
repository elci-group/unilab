use crate::report::{Report, Status};

pub fn human(report: &Report) -> String {
    let mut out = String::new();
    out.push_str(&format!("uni report — {}\n", report.target));
    out.push_str(&format!("generated {}\n\n", report.generated_at));

    out.push_str(&format!(
        "{:<10} {:<12} {:<6} {:>7}  {}\n",
        "TOOL", "STATUS", "GRADE", "SCORE", "PURPOSE"
    ));
    out.push_str(&"-".repeat(78));
    out.push('\n');

    for t in &report.tools {
        out.push_str(&format!(
            "{:<10} {:<12} {:<6} {:>7}  {}\n",
            t.tool,
            status_word(t.status),
            t.grade.unwrap_or("—"),
            t.score
                .map(|s| format!("{s:.1}"))
                .unwrap_or_else(|| "—".to_string()),
            t.purpose,
        ));
    }

    out.push('\n');
    for t in &report.tools {
        if matches!(t.status, Status::Skipped | Status::Unavailable) {
            if let Some(note) = &t.note {
                out.push_str(&format!("  {} — {note}\n", t.tool));
            }
            continue;
        }
        out.push_str(&format!("  {}: {}\n", t.tool, t.summary));
        for f in &t.findings {
            out.push_str(&format!("    - {f}\n"));
        }
        if let Some(note) = &t.note {
            out.push_str(&format!("    note: {note}\n"));
        }
    }

    out.push('\n');
    match report.overall.score {
        Some(score) => out.push_str(&format!(
            "OVERALL: {} ({score:.1}) across {}/{} graded tools\n",
            report.overall.grade.unwrap_or("—"),
            report.overall.graded_tools,
            report.overall.total_tools,
        )),
        None => out.push_str("OVERALL: ungraded — no tool produced a numeric score\n"),
    }

    out
}

fn status_word(s: Status) -> &'static str {
    match s {
        Status::Ok => "ok",
        Status::Warn => "warn",
        Status::Fail => "fail",
        Status::Error => "error",
        Status::Skipped => "skipped",
        Status::Unavailable => "unavailable",
    }
}
