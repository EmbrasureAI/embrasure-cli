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
        .stdout(predicate::str::contains("view"))
        .stdout(predicate::str::contains("doctor"))
        .stdout(predicate::str::contains("auth"));
}

#[test]
fn view_rejects_reports_before_version_four() {
    let directory = tempfile::tempdir().unwrap();
    let report = directory.path().join("report.json");
    fs::write(&report, r#"{"schema_version":3}"#).unwrap();

    Command::cargo_bin("embrasure")
        .unwrap()
        .args(["view", "--no-open"])
        .arg(report)
        .assert()
        .code(3)
        .stderr(predicate::str::contains("requires report version 4"));
}

#[test]
fn visual_example_matches_the_v4_report_contract() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let schema: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(root.join("schemas/report-v4.schema.json")).unwrap(),
    )
    .unwrap();
    let report: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(root.join("examples/visual-report.json")).unwrap(),
    )
    .unwrap();
    let validator = jsonschema::validator_for(&schema).unwrap();

    if let Err(error) = validator.validate(&report) {
        panic!("visual report example violates the v4 schema: {error}");
    }
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
fn missing_dbt_check_preserves_json_and_exit_code() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(
        directory.path().join("embrasure-check.yml"),
        r#"version: 1
dbt:
  project_dir: .
  profile: analytics
  command: missing-dbt-for-embrasure-test
accounts:
  - name: primary
    account: org-account
    user: validator
    role: validator
    database: analytics
    warehouse: dbt_ci
    production_schema: prod
    auth: { type: programmatic_access_token, token_env: UNUSED_TOKEN }
"#,
    )
    .unwrap();

    let assertion = Command::cargo_bin("embrasure")
        .unwrap()
        .current_dir(directory.path())
        .env("SHELL", "/bin/zsh")
        .args(["check", "--json"])
        .assert()
        .code(3);
    let stdout = std::str::from_utf8(&assertion.get_output().stdout).unwrap();
    let report: serde_json::Value = serde_json::from_str(stdout).unwrap();
    assert_eq!(report["exit_code"], 3);
    assert!(report["ci_schemas"].as_array().unwrap().is_empty());
    let error = report["execution_errors"][0].as_str().unwrap();
    assert!(error.contains("Install dbt Core and the adapter"));
    assert!(error.contains("missing-dbt-for-embrasure-test --version"));
    assert!(!error.contains("No such file or directory"));
    #[cfg(windows)]
    {
        assert!(error.contains("detected shell: powershell"));
        assert!(error.contains("Python environment"));
        assert!(error.contains("user PATH"));
    }
    #[cfg(not(windows))]
    {
        assert!(error.contains("detected shell: zsh"));
        assert!(error.contains("~/.zshrc"));
    }
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
        .stdout(predicate::str::contains("possible values: 1, 2, 3, 4"))
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
#[cfg(feature = "cloud-demo")]
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
#[cfg(feature = "cloud-demo")]
fn cloud_context_cannot_accidentally_enable_network_handoff() {
    Command::cargo_bin("embrasure")
        .unwrap()
        .args(["check", "--context", "one row per order"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--cloud"));
}

#[test]
#[cfg(feature = "cloud-demo")]
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
}

#[test]
#[cfg(not(feature = "cloud-demo"))]
fn default_release_has_no_cloud_surface() {
    Command::cargo_bin("embrasure")
        .unwrap()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("cloud").not());
    Command::cargo_bin("embrasure")
        .unwrap()
        .args(["check", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--cloud").not())
        .stdout(predicate::str::contains("--context").not());
    Command::cargo_bin("embrasure")
        .unwrap()
        .arg("cloud")
        .assert()
        .failure()
        .stderr(predicate::str::contains("unrecognized subcommand"));
}

#[test]
fn completions_support_only_documented_shells() {
    for shell in ["bash", "zsh", "fish", "powershell"] {
        Command::cargo_bin("embrasure")
            .unwrap()
            .args(["completion", shell])
            .assert()
            .success()
            .stdout(predicate::str::is_empty().not());
    }
    Command::cargo_bin("embrasure")
        .unwrap()
        .args(["completion", "cmd"])
        .assert()
        .failure();
}

#[test]
fn json_output_is_ansi_free() {
    Command::cargo_bin("embrasure")
        .unwrap()
        .args(["check", "--json", "--config", "missing.yml"])
        .assert()
        .code(3)
        .stdout(predicate::str::contains("\x1b").not());
}

#[test]
#[cfg(feature = "cloud-demo")]
fn dry_run_conflicts_with_cloud() {
    Command::cargo_bin("embrasure")
        .unwrap()
        .args(["check", "--dry-run", "--cloud"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("cannot be used with"));
}
