use assert_cmd::Command;
use predicates::prelude::*;

#[test]
fn help_exposes_enterprise_setup_commands() {
    Command::cargo_bin("embrasure")
        .unwrap()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("check"))
        .stdout(predicate::str::contains("doctor"))
        .stdout(predicate::str::contains("auth"));
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
