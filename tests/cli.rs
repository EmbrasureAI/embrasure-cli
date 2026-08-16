use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;

#[test]
fn help_exposes_enterprise_setup_commands() {
    Command::cargo_bin("embrasure")
        .unwrap()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("init"))
        .stdout(predicate::str::contains("check"))
        .stdout(predicate::str::contains("doctor"))
        .stdout(predicate::str::contains("auth"))
        .stdout(predicate::str::contains("cloud"));
}

#[test]
fn init_creates_a_minimal_valid_config() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(
        directory.path().join("dbt_project.yml"),
        "name: analytics\nprofile: analytics\n",
    )
    .unwrap();

    Command::cargo_bin("embrasure")
        .unwrap()
        .current_dir(directory.path())
        .args([
            "init",
            "--account",
            "my_org-my_account",
            "--user",
            "DBT_CI",
            "--role",
            "DBT_CI_ROLE",
            "--database",
            "ANALYTICS",
            "--warehouse",
            "DBT_CI_WH",
            "--production-schema",
            "PROD",
        ])
        .write_stdin("\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("Created embrasure-check.yml"));

    let config = fs::read_to_string(directory.path().join("embrasure-check.yml")).unwrap();
    assert!(config.contains("profile: analytics"));
    assert!(config.contains("account: my_org-my_account"));
    assert!(config.contains("type: oauth_local"));
    assert!(!config.contains("thresholds:"));

    Command::cargo_bin("embrasure")
        .unwrap()
        .current_dir(directory.path())
        .args(["auth", "status", "--json"])
        .assert()
        .code(3)
        .stdout(predicate::str::contains(r#""account": "primary""#));
}

#[test]
fn init_does_not_replace_an_existing_config() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(
        directory.path().join("dbt_project.yml"),
        "name: analytics\n",
    )
    .unwrap();
    fs::write(directory.path().join("embrasure-check.yml"), "keep me\n").unwrap();

    Command::cargo_bin("embrasure")
        .unwrap()
        .current_dir(directory.path())
        .arg("init")
        .assert()
        .code(3)
        .stderr(predicate::str::contains("already exists"));

    assert_eq!(
        fs::read_to_string(directory.path().join("embrasure-check.yml")).unwrap(),
        "keep me\n"
    );
}

#[test]
fn init_reuses_values_from_the_active_dbt_profile() {
    let directory = tempfile::tempdir().unwrap();
    let profiles_directory = directory.path().join("profiles");
    fs::create_dir(&profiles_directory).unwrap();
    fs::write(
        directory.path().join("dbt_project.yml"),
        "name: analytics\nprofile: analytics\n",
    )
    .unwrap();
    fs::write(
        profiles_directory.join("profiles.yml"),
        r#"analytics:
  target: dev
  outputs:
    dev:
      type: snowflake
      account: "{{ env_var('SNOWFLAKE_ACCOUNT') }}"
      user: DBT_CI
      role: DBT_CI_ROLE
      database: ANALYTICS
      warehouse: DBT_CI_WH
      schema: JACOB
"#,
    )
    .unwrap();

    Command::cargo_bin("embrasure")
        .unwrap()
        .current_dir(directory.path())
        .env("DBT_PROFILES_DIR", &profiles_directory)
        .env("SNOWFLAKE_ACCOUNT", "my_org-my_account")
        .arg("init")
        .write_stdin("\n\n\n\n\n\n\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("Production schema [PROD]"))
        .stdout(predicate::str::contains("Snowflake account identifier").not());

    let config = fs::read_to_string(directory.path().join("embrasure-check.yml")).unwrap();
    assert!(config.contains("profile: analytics"));
    assert!(config.contains("account: my_org-my_account"));
    assert!(config.contains("user: DBT_CI"));
    assert!(config.contains("production_schema: PROD"));
    assert!(!config.contains("JACOB"));
}

#[test]
fn doctor_returns_execution_failure_for_a_missing_config() {
    Command::cargo_bin("embrasure")
        .unwrap()
        .args(["doctor", "--config", "definitely-missing.yml", "--json"])
        .assert()
        .code(3)
        .stdout(predicate::str::contains(r#""ready": false"#))
        .stdout(predicate::str::contains("could not read config"));
}

#[test]
fn run_remains_an_alias_for_check() {
    Command::cargo_bin("embrasure")
        .unwrap()
        .args(["run", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Build and compare changed dbt models",
        ));
}

#[test]
fn check_exposes_quick_and_deep_modes() {
    Command::cargo_bin("embrasure")
        .unwrap()
        .args(["check", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--mode <MODE>"))
        .stdout(predicate::str::contains("quick"))
        .stdout(predicate::str::contains("deep"));
}

#[test]
fn check_exposes_scope_incremental_and_report_controls() {
    Command::cargo_bin("embrasure")
        .unwrap()
        .args(["check", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--downstream <DOWNSTREAM>"))
        .stdout(predicate::str::contains("--critical-tag <CRITICAL_TAGS>"))
        .stdout(predicate::str::contains(
            "--incremental-mode <INCREMENTAL_MODE>",
        ))
        .stdout(predicate::str::contains(
            "--report-version <REPORT_VERSION>",
        ))
        .stdout(predicate::str::contains("possible values: 1, 2, 3"))
        .stdout(predicate::str::contains("--verbose"));
}

#[test]
fn legacy_report_version_requires_json_output() {
    Command::cargo_bin("embrasure")
        .unwrap()
        .args(["check", "--report-version", "1"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--json"));
}

#[test]
fn check_exposes_explicit_cloud_handoff_controls() {
    Command::cargo_bin("embrasure")
        .unwrap()
        .args(["check", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--cloud"))
        .stdout(predicate::str::contains("--context <BUSINESS_INTENT>"))
        .stdout(predicate::str::contains("--context-file <PATH>"));
}

#[test]
fn cloud_context_cannot_accidentally_enable_network_handoff() {
    Command::cargo_bin("embrasure")
        .unwrap()
        .args(["check", "--context", "one row per order"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--cloud"));
}

#[test]
fn cloud_subcommands_are_discoverable() {
    Command::cargo_bin("embrasure")
        .unwrap()
        .args(["cloud", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("login"))
        .stdout(predicate::str::contains("whoami"))
        .stdout(predicate::str::contains("logout"))
        .stdout(predicate::str::contains("status"));
}

#[test]
fn global_config_works_before_and_after_subcommands() {
    Command::cargo_bin("embrasure")
        .unwrap()
        .args(["--config", "missing-a.yml", "doctor", "--json"])
        .assert()
        .code(3)
        .stdout(predicate::str::contains("missing-a.yml"));
    Command::cargo_bin("embrasure")
        .unwrap()
        .args(["doctor", "--config", "missing-b.yml", "--json"])
        .assert()
        .code(3)
        .stdout(predicate::str::contains("missing-b.yml"));
    Command::cargo_bin("embrasure")
        .unwrap()
        .args(["cloud", "status", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--config"));
}

#[test]
fn completions_support_only_documented_shells() {
    for shell in ["bash", "zsh", "fish"] {
        Command::cargo_bin("embrasure")
            .unwrap()
            .args(["completion", shell])
            .assert()
            .success()
            .stdout(predicate::str::is_empty().not());
    }
    Command::cargo_bin("embrasure")
        .unwrap()
        .args(["completion", "powershell"])
        .assert()
        .failure();
}

#[test]
fn dry_run_conflicts_with_cloud_and_json_is_ansi_free() {
    Command::cargo_bin("embrasure")
        .unwrap()
        .args(["check", "--dry-run", "--cloud"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("cannot be used with"));
    Command::cargo_bin("embrasure")
        .unwrap()
        .args(["check", "--json", "--config", "missing.yml"])
        .assert()
        .code(3)
        .stdout(predicate::str::contains("\x1b").not());
}
