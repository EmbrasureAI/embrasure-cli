mod auth;
mod bigquery;
mod clean;
#[cfg(feature = "cloud-demo")]
mod cloud;
mod compare;
mod config;
mod databricks;
mod dbt;
mod doctor;
mod git;
mod init;
mod lineage;
mod loopback;
mod metabase;
mod progress;
mod provider;
mod query;
mod report;
mod run;
mod snowflake;
mod style;
mod update;

use std::{io::IsTerminal, path::PathBuf, process::ExitCode, time::Instant};

use clap::{CommandFactory, Parser, Subcommand, ValueEnum};

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

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CompletionShell {
    Bash,
    Zsh,
    Fish,
    Powershell,
}

impl From<IncrementalModeArg> for IncrementalMode {
    fn from(value: IncrementalModeArg) -> Self {
        match value {
            IncrementalModeArg::Clone => Self::Clone,
            IncrementalModeArg::FullRefresh => Self::FullRefresh,
        }
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "embrasure",
    version,
    about = "Validate dbt changes against production warehouse data"
)]
struct Cli {
    /// Configuration file.
    #[arg(long, global = true, default_value = "embrasure-check.yml")]
    config: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Create a minimal configuration for the current dbt project.
    Init {
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
        location: Option<String>,
        #[arg(long, hide = true)]
        production_schema: Option<String>,
    },
    /// Build and compare changed dbt models, then run configured query checks.
    #[command(alias = "run")]
    Check {
        /// Git revision used as the production comparison base.
        #[arg(long, default_value = "origin/main")]
        base: String,
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
        report_version: Option<report::ReportVersion>,
        /// Show every finding and impacted lineage node.
        #[arg(long)]
        verbose: bool,
        /// Validate only these changed models. Repeat for multiple models; combine with --downstream none for the fastest loop.
        #[arg(long = "select", value_name = "MODEL")]
        select: Vec<String>,
        /// Plan validation without creating schemas or querying warehouse data.
        #[arg(long)]
        #[cfg_attr(feature = "cloud-demo", arg(conflicts_with = "cloud"))]
        dry_run: bool,
        #[cfg(feature = "cloud-demo")]
        /// Hand the exact validated working state to a durable Embrasure Cloud agent.
        #[arg(long)]
        cloud: bool,
        #[cfg(feature = "cloud-demo")]
        /// Business intent used to create bounded validation assertions. Repeat to preserve ordering.
        #[arg(long = "context", value_name = "BUSINESS_INTENT", requires = "cloud")]
        context: Vec<String>,
        #[cfg(feature = "cloud-demo")]
        /// Read additional business intent from a UTF-8 file.
        #[arg(long, value_name = "PATH", requires = "cloud")]
        context_file: Option<PathBuf>,
    },
    /// Check local tools, credentials, warehouse permissions, optional Metabase access, and available updates.
    Doctor {
        /// Skip the temporary schema or dataset create/drop permission test.
        #[arg(long)]
        read_only: bool,
        /// Emit exactly one JSON document on stdout.
        #[arg(long)]
        json: bool,
    },
    /// Inspect warehouse credentials or manage Snowflake browser sessions.
    Auth {
        #[command(subcommand)]
        command: AuthCommand,
    },
    #[cfg(feature = "cloud-demo")]
    /// Manage the Embrasure Cloud demo session and durable runs.
    Cloud {
        #[command(subcommand)]
        command: CloudCommand,
    },
    /// Generate a shell completion script.
    Completion {
        #[arg(value_enum)]
        shell: CompletionShell,
    },
    /// List or remove old Embrasure-managed temporary schemas or datasets.
    Clean {
        /// Minimum schema or dataset age in hours.
        #[arg(long, default_value_t = 6)]
        older_than: u64,
        /// Remove matched schemas. Without this flag, only list them.
        #[arg(long)]
        yes: bool,
        /// Emit one JSON document on stdout.
        #[arg(long)]
        json: bool,
    },
    /// Check for or install the latest Embrasure release.
    Update {
        /// Report update availability without installing it.
        #[arg(long)]
        check: bool,
    },
}

#[derive(Debug, Subcommand)]
enum AuthCommand {
    /// Sign in to an account configured with type: oauth_local.
    Login {
        /// Configured account name. Optional when there is only one account.
        #[arg(long)]
        account: Option<String>,
    },
    /// Show whether credentials are ready, without printing secrets.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Remove the cached browser session for an account.
    Logout {
        /// Configured account name. Optional when there is only one account.
        #[arg(long)]
        account: Option<String>,
    },
}

