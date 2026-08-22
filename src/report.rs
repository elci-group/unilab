//! The report shape. Every field here is a struct field (not a HashMap), and
//! `tools` is always built by walking `ToolId::ALL` in order — so the same
//! target, run twice, produces byte-identical JSON key/array ordering. Only
//! `generated_at` and each tool's `duration_ms` vary between runs.

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Ok,
    Warn,
    Fail,
    Error,
    Skipped,
    Unavailable,
}

/// Maps a 0-100 health score to a letter grade. Higher is healthier for
/// every tool's `score` by construction (parsers normalize to that
/// convention even when the underlying tool's own numbers point the other
/// way, e.g. amber's "replaceability" score).
pub fn letter_for(score: f64) -> &'static str {
    match score {
        s if s >= 97.0 => "A+",
        s if s >= 93.0 => "A",
        s if s >= 90.0 => "A-",
        s if s >= 87.0 => "B+",
        s if s >= 83.0 => "B",
        s if s >= 80.0 => "B-",
        s if s >= 77.0 => "C+",
        s if s >= 73.0 => "C",
        s if s >= 70.0 => "C-",
        s if s >= 67.0 => "D+",
        s if s >= 63.0 => "D",
        s if s >= 60.0 => "D-",
        _ => "F",
    }
}

#[derive(Debug, Serialize)]
pub struct ToolReport {
    pub tool: &'static str,
    pub purpose: &'static str,
    pub status: Status,
    pub score: Option<f64>,
    pub grade: Option<&'static str>,
    pub exit_code: Option<i32>,
    pub duration_ms: Option<u128>,
    pub summary: String,
    pub findings: Vec<String>,
    pub note: Option<String>,
    pub raw: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
pub struct Overall {
    pub score: Option<f64>,
    pub grade: Option<&'static str>,
    pub graded_tools: usize,
    pub total_tools: usize,
    /// (tool key, weight) pairs, in the same fixed alphabetical order as
    /// `tools`, so weights are auditable without re-deriving them.
    pub weights: Vec<(String, f64)>,
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub schema: &'static str,
    pub target: String,
    pub generated_at: String,
    pub tools_dir: String,
    pub tools: Vec<ToolReport>,
    pub overall: Overall,
}

impl Report {
    pub fn compute_overall(tools: &[ToolReport]) -> Overall {
        let mut weighted_sum = 0.0;
        let mut weight_total = 0.0;
        let mut weights = Vec::new();

        for t in tools {
            if let Some(score) = t.score {
                weights.push((t.tool.to_string(), 1.0));
                weighted_sum += score;
                weight_total += 1.0;
            }
        }

        let score = if weight_total > 0.0 {
            Some(weighted_sum / weight_total)
        } else {
            None
        };

        Overall {
            score,
            grade: score.map(letter_for),
            graded_tools: weights.len(),
            total_tools: tools.len(),
            weights,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letter_boundaries() {
        assert_eq!(letter_for(100.0), "A+");
        assert_eq!(letter_for(97.0), "A+");
        assert_eq!(letter_for(96.9), "A");
        assert_eq!(letter_for(90.0), "A-");
        assert_eq!(letter_for(89.9), "B+");
        assert_eq!(letter_for(60.0), "D-");
        assert_eq!(letter_for(59.9), "F");
        assert_eq!(letter_for(0.0), "F");
    }

    fn tool_report(tool: &'static str, score: Option<f64>) -> ToolReport {
        ToolReport {
            tool,
            purpose: "test",
            status: Status::Ok,
            score,
            grade: score.map(letter_for),
            exit_code: Some(0),
            duration_ms: Some(1),
            summary: String::new(),
            findings: Vec::new(),
            note: None,
            raw: None,
        }
    }

    #[test]
    fn overall_ignores_ungraded_tools() {
        let tools = vec![
            tool_report("amber", Some(80.0)),
            tool_report("bart", None),
            tool_report("traci", Some(100.0)),
        ];
        let overall = Report::compute_overall(&tools);
        assert_eq!(overall.graded_tools, 2);
        assert_eq!(overall.total_tools, 3);
        assert!((overall.score.unwrap() - 90.0).abs() < 1e-9);
        assert_eq!(overall.grade, Some("A-"));
    }

    #[test]
    fn overall_is_none_when_nothing_graded() {
        let tools = vec![tool_report("bart", None)];
        let overall = Report::compute_overall(&tools);
        assert_eq!(overall.score, None);
        assert_eq!(overall.grade, None);
    }
}
