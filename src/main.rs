use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "agentctl")]
#[command(about = "Compact, bounded command and agent reports for LLM tool loops")]
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
        #[arg(default_value = "codex")]
        agent: String,
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
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    Generate,
    Check,
    Show,
}

fn main() -> Result<()> {
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
        } => {
            let loaded = agentctl::config::check()?;
            let agent_config =
                loaded.config.agents.get(&agent).cloned().with_context(|| {
                    format!("unknown agent `{agent}` in {}", loaded.path.display())
                })?;
            let prompt = read_prompt(prompt.as_deref(), prompt_file.as_deref())?;
            let cwd = resolve_cwd(agent_config.cwd.as_deref(), cwd.as_deref())?;
            let options = agentctl::jobs::AgentRunOptions {
                agent_name: agent,
                agent: agent_config,
                prompt,
                cwd,
                iterations,
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