#[cfg(feature = "cloud-demo")]
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
    let config = cli.config;
    match cli.command {
        Command::Init {
            force,
            profile,
            account,
            user,
            role,
            database,
            warehouse,
            location,
            production_schema,
        } => match init::run(
            &config,
            force,
            init::Options {
                profile,
                provider: None,
                account,
                user,
                role,
                database,
                warehouse,
                location,
                production_schema,
            },
        ) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => fail(error),
        },
        Command::Check {
            base,
            json,
            markdown,
            mode,
            downstream,
            critical_tags,
            incremental_mode,
            report_version,
            verbose,
            select,
            dry_run,
            #[cfg(feature = "cloud-demo")]
            cloud,
            #[cfg(feature = "cloud-demo")]
            context,
            #[cfg(feature = "cloud-demo")]
            context_file,
        } => {
            let check_started = Instant::now();
            let progress_display = progress::Display::start(&base, json, dry_run);
            let progress_active = progress_display.is_some();
            #[cfg(feature = "cloud-demo")]
            let use_cloud = cloud;
            let options = run::CheckOptions {
                mode: mode.map(Into::into),
                downstream: downstream.map(Into::into),
                critical_tags: (!critical_tags.is_empty()).then_some(critical_tags),
                incremental_mode: incremental_mode.map(Into::into),
                select,
                progress: progress_display.as_ref().map(progress::Display::reporter),
            };
            let loaded_config = run::load_config(&config, &options);

            #[cfg(feature = "cloud-demo")]
            let (intent, snapshot, mut report) = {
                let intent = if use_cloud {
                    match cloud::normalize_context(&context, context_file.as_deref()) {
                        Ok(value) => Some(value),
                        Err(error) => return fail(error),
                    }
                } else {
                    None
                };
                let snapshot = match loaded_config.as_ref() {
                    Ok(loaded) if use_cloud => {
                        match cloud::prepare_snapshot(&config, loaded, &base, &options) {
                            Ok(value) => Some(value),
                            Err(error) => {
                                return fail(format_args!(
                                    "could not prepare cloud snapshot: {error:#}"
                                ));
                            }
                        }
                    }
                    Ok(_) if dry_run => None,
                    Ok(loaded) => cloud::prepare_snapshot(&config, loaded, &base, &options).ok(),
                    Err(_) => None,
                };
                let cached = snapshot
                    .as_ref()
                    .and_then(|value| cloud::cached_review(value).ok().flatten());
                let report = if use_cloud {
                    if let Some(report) = cached {
                        eprintln!("Reusing the local review for this exact working-tree state");
                        report
                    } else {
                        eprintln!(
                            "The working tree or review configuration changed; rerunning local review"
                        );
                        match loaded_config {
                            Ok(config) => {
                                run::run_check_with_config(config, &base, &options, dry_run).await
                            }
                            Err(error) => run::failed_check(&base, error),
                        }
                    }
                } else {
                    if !progress_active {
                        eprintln!("embrasure: validating changes against {base}");
                    }
                    match loaded_config {
                        Ok(config) => {
                            run::run_check_with_config(config, &base, &options, dry_run).await
                        }
                        Err(error) => run::failed_check(&base, error),
                    }
                };
                if let Some(snapshot) = &snapshot
                    && let Err(error) = cloud::save_review(snapshot, &report)
                {
                    eprintln!("embrasure: could not save the local review cache: {error:#}");
                }
                (intent, snapshot, report)
            };

            #[cfg(not(feature = "cloud-demo"))]
            let mut report = {
                if !progress_active {
                    eprintln!("embrasure: validating changes against {base}");
                }
                match loaded_config {
                    Ok(config) => {
                        run::run_check_with_config(config, &base, &options, dry_run).await
                    }
                    Err(error) => run::failed_check(&base, error),
                }
            };
            if let Some(path) = markdown
                && let Err(error) = report.write_markdown(&path)
            {
                report
                    .execution_errors
                    .push(format!("could not write Markdown report: {error:#}"));
                report.finalize();
            }
            let report_version = report_version.unwrap_or(report::ReportVersion::V4);

            if let Some(display) = progress_display {
                display.finish(&report);
            }

            #[cfg(feature = "cloud-demo")]
            if use_cloud {
                if report.exit_code == report::EXIT_EXECUTION {
                    if json {
                        println!("{}", cloud_result_json(&report, None, report_version));
                    } else {
                        print!("{}", report.human_styled(verbose, &style::Style::stdout()));
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
                return match receipt {
                    Ok(receipt) => {
                        if json {
                            println!(
                                "{}",
                                cloud_result_json(&report, Some(&receipt), report_version)
                            );
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
                        if json {
                            println!("{}", cloud_result_json(&report, None, report_version));
                        }
                        fail(error)
                    }
                };
            }

            if json {
                match report.json(report_version) {
                    Ok(value) => println!("{value}"),
                    Err(error) => {
                        return fail(format_args!("could not serialize JSON report: {error}"));
                    }
                }
                ExitCode::from(report.exit_code)
            } else {
                print!(
                    "{}",
                    report.human_styled_with_elapsed(
                        verbose,
                        &style::Style::stdout(),
                        check_started.elapsed(),
                    )
                );
                ExitCode::from(report.exit_code)
            }
        }
        Command::Doctor { read_only, json } => {
            let report = doctor::run(&config, !read_only).await;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&report).expect("doctor report is serializable")
                );
            } else {
                print!("{}", report.human_styled(&style::Style::stdout()));
                if let Some(notice) = update::doctor_notice().await {
                    eprintln!("{notice}");
                }
            }
            ExitCode::from(if report.ready {
                0
            } else {
                report::EXIT_EXECUTION
            })
        }
        Command::Auth { command } => match command {
            AuthCommand::Login { account } => {
                match auth::login_from_config(&config, account.as_deref()).await {
                    Ok(name) => {
                        println!("Signed in to Snowflake account {name}.");
                        ExitCode::SUCCESS
                    }
                    Err(error) => fail(error),
                }
            }
            AuthCommand::Status { json } => match auth::status_from_config(&config).await {
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
                Err(error) => fail(error),
            },
            AuthCommand::Logout { account } => {
                match auth::logout_from_config(&config, account.as_deref()) {
                    Ok(name) => {
                        println!("Removed the cached Snowflake session for account {name}.");
                        ExitCode::SUCCESS
                    }
                    Err(error) => fail(error),
                }
            }
        },
        #[cfg(feature = "cloud-demo")]
        Command::Cloud { command } => match command {
            CloudCommand::Login => match cloud::login().await {
                Ok(session) => {
                    println!(
                        "Signed in to Embrasure Cloud.\nWorkspace: {}",
                        session.workspace_id
                    );
                    ExitCode::SUCCESS
                }
                Err(error) => fail(error),
            },
            CloudCommand::Whoami { json } => match cloud::whoami().await {
                Ok(value) => {
                    if json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&value)
                                .expect("cloud identity is serializable")
                        );
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
                Err(error) => fail(error),
            },
            CloudCommand::Logout => match cloud::logout().await {
                Ok(()) => {
                    println!("Signed out of Embrasure Cloud.");
                    ExitCode::SUCCESS
                }
                Err(error) => fail(error),
            },
            CloudCommand::Status { run_id, json } => match cloud::status(run_id.as_deref()).await {
                Ok(value) => {
                    if json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&value)
                                .expect("cloud status is serializable")
                        );
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
                Err(error) => fail(error),
            },
        },
        Command::Completion { shell } => {
            let mut command = Cli::command();
            let mut stdout = std::io::stdout();
            match shell {
                CompletionShell::Bash => clap_complete::generate(
                    clap_complete::shells::Bash,
                    &mut command,
                    "embrasure",
                    &mut stdout,
                ),
                CompletionShell::Zsh => clap_complete::generate(
                    clap_complete::shells::Zsh,
                    &mut command,
                    "embrasure",
                    &mut stdout,
                ),
                CompletionShell::Fish => clap_complete::generate(
                    clap_complete::shells::Fish,
                    &mut command,
                    "embrasure",
                    &mut stdout,
                ),
                CompletionShell::Powershell => clap_complete::generate(
                    clap_complete::shells::PowerShell,
                    &mut command,
                    "embrasure",
                    &mut stdout,
                ),
            }
            if std::io::stderr().is_terminal() {
                let hint = match shell {
                    CompletionShell::Bash => {
                        "Source this script from your Bash profile or completion directory."
                    }
                    CompletionShell::Zsh => {
                        "Save this script in a directory listed by your Zsh fpath."
                    }
                    CompletionShell::Fish => {
                        "Save this script as ~/.config/fish/completions/embrasure.fish."
                    }
                    CompletionShell::Powershell => {
                        "Save this script and source it from your PowerShell profile."
                    }
                };
                eprintln!("{hint}");
            }
            ExitCode::SUCCESS
        }
        Command::Clean {
            older_than,
            yes,
            json,
        } => {
            let result = clean::run(&config, older_than, yes).await;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&result).expect("clean report is serializable")
                );
            } else {
                print!("{}", result.human());
            }
            ExitCode::from(if result.errors.is_empty() {
                report::EXIT_PASS
            } else {
                report::EXIT_EXECUTION
            })
        }
        Command::Update { check } => match update::run(check).await {
            Ok(message) => {
                println!("{message}");
                ExitCode::SUCCESS
            }
            Err(error) => fail(error),
        },
    }
}

#[cfg(feature = "cloud-demo")]
fn cloud_result_json(
    report: &report::Report,
    receipt: Option<&cloud::HandoffReceipt>,
    version: report::ReportVersion,
) -> String {
    serde_json::to_string_pretty(&serde_json::json!({
        "local_review": report
            .json_value(version)
            .expect("local review is serializable"),
        "cloud_handoff": receipt,
    }))
    .expect("cloud result is serializable")
}

fn fail(error: impl std::fmt::Display) -> ExitCode {
    let style = style::Style::stderr();
    eprintln!("{}: {error:#}", style.bad("embrasure"));
    ExitCode::from(report::EXIT_EXECUTION)
}
