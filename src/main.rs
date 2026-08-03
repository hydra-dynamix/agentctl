use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "agentctl")]
#[command(about = "Compact, bounded command and agent reports for LLM tool loops")]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Exec {
        #[arg(long)]
        json: bool,
        #[arg(long)]
        raw: bool,
        #[arg(long)]
        label: Option<String>,
        #[arg(required = true, trailing_var_arg = true)]
        argv: Vec<String>,
    },
    Run {
        agent: Option<String>,
        #[arg(long, conflicts_with = "prompt_file")]
        prompt: Option<String>,
        #[arg(long)]
        prompt_file: Option<PathBuf>,
        #[arg(long)]
        detach: bool,
        #[arg(long, default_value_t = 1)]
        iterations: u32,
        #[arg(long)]
        cwd: Option<PathBuf>,
        #[arg(long)]
        json: bool,
        /// Do not use another LDGR-selected harness when the primary is unavailable.
        #[arg(long)]
        no_fallback: bool,
    },
    Status {
        job_id: String,
        #[arg(long, default_value_t = 40)]
        tail: usize,
        #[arg(long)]
        json: bool,
    },
    List {
        #[arg(long)]
        json: bool,
    },
    Logs {
        job_id: String,
        #[arg(long)]
        follow: bool,
    },
    #[command(name = "__supervise", hide = true)]
    Supervise { spec: PathBuf },
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Detect the host platform, LDGR selection, and runnable harnesses.
    Discover {
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    Generate,
    Check,
    Show,
}

fn main() -> Result<()> {
    agentctl::platform::repair_process_home()?;
    let cli = Cli::parse();

    match cli.command {
        Command::Exec {
            json,
            raw,
            label,
            argv,
        } => {
            let loaded = agentctl::config::check()?;
            let options = agentctl::exec::CaptureOptions {
                max_output_bytes: loaded.config.summary.max_output_bytes,
                tail_bytes: loaded.config.summary.tail_bytes,
                label,
                stream_raw: raw,
            };
            let run = agentctl::exec::run_captured(&argv, &options)?;

            if !raw {
                let summary = match &loaded.config.summary.summarizer_command {
                    Some(command) => agentctl::summary::summarize_with_command(
                        &run,
                        &loaded.config.summary,
                        command,
                    )
                    .unwrap_or_else(|error| {
                        eprintln!("warning: summary command failed: {error:#}");
                        agentctl::summary::summarize(&run, &loaded.config.summary)
                    }),
                    None => agentctl::summary::summarize(&run, &loaded.config.summary),
                };

                if json {
                    println!("{}", serde_json::to_string_pretty(&summary)?);
                } else {
                    print_summary(&summary);
                }
            }

            std::process::exit(run.exit_status.code().unwrap_or(1));
        }
        Command::Run {
            agent,
            prompt,
            prompt_file,
            detach,
            iterations,
            cwd,
            json,
            no_fallback,
        } => {
            let attempt = agentctl::capture::ExecutionAttempt::begin(
                Path::new("."),
                agent.as_deref().unwrap_or("auto"),
            )?;
            let initialized = (|| -> Result<_> {
                let loaded = agentctl::config::check().with_context(|| {
                    "Agentctl configuration is invalid; run `agentctl discover` for recovery options"
                })?;
                let resolved =
                    agentctl::discovery::resolve_agent(&loaded, agent.as_deref(), !no_fallback)?;
                if resolved.name.starts_with("ldgr-loop") || resolved.name == "ldgr-summary" {
                    agentctl::compatibility::negotiate(Path::new(".")).map_err(|error| {
                        anyhow::Error::new(error).context(
                            "LDGR launcher/Core compatibility negotiation failed before worker startup",
                        )
                    })?;
                }
                let prompt = read_prompt(prompt.as_deref(), prompt_file.as_deref())?;
                let cwd = resolve_cwd(resolved.agent.cwd.as_deref(), cwd.as_deref())?;
                Ok((loaded, resolved, prompt, cwd))
            })();
            let (loaded, resolved, prompt, cwd) = match initialized {
                Ok(initialized) => initialized,
                Err(error) => {
                    let rendered = format!("{error:#}");
                    let failure = if rendered.contains("compatibility negotiation")
                        || rendered.contains("requires LDGR Core")
                    {
                        let detected_core_version =
                            agentctl::compatibility::status(Path::new(".")).core_version;
                        agentctl::capture::FailureKind::CoreIncompatible {
                            detected_core_version,
                        }
                    } else if rendered.to_ascii_lowercase().contains("home")
                        || format!("{error:#}").contains("current directory")
                    {
                        agentctl::capture::FailureKind::Environment
                    } else {
                        agentctl::capture::FailureKind::Configuration
                    };
                    attempt
                        .record_error(failure)
                        .context("persisting accepted agent bootstrap failure")?;
                    return Err(error);
                }
            };
            for warning in &resolved.warnings {
                eprintln!("warning: {warning}");
            }
            let options = agentctl::jobs::AgentRunOptions {
                agent_name: resolved.name,
                agent: resolved.agent,
                prompt,
                cwd,
                iterations,
                attempt,
            };
            if detach {
                let record = agentctl::jobs::launch_detached(&options)?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&record)?);
                } else {
                    print_job_record(&record);
                }
            } else {
                let reports = agentctl::jobs::run_foreground(&options, &loaded.config.summary)?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&reports)?);
                } else {
                    for report in &reports {
                        if report.iterations > 1 {
                            println!("Iteration {}/{}", report.iteration, report.iterations);
                        }
                        print_summary(&report.summary);
                    }
                }
            }
        }
        Command::Status { job_id, tail, json } => {
            let status = agentctl::jobs::status(&job_id, tail)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&status)?);
            } else {
                print_job_status(&status);
            }
        }
        Command::List { json } => {
            let records = agentctl::jobs::list()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&records)?);
            } else if records.is_empty() {
                println!("No agent jobs found.");
            } else {
                for record in &records {
                    println!(
                        "{}  {:?}  pid={}  agent={}  exit={}",
                        record.id,
                        record.state,
                        record.pid,
                        record.agent,
                        record
                            .exit_code
                            .map(|code| code.to_string())
                            .unwrap_or_else(|| "<none>".to_owned())
                    );
                }
            }
        }
        Command::Logs { job_id, follow } => {
            agentctl::jobs::logs(&job_id, follow)?;
        }
        Command::Supervise { spec } => {
            let exit_code = agentctl::jobs::supervise(&spec)?;
            std::process::exit(exit_code);
        }
        Command::Config { command } => match command {
            ConfigCommand::Generate => {
                let path = agentctl::config::generate()?;
                println!("generated {}", path.display());
            }
            ConfigCommand::Check => {
                let loaded = agentctl::config::check()?;
                println!("ok {}", loaded.path.display());
            }
            ConfigCommand::Show => {
                print!("{}", agentctl::config::show()?);
            }
        },
        Command::Discover { json } => {
            let report = agentctl::discovery::diagnose()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                agentctl::discovery::print_human(&report);
            }
        }
    }

    Ok(())
}

