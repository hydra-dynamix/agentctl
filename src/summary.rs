use std::path::PathBuf;

use anyhow::{Result, bail};
use serde::Serialize;

use crate::config::SummaryConfig;
use crate::exec::{CaptureOptions, CapturedRun, ProcessOptions, format_command, run_captured_with};
use crate::output::{bounded_output, combine_output};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Summary {
    pub command: String,
    pub exit_code: i32,
    pub duration_ms: u128,
    pub preview_lines: Vec<String>,
    pub raw_log_path: PathBuf,
}

pub fn summarize(run: &CapturedRun, config: &SummaryConfig) -> Summary {
    let combined = combine_output(&run.stdout, &run.stderr);
    let bounded = bounded_output(
        &combined,
        config.max_output_bytes,
        config.tail_bytes.min(config.max_output_bytes),
    );
    Summary {
        command: format_command(&run.command),
        exit_code: exit_code(run),
        duration_ms: run.elapsed.as_millis(),
        preview_lines: notable_lines(&bounded, config.max_preview_lines),
        raw_log_path: run.raw_log_path.clone(),
    }
}

pub fn summarize_with_command(
    run: &CapturedRun,
    config: &SummaryConfig,
    summarizer_argv: &[String],
) -> Result<Summary> {
    if summarizer_argv.is_empty() {
        bail!("summary.summarizer_command must not be empty when set");
    }

    let combined = combine_output(&run.stdout, &run.stderr);
    let bounded = bounded_output(
        &combined,
        config.max_output_bytes,
        config.tail_bytes.min(config.max_output_bytes),
    );
    if bounded.trim().is_empty() {
        return Ok(summarize(run, config));
    }

    let prompt = format!(
        "Summarize this developer command result for another coding agent.\n\
         Be concise. Include the exit status, likely root cause, and next action.\n\
         Do not reproduce long traces.\n\n\
         Command: {}\n\
         Exit code: {}\n\
         Duration: {}ms\n\n\
         Captured output:\n{}\n",
        format_command(&run.command),
        exit_code(run),
        run.elapsed.as_millis(),
        bounded
    );

    let output = run_captured_with(
        summarizer_argv,
        &CaptureOptions {
            max_output_bytes: config.max_output_bytes,
            tail_bytes: config.tail_bytes,
            label: Some("summarizer".to_owned()),
            stream_raw: false,
        },
        &ProcessOptions {
            stdin: Some(prompt.into_bytes()),
            ..ProcessOptions::default()
        },
    )?;
    if !output.exit_status.success() {
        bail!(
            "summary command exited with {}: {}",
            output.exit_status.code().unwrap_or(1),
            output.stderr.trim()
        );
    }

    let summary_text = output.stdout.trim();
    if summary_text.is_empty() {
        return Ok(summarize(run, config));
    }

    Ok(Summary {
        command: format_command(&run.command),
        exit_code: exit_code(run),
        duration_ms: run.elapsed.as_millis(),
        preview_lines: summary_text
            .lines()
            .take(config.max_preview_lines)
            .map(str::to_owned)
            .collect(),
        raw_log_path: run.raw_log_path.clone(),
    })
}

pub fn notable_lines(output: &str, max_preview_lines: usize) -> Vec<String> {
    if output.trim().is_empty() {
        return Vec::new();
    }

    let interesting = output
        .lines()
        .filter(|line| {
            let lowered = line.to_ascii_lowercase();
            lowered.contains("error")
                || lowered.contains("failed")
                || lowered.contains("panic")
                || lowered.contains("exception")
                || lowered.contains("warning")
                || lowered.contains("traceback")
        })
        .take(max_preview_lines)
        .map(str::to_owned)
        .collect::<Vec<_>>();

    if !interesting.is_empty() {
        return interesting;
    }

    let mut tail = output
        .lines()
        .rev()
        .take(max_preview_lines)
        .map(str::to_owned)
        .collect::<Vec<_>>();
    tail.reverse();
    tail
}

fn exit_code(run: &CapturedRun) -> i32 {
    run.exit_status.code().unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notable_lines_prefers_error_warning_and_failure_lines() {
        let output = "first\nwarning: be careful\nmiddle\nerror: broke\nfailed test\nlast\n";

        let lines = notable_lines(output, 2);

        assert_eq!(lines, vec!["warning: be careful", "error: broke"]);
    }

    #[test]
    fn notable_lines_falls_back_to_tail() {
        let output = "one\ntwo\nthree\nfour\n";

        let lines = notable_lines(output, 2);

        assert_eq!(lines, vec!["three", "four"]);
    }
}
