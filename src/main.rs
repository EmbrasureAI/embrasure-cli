mod auth;
mod compare;
mod config;
mod dbt;
mod doctor;
mod metabase;
mod report;
mod run;
mod snowflake;

use std::{path::PathBuf, process::ExitCode};

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "embrasure-check",
    version,
    about = "Validate dbt changes against production Snowflake data"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Build and compare changed dbt models.
    Run {
        /// Git revision used as the production comparison base.
        #[arg(long, default_value = "origin/main")]
        base: String,
        /// Configuration file.
        #[arg(long, default_value = "embrasure-check.yml")]
        config: PathBuf,
        /// Emit exactly one versioned JSON document on stdout.
        #[arg(long)]
        json: bool,
        /// Also write a deterministic Markdown report.
        #[arg(long, value_name = "PATH")]
        markdown: Option<PathBuf>,
    },
    /// Check local tools, credentials, Snowflake permissions, and optional Metabase access.
    Doctor {
        /// Configuration file.
        #[arg(long, default_value = "embrasure-check.yml")]
        config: PathBuf,
        /// Skip the temporary schema create/drop permission test.
        #[arg(long)]
        read_only: bool,
        /// Emit exactly one JSON document on stdout.
        #[arg(long)]
        json: bool,
    },
    /// Manage interactive Snowflake browser sessions.
    Auth {
        #[command(subcommand)]
        command: AuthCommand,
    },
}

#[derive(Debug, Subcommand)]
enum AuthCommand {
    /// Sign in to an account configured with type: oauth_local.
    Login {
        #[arg(long, default_value = "embrasure-check.yml")]
        config: PathBuf,
        /// Configured account name. Optional when there is only one account.
        #[arg(long)]
        account: Option<String>,
    },
    /// Show whether credentials are ready, without printing secrets.
    Status {
        #[arg(long, default_value = "embrasure-check.yml")]
        config: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Remove the cached browser session for an account.
    Logout {
        #[arg(long, default_value = "embrasure-check.yml")]
        config: PathBuf,
        /// Configured account name. Optional when there is only one account.
        #[arg(long)]
        account: Option<String>,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Run {
            base,
            config,
            json,
            markdown,
        } => {
            eprintln!("embrasure-check: validating changes against {base}");
            let mut report = run::run_check(&config, &base).await;
            if let Some(path) = markdown
                && let Err(error) = report.write_markdown(&path)
            {
                report
                    .execution_errors
                    .push(format!("could not write Markdown report: {error:#}"));
                report.finalize();
            }
            if json {
                match serde_json::to_string_pretty(&report) {
                    Ok(value) => println!("{value}"),
                    Err(error) => {
                        eprintln!("embrasure-check: could not serialize JSON report: {error}");
                        return ExitCode::from(report::EXIT_EXECUTION);
                    }
                }
            } else {
                print!("{}", report.human());
            }
            ExitCode::from(report.exit_code)
        }
        Command::Doctor {
            config,
            read_only,
            json,
        } => {
            let report = doctor::run(&config, !read_only).await;
            if json {
                println!("{}", serde_json::to_string_pretty(&report).unwrap_or_else(|error| {
                    format!(r#"{{"status":"error","message":"could not serialize report: {error}"}}"#)
                }));
            } else {
                print!("{}", report.human());
            }
            ExitCode::from(if report.ready {
                0
            } else {
                report::EXIT_EXECUTION
            })
        }
        Command::Auth { command } => match command {
            AuthCommand::Login { config, account } => {
                match auth::login_from_config(&config, account.as_deref()).await {
                    Ok(name) => {
                        println!("Signed in to Snowflake account {name}.");
                        ExitCode::SUCCESS
                    }
                    Err(error) => {
                        eprintln!("embrasure-check: {error:#}");
                        ExitCode::from(report::EXIT_EXECUTION)
                    }
                }
            }
            AuthCommand::Status { config, json } => match auth::status_from_config(&config) {
                Ok(statuses) => {
                    if json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&statuses)
                                .expect("auth status is serializable")
                        );
                    } else {
                        for status in &statuses {
                            println!("{}: {} ({})", status.account, status.status, status.method);
                        }
                    }
                    ExitCode::from(if statuses.iter().all(|item| item.ready) {
                        0
                    } else {
                        report::EXIT_EXECUTION
                    })
                }
                Err(error) => {
                    eprintln!("embrasure-check: {error:#}");
                    ExitCode::from(report::EXIT_EXECUTION)
                }
            },
            AuthCommand::Logout { config, account } => {
                match auth::logout_from_config(&config, account.as_deref()) {
                    Ok(name) => {
                        println!("Removed the cached Snowflake session for account {name}.");
                        ExitCode::SUCCESS
                    }
                    Err(error) => {
                        eprintln!("embrasure-check: {error:#}");
                        ExitCode::from(report::EXIT_EXECUTION)
                    }
                }
            }
        },
    }
}