fn read_prompt(prompt: Option<&str>, prompt_file: Option<&Path>) -> Result<String> {
    match (prompt, prompt_file) {
        (Some(_), Some(_)) => bail!("use either --prompt or --prompt-file, not both"),
        (Some(value), None) => Ok(value.to_owned()),
        (None, Some(path)) if path == Path::new("-") => {
            let mut input = String::new();
            std::io::stdin()
                .read_to_string(&mut input)
                .context("reading prompt from stdin")?;
            Ok(input)
        }
        (None, Some(path)) => {
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))
        }
        (None, None) => {
            let mut input = String::new();
            std::io::stdin()
                .read_to_string(&mut input)
                .context("reading prompt from stdin")?;
            if input.trim().is_empty() {
                bail!("provide --prompt, --prompt-file, or prompt text on stdin");
            }
            Ok(input)
        }
    }
}

fn resolve_cwd(configured: Option<&Path>, override_cwd: Option<&Path>) -> Result<PathBuf> {
    let cwd = if let Some(path) = override_cwd {
        path.to_path_buf()
    } else if let Some(path) = configured {
        path.to_path_buf()
    } else {
        std::env::current_dir().context("determining current directory")?
    };
    if cwd.is_absolute() {
        Ok(cwd)
    } else {
        Ok(std::env::current_dir()
            .context("determining current directory")?
            .join(cwd))
    }
}

fn print_summary(summary: &agentctl::summary::Summary) {
    println!("Summary");
    println!("- command: {}", summary.command);
    println!("- exit: {}", summary.exit_code);
    println!("- duration: {}ms", summary.duration_ms);
    println!("- raw log: {}", summary.raw_log_path.display());
    if summary.preview_lines.is_empty() {
        println!("- output: <empty>");
    } else {
        println!("- notable output:");
        for line in &summary.preview_lines {
            println!("  {line}");
        }
    }
}

fn print_job_record(record: &agentctl::jobs::JobRecord) {
    println!("Job: {}", record.id);
    println!("Agent: {}", record.agent);
    println!("State: {:?}", record.state);
    println!("PID: {}", record.pid);
    println!("CWD: {}", record.cwd.display());
    println!("Iterations: {}", record.iterations);
    println!("Command: {}", record.command);
    println!("Log: {}", record.log_path.display());
}

fn print_job_status(status: &agentctl::jobs::JobStatus) {
    print_job_record(&status.record);
    println!(
        "Exit: {}",
        status
            .record
            .exit_code
            .map(|code| code.to_string())
            .unwrap_or_else(|| "<none>".to_owned())
    );
    if !status.tail_lines.is_empty() {
        println!("Tail");
        for line in &status.tail_lines {
            println!("{line}");
        }
    }
}
