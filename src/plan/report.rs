//! CI artifacts for executed automation plan steps.

// Rust guideline compliant 2026-10-07

use std::io::{self, Write};
use std::path::PathBuf;

use serde_json::{Value, json};

use super::PlanStep;

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub(crate) enum ReportFormat {
    Junit,
    Sarif,
}

enum Outcome {
    Passed,
    Failed(String),
    Skipped(String),
}

struct Record<'a> {
    step: &'a PlanStep,
    outcome: Outcome,
}

pub(super) struct Report<'a> {
    format: ReportFormat,
    path: PathBuf,
    records: Vec<Record<'a>>,
}

impl<'a> Report<'a> {
    pub(super) fn new(format: ReportFormat, path: PathBuf) -> Self {
        Self {
            format,
            path,
            records: Vec::new(),
        }
    }

    pub(super) fn record(&mut self, step: &'a PlanStep, event: &Value) {
        // Only a short diagnostic is retained. Action results can be whole terminal snapshots.
        let detail = |field: &str| {
            event[field]
                .as_str()
                .unwrap_or_default()
                .chars()
                .take(512)
                .collect()
        };
        let outcome = match event["status"].as_str() {
            Some("error") => Outcome::Failed(detail("error")),
            Some("skipped") => Outcome::Skipped(detail("reason")),
            _ => Outcome::Passed,
        };
        self.records.push(Record { step, outcome });
    }

    pub(super) fn write(self, failures: usize) -> io::Result<()> {
        let bytes = match self.format {
            ReportFormat::Junit => self.junit(failures)?,
            ReportFormat::Sarif => self.sarif()?,
        };
        crate::runtime::write_private_atomic(&self.path, &bytes, "plan")
    }

    fn junit(&self, failures: usize) -> io::Result<Vec<u8>> {
        let mut xml = Vec::new();
        let skipped = self
            .records
            .iter()
            .filter(|record| matches!(record.outcome, Outcome::Skipped(_)))
            .count();
        writeln!(xml, r#"<?xml version="1.0" encoding="UTF-8"?>"#)?;
        writeln!(
            xml,
            r#"<testsuite name="vvmux" tests="{}" failures="{failures}" skipped="{skipped}">"#,
            self.records.len()
        )?;
        for record in &self.records {
            write!(
                xml,
                r#"  <testcase classname="{}" name="{}">"#,
                xml_text(&record.step.method),
                xml_text(&record.step.id)
            )?;
            match &record.outcome {
                Outcome::Passed => {}
                Outcome::Failed(detail) => {
                    write!(xml, r#"<failure message="{}"/>"#, xml_text(detail))?;
                }
                Outcome::Skipped(reason) => {
                    write!(xml, r#"<skipped message="{}"/>"#, xml_text(reason))?;
                }
            }
            writeln!(xml, "</testcase>")?;
        }
        writeln!(xml, "</testsuite>")?;
        Ok(xml)
    }

    fn sarif(&self) -> io::Result<Vec<u8>> {
        let methods: std::collections::BTreeSet<_> = self
            .records
            .iter()
            .map(|record| &record.step.method)
            .collect();
        let rules: Vec<_> = methods
            .into_iter()
            .map(|method| json!({"id": method}))
            .collect();
        let results = self
            .records
            .iter()
            .map(|record| {
                let (kind, level, detail) = match &record.outcome {
                    Outcome::Passed => ("pass", "none", "ok"),
                    Outcome::Failed(detail) => ("fail", "error", detail.as_str()),
                    Outcome::Skipped(reason) => ("notApplicable", "none", reason.as_str()),
                };
                json!({
                    "ruleId": record.step.method, "kind": kind, "level": level,
                    "message": {"text": format!("{}: {detail}", record.step.id)},
                    "properties": {"stepId": record.step.id},
                })
            })
            .collect::<Vec<_>>();
        serde_json::to_vec_pretty(&json!({
            "version": "2.1.0",
            "runs": [{"tool": {"driver": {"name": "vvmux", "rules": rules}}, "results": results}],
        }))
        .map_err(io::Error::other)
    }
}

fn xml_text(text: &str) -> String {
    let mut escaped = String::new();
    for character in text.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            '\t' | '\r' | '\n' | ' '..='\u{d7ff}' | '\u{e000}'..='\u{fffd}' | '\u{10000}'.. => {
                escaped.push(character);
            }
            _ => escaped.push('\u{fffd}'),
        }
    }
    escaped
}
