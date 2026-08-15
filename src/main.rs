mod auth;
mod compare;
mod config;
mod dbt;
mod doctor;
mod init;
mod metabase;
mod report;
mod run;
mod snowflake;

use std::{path::PathBuf, process::ExitCode};

use clap::{Parser, Subcommand, ValueEnum};

use crate::config::ComparisonMode;
use crate::config::{DownstreamPolicy, IncrementalMode};

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ModeArg {
    Quick,
    Deep,
}

impl From<ModeArg> for ComparisonMode {
    fn from(value: ModeArg) -> Self {
        match value {
            ModeArg::Quick => Self::Quick,
            ModeArg::Deep => Self::Deep,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum DownstreamArg {
    None,
    Critical,
    All,
}

impl From<DownstreamArg> for DownstreamPolicy {
    fn from(value: DownstreamArg) -> Self {
        match value {
            DownstreamArg::None => Self::None,
            DownstreamArg::Critical => Self::Critical,
            DownstreamArg::All => Self::All,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum IncrementalModeArg {
    Clone,
    FullRefresh,
}

impl From<IncrementalModeArg> for IncrementalMode {
    fn from(value: IncrementalModeArg) -> Self {
        match value {
            IncrementalModeArg::Clone => Self::Clone,
            IncrementalModeArg::FullRefresh => Self::FullRefresh,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ReportVersionArg {
    #[value(name = "1")]
    V1,
    #[value(name = "2")]
    V2,
}

impl ReportVersionArg {
    fn number(self) -> u8 {
        match self {
            Self::V1 => 1,
            Self::V2 => 2,
        }
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "embrasure",
    version,
    about = "Validate dbt changes against production Snowflake data"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Create a minimal configuration for the current dbt project.
    Init {
        /// Configuration file to create.
        #[arg(long, default_value = "embrasure-check.yml")]
        config: PathBuf,
        /// Replace an existing configuration file.
        #[arg(long)]
        force: bool,
        #[arg(long, hide = true)]
        profile: Option<String>,
        #[arg(long, hide = true)]
        account: Option<String>,
        #[arg(long, hide = true)]
        user: Option<String>,
        #[arg(long, hide = true)]
        role: Option<String>,
        #[arg(long, hide = true)]
        database: Option<String>,
        #[arg(long, hide = true)]
        warehouse: Option<String>,
        #[arg(long, hide = true)]
        production_schema: Option<String>,
    },
    /// Build and compare changed dbt models.
    #[command(alias = "run")]
    Check {
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
        /// Validation depth. Quick skips percentiles and estimates cardinality.
        #[arg(long, value_enum)]
        mode: Option<ModeArg>,
        /// Downstream validation scope. Full impact is always reported.
        #[arg(long, value_enum)]
        downstream: Option<DownstreamArg>,
        /// Replace configured critical tags. Repeat for multiple tags.
        #[arg(long = "critical-tag")]
        critical_tags: Vec<String>,
        /// How existing incremental models are built for validation.
        #[arg(long, value_enum)]
        incremental_mode: Option<IncrementalModeArg>,
        /// JSON report contract version.
        #[arg(long, value_enum, requires = "json")]
        report_version: Option<ReportVersionArg>,
        /// Show every finding and impacted lineage node.
        #[arg(long)]
        verbose: bool,
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
        Command::Init {
            config,
            force,
            profile,
            account,
            user,
            role,
            database,
            warehouse,
            production_schema,
        } => match init::run(
            &config,
            force,
            init::Options {
                profile,
                account,
                user,
                role,
                database,
                warehouse,
                production_schema,
            },
        ) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("embrasure: {error:#}");
                ExitCode::from(report::EXIT_EXECUTION)
            }
        },
        Command::Check {
            base,
            config,
            json,
            markdown,
            mode,
            downstream,
            critical_tags,
            incremental_mode,
            report_version,
            verbose,
        } => {
            eprintln!("embrasure: validating changes against {base}");
            let mut report = run::run_check(
                &config,
                &base,
                run::CheckOptions {
                    mode: mode.map(Into::into),
                    downstream: downstream.map(Into::into),
                    critical_tags: (!critical_tags.is_empty()).then_some(critical_tags),
                    incremental_mode: incremental_mode.map(Into::into),
                },
            )
            .await;
            if let Some(path) = markdown
                && let Err(error) = report.write_markdown(&path)
            {
                report
                    .execution_errors
                    .push(format!("could not write Markdown report: {error:#}"));
                report.finalize();
            }
            if json {
                match report.json(report_version.unwrap_or(ReportVersionArg::V2).number()) {
                    Ok(value) => println!("{value}"),
                    Err(error) => {
                        eprintln!("embrasure: could not serialize JSON report: {error}");
                        return ExitCode::from(report::EXIT_EXECUTION);
                    }
                }
            } else {
                print!("{}", report.human(verbose));
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
                        eprintln!("embrasure: {error:#}");
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
                    eprintln!("embrasure: {error:#}");
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
                        eprintln!("embrasure: {error:#}");
                        ExitCode::from(report::EXIT_EXECUTION)
                    }
                }
            }
        },
    }
}
