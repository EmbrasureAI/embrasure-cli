mod auth;
mod cloud;
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
        /// Hand the exact validated working state to a durable Embrasure Cloud agent.
        #[arg(long)]
        cloud: bool,
        /// Business intent used to create bounded validation assertions. Repeat to preserve ordering.
        #[arg(long = "context", value_name = "BUSINESS_INTENT", requires = "cloud")]
        context: Vec<String>,
        /// Read additional business intent from a UTF-8 file.
        #[arg(long, value_name = "PATH", requires = "cloud")]
        context_file: Option<PathBuf>,
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
    /// Manage the optional Embrasure Cloud session and durable runs.
    Cloud {
        #[command(subcommand)]
        command: CloudCommand,
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

#[derive(Debug, Subcommand)]
enum CloudCommand {
    /// Sign in through the browser and save a separate OS-keychain session.
    Login,
    /// Show the signed-in Embrasure identity and workspace.
    Whoami {
        #[arg(long)]
        json: bool,
    },
    /// Remove and revoke the saved Embrasure Cloud session.
    Logout,
    /// Show the latest handoff or a specified durable run.
    Status {
        run_id: Option<String>,
        #[arg(long)]
        json: bool,
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
            cloud: use_cloud,
            context,
            context_file,
        } => {
            let options = run::CheckOptions {
                mode: mode.map(Into::into),
                downstream: downstream.map(Into::into),
                critical_tags: (!critical_tags.is_empty()).then_some(critical_tags),
                incremental_mode: incremental_mode.map(Into::into),
            };
            let intent = if use_cloud {
                match cloud::normalize_context(&context, context_file.as_deref()) {
                    Ok(value) => Some(value),
                    Err(error) => {
                        eprintln!("embrasure: {error:#}");
                        return ExitCode::from(report::EXIT_EXECUTION);
                    }
                }
            } else {
                None
            };
            let snapshot = match cloud::prepare_snapshot(&config, &base, &options) {
                Ok(value) => Some(value),
                Err(error) if !use_cloud => {
                    let _ = error;
                    None
                }
                Err(error) => {
                    eprintln!("embrasure: could not prepare cloud snapshot: {error:#}");
                    return ExitCode::from(report::EXIT_EXECUTION);
                }
            };
            let cached = snapshot
                .as_ref()
                .and_then(|value| cloud::cached_review(value).ok().flatten());
            let mut report = if use_cloud {
                if let Some(report) = cached {
                    eprintln!("Reusing the local review for this exact working-tree state");
                    report
                } else {
                    eprintln!(
                        "The working tree or review configuration changed; rerunning local review"
                    );
                    run::run_check(&config, &base, options.clone()).await
                }
            } else {
                eprintln!("embrasure: validating changes against {base}");
                run::run_check(&config, &base, options.clone()).await
            };
            if let Some(snapshot) = &snapshot {
                if let Err(error) = cloud::save_review(snapshot, &report) {
                    eprintln!("embrasure: could not save the local review cache: {error:#}");
                }
            }
            if let Some(path) = markdown
                && let Err(error) = report.write_markdown(&path)
            {
                report
                    .execution_errors
                    .push(format!("could not write Markdown report: {error:#}"));
                report.finalize();
            }
            if use_cloud {
                if report.exit_code == report::EXIT_EXECUTION {
                    if json {
                        println!(
                            "{}",
                            serde_json::json!({"local_review": report, "cloud_handoff": null})
                        );
                    } else {
                        print!("{}", report.human(verbose));
                    }
                    return ExitCode::from(report::EXIT_EXECUTION);
                }
                let snapshot = snapshot.expect("cloud snapshots are prepared before local review");
                cloud::print_snapshot(&snapshot);
                let progress = cloud::Progress::start("Handing off to Embrasure Cloud");
                let receipt = cloud::handoff(
                    &snapshot,
                    &report,
                    &base,
                    intent.as_deref().unwrap_or_default(),
                )
                .await;
                drop(progress);
                match receipt {
                    Ok(receipt) => {
                        if json {
                            println!("{}", serde_json::to_string_pretty(&serde_json::json!({"local_review": report, "cloud_handoff": receipt})).expect("cloud result is serializable"));
                        } else {
                            let downstream = report.impact.dbt_models.len();
                            let exposures = report.impact.dbt_exposures.len()
                                + report.impact.metabase_dashboards.len();
                            println!(
                                "\nCloud review accepted\nRun: {}\n{} downstream models and {} dashboard{} in scope\nYour laptop is no longer part of the execution path\nWatch: {}",
                                receipt.run_id,
                                downstream,
                                exposures,
                                if exposures == 1 { "" } else { "s" },
                                receipt.run_url
                            );
                        }
                        ExitCode::from(report.exit_code)
                    }
                    Err(error) => {
                        eprintln!("embrasure: {error:#}");
                        ExitCode::from(report::EXIT_EXECUTION)
                    }
                }
            } else if json {
                match report.json(report_version.unwrap_or(ReportVersionArg::V2).number()) {
                    Ok(value) => println!("{value}"),
                    Err(error) => {
                        eprintln!("embrasure: could not serialize JSON report: {error}");
                        return ExitCode::from(report::EXIT_EXECUTION);
                    }
                }
                ExitCode::from(report.exit_code)
            } else {
                print!("{}", report.human(verbose));
                ExitCode::from(report.exit_code)
            }
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
        Command::Cloud { command } => match command {
            CloudCommand::Login => match cloud::login().await {
                Ok(session) => {
                    println!(
                        "Signed in to Embrasure Cloud.\nWorkspace: {}",
                        session.workspace_id
                    );
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    eprintln!("embrasure: {error:#}");
                    ExitCode::from(report::EXIT_EXECUTION)
                }
            },
            CloudCommand::Whoami { json } => match cloud::whoami().await {
                Ok(value) => {
                    if json {
                        println!("{}", serde_json::to_string_pretty(&value).unwrap());
                    } else {
                        println!(
                            "Signed in as {}.\nWorkspace: {}",
                            value
                                .get("email")
                                .or_else(|| value.get("user_id"))
                                .and_then(|item| item.as_str())
                                .unwrap_or("unknown"),
                            value
                                .get("workspace_id")
                                .and_then(|item| item.as_str())
                                .unwrap_or("selected during login")
                        );
                    }
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    eprintln!("embrasure: {error:#}");
                    ExitCode::from(report::EXIT_EXECUTION)
                }
            },
            CloudCommand::Logout => match cloud::logout().await {
                Ok(()) => {
                    println!("Signed out of Embrasure Cloud.");
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    eprintln!("embrasure: {error:#}");
                    ExitCode::from(report::EXIT_EXECUTION)
                }
            },
            CloudCommand::Status { run_id, json } => match cloud::status(run_id.as_deref()).await {
                Ok(value) => {
                    if json {
                        println!("{}", serde_json::to_string_pretty(&value).unwrap());
                    } else {
                        println!(
                            "Run: {}\nStatus: {}\nWatch: {}",
                            value
                                .get("run_id")
                                .and_then(|v| v.as_str())
                                .unwrap_or("unknown"),
                            value
                                .get("status")
                                .and_then(|v| v.as_str())
                                .unwrap_or("unknown"),
                            value
                                .get("run_url")
                                .and_then(|v| v.as_str())
                                .unwrap_or("unavailable")
                        );
                    }
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    eprintln!("embrasure: {error:#}");
                    ExitCode::from(report::EXIT_EXECUTION)
                }
            },
        },
    }
}
