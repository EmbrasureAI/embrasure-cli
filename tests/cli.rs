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
        .stdout(predicate::str::contains("auth"));
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
