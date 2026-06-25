use std::io::Write;
use std::path::PathBuf;
use std::process::{Command as ProcessCommand, Stdio};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::config::SummaryConfig;
use crate::exec::{CapturedRun, format_command};
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

    let mut child = ProcessCommand::new(&summarizer_argv[0])
        .args(&summarizer_argv[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| {
            format!(
                "starting summary command `{}`",
                format_command(summarizer_argv)
            )
        })?;

    if let Some(stdin) = child.stdin.as_mut() {
        stdin
            .write_all(prompt.as_bytes())
            .context("writing prompt to summary command")?;
    }

    let output = child
        .wait_with_output()
        .context("waiting for summary command")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "summary command exited with {}: {}",
            output.status.code().unwrap_or(1),
            stderr.trim()
        );
    }

    let summary_text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if summary_text.is_empty() {
        return Ok(summarize(run, config));
    }

    Ok(Summary {
        command: format_command(&run.command),
        exit_code: exit_code(run),
        duration_ms: run.elapsed.as_millis(),
        preview_lines: summary_text.lines().map(str::to_owned).collect(),
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
