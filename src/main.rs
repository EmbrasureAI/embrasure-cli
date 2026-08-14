mod compare;
mod config;
mod dbt;
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
    }
}
