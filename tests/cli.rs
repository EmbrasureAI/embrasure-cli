use assert_cmd::Command;
use predicates::prelude::*;

#[test]
fn help_exposes_enterprise_setup_commands() {
    Command::cargo_bin("embrasure-check")
        .unwrap()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("doctor"))
        .stdout(predicate::str::contains("auth"));
}

#[test]
fn doctor_returns_execution_failure_for_a_missing_config() {
    Command::cargo_bin("embrasure-check")
        .unwrap()
        .args(["doctor", "--config", "definitely-missing.yml", "--json"])
        .assert()
        .code(3)
        .stdout(predicate::str::contains(r#""ready": false"#))
        .stdout(predicate::str::contains("could not read config"));
}
