use std::{path::Path, process::Output};

use anyhow::{Context, Result, bail};

pub fn output(repo: &Path, args: &[&str]) -> Result<Output> {
    std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .with_context(|| format!("could not run git {}", args.join(" ")))
}

pub fn text(repo: &Path, args: &[&str]) -> Result<String> {
    let output = output(repo, args)?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

pub fn success(repo: &Path, args: &[&str]) -> Result<()> {
    text(repo, args).map(|_| ())
}
