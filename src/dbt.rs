use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    env, fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use anyhow::{Context, Result, bail};
use chrono::Utc;
use globset::Glob;
use rand::{Rng, distributions::Alphanumeric};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tempfile::TempDir;

use crate::{
    config::{AccountConfig, AuthConfig, Config},
    report::{CoverageGap, ImpactReport, ImpactedAsset},
};

#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    pub nodes: BTreeMap<String, ManifestNode>,
    #[serde(default)]
    pub exposures: BTreeMap<String, Exposure>,
    #[serde(default)]
    pub child_map: BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ManifestNode {
    pub unique_id: String,
    pub name: String,
    pub resource_type: String,
    pub database: Option<String>,
    pub schema: String,
    pub alias: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct DependsOn {
    #[serde(default)]
    pub nodes: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Exposure {
    pub unique_id: String,
    pub name: String,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub depends_on: DependsOn,
}

#[derive(Debug)]
pub struct DbtContext {
    _scratch: TempDir,
    pub repo_root: PathBuf,
    pub profiles_dir: PathBuf,
    state_dirs: BTreeMap<String, PathBuf>,
    manifests: BTreeMap<String, Manifest>,
    production_manifests: BTreeMap<String, Manifest>,
    base_worktree: Option<PathBuf>,
}

#[derive(Debug)]
pub struct BuildResult {
    pub passed: bool,
    pub failures: Vec<BuildFailure>,
}

#[derive(Debug)]
pub struct BuildFailure {
    pub unique_id: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
struct ProfileFile {
    #[serde(flatten)]
    profiles: BTreeMap<String, Profile>,
}

#[derive(Debug, Clone, Serialize)]
struct Profile {
    target: String,
    outputs: BTreeMap<String, ProfileOutput>,
}

#[derive(Debug, Clone, Serialize)]
struct ProfileOutput {
    #[serde(rename = "type")]
    kind: &'static str,
    account: String,
    user: String,
    role: String,
    database: String,
    warehouse: String,
    schema: String,
    threads: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    authenticator: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    private_key_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    private_key_passphrase: Option<String>,
    query_tag: String,
    session_parameters: BTreeMap<String, Value>,
}

impl Manifest {
    pub fn load(path: &Path) -> Result<Self> {
        let bytes = fs::read(path)
            .with_context(|| format!("could not read dbt manifest {}", path.display()))?;
        serde_json::from_slice(&bytes)
            .with_context(|| format!("invalid dbt manifest {}", path.display()))
    }

    pub fn descendants(&self, roots: &BTreeSet<String>) -> BTreeSet<String> {
        let mut found = roots.clone();
        let mut queue: VecDeque<String> = roots.iter().cloned().collect();
        while let Some(node) = queue.pop_front() {
            for child in self.child_map.get(&node).into_iter().flatten() {
                if found.insert(child.clone()) {
                    queue.push_back(child.clone());
                }
            }
        }
        found
    }

    pub fn node_id(&self, id_or_name: &str) -> Option<String> {
        if self.nodes.contains_key(id_or_name) {
            return Some(id_or_name.to_owned());
        }
        let mut matches = self.nodes.values().filter(|node| node.name == id_or_name);
        let found = matches.next()?.unique_id.clone();
        if matches.next().is_some() {
            None
        } else {
            Some(found)
        }
    }

    pub fn impact(&self, selected: &BTreeSet<String>) -> ImpactReport {
        let downstream = self.descendants(selected);
        let mut dbt_models = downstream
            .iter()
            .filter_map(|id| self.nodes.get(id))
            .filter(|node| node.resource_type == "model")
            .map(|node| ImpactedAsset {
                id: node.unique_id.clone(),
                name: node.name.clone(),
                url: None,
            })
            .collect::<Vec<_>>();
        let mut dbt_exposures = self
            .exposures
            .values()
            .filter(|exposure| {
                exposure
                    .depends_on
                    .nodes
                    .iter()
                    .any(|id| downstream.contains(id))
            })
            .map(|exposure| ImpactedAsset {
                id: exposure.unique_id.clone(),
                name: exposure.name.clone(),
                url: exposure.url.clone(),
            })
            .collect::<Vec<_>>();
        dbt_models.sort();
        dbt_exposures.sort();
        ImpactReport {
            dbt_models,
            dbt_exposures,
            ..ImpactReport::default()
        }
    }
}

pub fn prepare(
    config: &Config,
    base: &str,
    ci_schema: &str,
    query_tag: &str,
) -> Result<DbtContext> {
    let scratch = tempfile::tempdir().context("could not create temporary dbt directory")?;
    let repo_root = git_output(&config.dbt.project_dir, &["rev-parse", "--show-toplevel"])?;
    let repo_root = PathBuf::from(repo_root.trim());

    let profiles_dir = scratch.path().join("profiles");
    fs::create_dir_all(&profiles_dir)?;
    write_profiles(
        config,
        &profiles_dir,
        |_account| ci_schema.to_owned(),
        query_tag,
    )?;

    let mut worktree_registration = None;
    let state_dirs = if let Some(state) = &config.dbt.state_dir {
        if config.accounts.len() > 1 {
            for account in &config.accounts {
                let manifest = state.join(&account.name).join("manifest.json");
                if !manifest.exists() {
                    bail!(
                        "multiple accounts require target-specific state at {}; omit dbt.state_dir to generate it automatically",
                        manifest.display()
                    );
                }
            }
        }
        config
            .accounts
            .iter()
            .map(|account| {
                let account_state = state.join(&account.name);
                let directory = if account_state.join("manifest.json").exists() {
                    account_state
                } else {
                    state.clone()
                };
                (account.name.clone(), directory)
            })
            .collect::<BTreeMap<_, _>>()
    } else {
        let base_worktree = scratch.path().join("base-worktree");
        run_git(
            &repo_root,
            &[
                "worktree",
                "add",
                "--detach",
                path_str(&base_worktree)?,
                base,
            ],
        )?;
        worktree_registration = Some(WorktreeRegistration {
            repo: repo_root.clone(),
            path: base_worktree.clone(),
            active: true,
        });
        let relative_project = config
            .dbt
            .project_dir
            .strip_prefix(&repo_root)
            .context("dbt.project_dir must be inside the Git repository")?;
        let base_project = base_worktree.join(relative_project);
        let base_profiles = scratch.path().join("base-profiles");
        fs::create_dir_all(&base_profiles)?;
        write_profiles(
            config,
            &base_profiles,
            |account| account.production_schema.clone(),
            query_tag,
        )?;
        maybe_dbt_deps(config, &base_project, &base_profiles)?;
        let mut state_dirs = BTreeMap::new();
        for account in &config.accounts {
            let state_dir = scratch.path().join("base-state").join(&account.name);
            dbt_command(
                config,
                &base_project,
                &base_profiles,
                &[
                    "parse",
                    "--target",
                    &account.name,
                    "--target-path",
                    path_str(&state_dir)?,
                ],
            )?;
            state_dirs.insert(account.name.clone(), state_dir);
        }
        state_dirs
    };

    maybe_dbt_deps(config, &config.dbt.project_dir, &profiles_dir)?;
    let mut manifests = BTreeMap::new();
    let mut production_manifests = BTreeMap::new();
    for account in &config.accounts {
        let current_target = scratch.path().join("current-target").join(&account.name);
        dbt_command(
            config,
            &config.dbt.project_dir,
            &profiles_dir,
            &[
                "parse",
                "--target",
                &account.name,
                "--target-path",
                path_str(&current_target)?,
            ],
        )?;
        manifests.insert(
            account.name.clone(),
            Manifest::load(&current_target.join("manifest.json"))?,
        );
        let state_dir = state_dirs
            .get(&account.name)
            .context("missing account state directory")?;
        production_manifests.insert(
            account.name.clone(),
            Manifest::load(&state_dir.join("manifest.json"))?,
        );
    }
    let base_worktree = worktree_registration.as_mut().map(|registration| {
        registration.active = false;
        registration.path.clone()
    });

    Ok(DbtContext {
        _scratch: scratch,
        repo_root,
        profiles_dir,
        state_dirs,
        manifests,
        production_manifests,
        base_worktree,
    })
}

impl DbtContext {
    pub fn manifest(&self, account: &str) -> Result<&Manifest> {
        self.manifests
            .get(account)
            .with_context(|| format!("missing current manifest for account {account}"))
    }

    pub fn production_manifest(&self, account: &str) -> Result<&Manifest> {
        self.production_manifests
            .get(account)
            .with_context(|| format!("missing production manifest for account {account}"))
    }

    pub fn state_dir(&self, account: &str) -> Result<&Path> {
        self.state_dirs
            .get(account)
            .map(PathBuf::as_path)
            .with_context(|| format!("missing state directory for account {account}"))
    }

    fn build_target(&self, account: &str) -> PathBuf {
        self._scratch.path().join("build-target").join(account)
    }

    pub fn cleanup_worktree(&mut self) -> Result<()> {
        let Some(path) = self.base_worktree.take() else {
            return Ok(());
        };
        run_git(
            &self.repo_root,
            &["worktree", "remove", "--force", path_str(&path)?],
        )
    }

    pub fn changed_paths(&self, base: &str) -> Result<Vec<String>> {
        let output = git_output(&self.repo_root, &["diff", "--name-only", base])?;
        let mut paths = output.lines().map(str::to_owned).collect::<Vec<_>>();
        let untracked = git_output(
            &self.repo_root,
            &["ls-files", "--others", "--exclude-standard"],
        )?;
        paths.extend(untracked.lines().map(str::to_owned));
        paths.sort();
        paths.dedup();
        Ok(paths)
    }
}

impl Drop for DbtContext {
    fn drop(&mut self) {
        if let Some(path) = self.base_worktree.take() {
            let _ = Command::new("git")
                .arg("-C")
                .arg(&self.repo_root)
                .args(["worktree", "remove", "--force"])
                .arg(path)
                .output();
        }
    }
}

pub fn select_models(
    config: &Config,
    context: &DbtContext,
    account: &AccountConfig,
    changed_paths: &[String],
) -> Result<BTreeSet<String>> {
    let manifest = context.manifest(&account.name)?;
    let mut selector = String::from("state:modified+");
    if let Some(account_selector) = &account.selector {
        selector.push(',');
        selector.push_str(account_selector);
    }
    let output = dbt_output(
        config,
        &config.dbt.project_dir,
        &context.profiles_dir,
        &[
            "ls",
            "--quiet",
            "--target",
            &account.name,
            "--state",
            path_str(context.state_dir(&account.name)?)?,
            "--select",
            &selector,
            "--resource-type",
            "model",
            "--output",
            "json",
            "--output-keys",
            "unique_id",
        ],
    )?;
    let mut selected = BTreeSet::new();
    for line in output.lines().filter(|line| !line.trim().is_empty()) {
        if let Ok(value) = serde_json::from_str::<Value>(line)
            && let Some(id) = value.get("unique_id").and_then(Value::as_str)
        {
            selected.insert(id.to_owned());
        }
    }

    for mapping in &config.external_changes {
        let matcher = Glob::new(&mapping.path)
            .with_context(|| format!("invalid external change glob {}", mapping.path))?
            .compile_matcher();
        if changed_paths.iter().any(|path| matcher.is_match(path)) {
            for configured in &mapping.models {
                let id = manifest.node_id(configured).with_context(|| {
                    format!("external change model {configured} is missing or ambiguous")
                })?;
                selected.extend(manifest.descendants(&BTreeSet::from([id])));
            }
        }
    }
    if let Some(account_selector) = &account.selector {
        let allowed_output = dbt_output(
            config,
            &config.dbt.project_dir,
            &context.profiles_dir,
            &[
                "ls",
                "--quiet",
                "--target",
                &account.name,
                "--select",
                account_selector,
                "--resource-type",
                "model",
                "--output",
                "json",
                "--output-keys",
                "unique_id",
            ],
        )?;
        let allowed = unique_ids(&allowed_output);
        selected.retain(|id| allowed.contains(id));
    }
    selected.retain(|id| {
        manifest
            .nodes
            .get(id)
            .is_some_and(|node| node.resource_type == "model")
    });
    Ok(selected)
}

fn unique_ids(output: &str) -> BTreeSet<String> {
    output
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter_map(|value| {
            value
                .get("unique_id")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .collect()
}

pub fn build_models(
    config: &Config,
    context: &DbtContext,
    account: &AccountConfig,
    selected: &BTreeSet<String>,
) -> Result<BuildResult> {
    if selected.is_empty() {
        return Ok(BuildResult {
            passed: true,
            failures: vec![],
        });
    }
    let target_path = context.build_target(&account.name);
    let mut owned = vec![
        "build".to_owned(),
        "--target".to_owned(),
        account.name.clone(),
        "--state".to_owned(),
        path_str(context.state_dir(&account.name)?)?.to_owned(),
        "--defer".to_owned(),
        "--fail-fast".to_owned(),
        "--target-path".to_owned(),
        path_str(&target_path)?.to_owned(),
        "--select".to_owned(),
    ];
    owned.extend(selected.iter().cloned());
    let args = owned.iter().map(String::as_str).collect::<Vec<_>>();
    let output = dbt_process(
        config,
        &config.dbt.project_dir,
        &context.profiles_dir,
        &args,
    )?;
    if output.status.success() {
        return Ok(BuildResult {
            passed: true,
            failures: vec![],
        });
    }
    let mut failures = load_run_failures(&target_path.join("run_results.json"))?;
    if failures.is_empty() {
        failures.push(BuildFailure {
            unique_id: "dbt".into(),
            message: command_failure_message(&output),
        });
    }
    Ok(BuildResult {
        passed: false,
        failures,
    })
}

fn load_run_failures(path: &Path) -> Result<Vec<BuildFailure>> {
    if !path.exists() {
        return Ok(vec![]);
    }
    let value: Value = serde_json::from_slice(&fs::read(path)?)
        .with_context(|| format!("invalid dbt run results {}", path.display()))?;
    let mut failures = value
        .get("results")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|result| {
            matches!(
                result.get("status").and_then(Value::as_str),
                Some("error" | "fail")
            )
        })
        .map(|result| BuildFailure {
            unique_id: result
                .get("unique_id")
                .and_then(Value::as_str)
                .unwrap_or("dbt")
                .to_owned(),
            message: result
                .get("message")
                .and_then(Value::as_str)
                .filter(|message| !message.trim().is_empty())
                .unwrap_or("dbt reported a failure without a message")
                .to_owned(),
        })
        .collect::<Vec<_>>();
    failures.sort_by(|a, b| {
        a.unique_id
            .cmp(&b.unique_id)
            .then(a.message.cmp(&b.message))
    });
    Ok(failures)
}

fn command_failure_message(output: &Output) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{}\n{}", stdout.trim(), stderr.trim());
    let trimmed = combined.trim();
    if trimmed.is_empty() {
        format!("dbt exited with {}", output.status)
    } else {
        trimmed
            .chars()
            .rev()
            .take(4000)
            .collect::<String>()
            .chars()
            .rev()
            .collect()
    }
}

pub fn ci_schema_name(prefix: &str, repo: &Path) -> Result<String> {
    let sha = git_output(repo, &["rev-parse", "--short=8", "HEAD"])?;
    let random: String = rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(6)
        .map(char::from)
        .collect();
    let timestamp = Utc::now().format("%Y%m%d%H%M%S");
    Ok(format!(
        "{}_{}_{}_{}",
        prefix.to_ascii_uppercase(),
        sha.trim().to_ascii_uppercase(),
        timestamp,
        random.to_ascii_uppercase()
    ))
}

pub fn coverage_gaps(manifest: &Manifest, selected: &BTreeSet<String>) -> Vec<CoverageGap> {
    selected
        .iter()
        .filter_map(|id| manifest.nodes.get(id))
        .filter(|node| {
            manifest
                .child_map
                .get(&node.unique_id)
                .into_iter()
                .flatten()
                .any(|child| {
                    manifest
                        .nodes
                        .get(child)
                        .is_some_and(|child_node| child_node.resource_type == "model")
                })
        })
        .map(|node| CoverageGap {
            scope: node.unique_id.clone(),
            check: "column_lineage".into(),
            reason:
                "dbt manifest artifacts do not contain authoritative column-level dependency edges"
                    .into(),
        })
        .collect()
}

struct WorktreeRegistration {
    repo: PathBuf,
    path: PathBuf,
    active: bool,
}

impl Drop for WorktreeRegistration {
    fn drop(&mut self) {
        if self.active {
            let _ = Command::new("git")
                .arg("-C")
                .arg(&self.repo)
                .args(["worktree", "remove", "--force"])
                .arg(&self.path)
                .output();
        }
    }
}

fn write_profiles(
    config: &Config,
    dir: &Path,
    schema: impl Fn(&AccountConfig) -> String,
    query_tag: &str,
) -> Result<()> {
    let mut outputs = BTreeMap::new();
    for account in &config.accounts {
        let mut session_parameters = BTreeMap::new();
        session_parameters.insert("QUERY_TAG".to_owned(), Value::String(query_tag.to_owned()));
        session_parameters.insert(
            "STATEMENT_TIMEOUT_IN_SECONDS".to_owned(),
            Value::from(config.safety.statement_timeout_seconds),
        );
        let (authenticator, token, private_key_path, private_key_passphrase) = match &account.auth {
            AuthConfig::Oauth { token_env } => (
                Some("oauth"),
                Some(env::var(token_env).with_context(|| {
                    format!("missing OAuth token environment variable {token_env}")
                })?),
                None,
                None,
            ),
            AuthConfig::KeyPair {
                private_key_path,
                passphrase_env,
            } => (
                None,
                None,
                Some(private_key_path.to_string_lossy().into_owned()),
                passphrase_env
                    .as_ref()
                    .map(|name| {
                        env::var(name).with_context(|| {
                            format!("missing key passphrase environment variable {name}")
                        })
                    })
                    .transpose()?,
            ),
        };
        outputs.insert(
            account.name.clone(),
            ProfileOutput {
                kind: "snowflake",
                account: account.account.clone(),
                user: account.user.clone(),
                role: account.role.clone(),
                database: account.database.clone(),
                warehouse: account.warehouse.clone(),
                schema: schema(account),
                threads: config.dbt.threads,
                authenticator,
                token,
                private_key_path,
                private_key_passphrase,
                query_tag: query_tag.to_owned(),
                session_parameters,
            },
        );
    }
    let file = ProfileFile {
        profiles: BTreeMap::from([(
            config.dbt.profile.clone(),
            Profile {
                target: config.accounts[0].name.clone(),
                outputs,
            },
        )]),
    };
    let bytes = serde_yaml::to_string(&file)?;
    fs::write(dir.join("profiles.yml"), bytes).context("could not write temporary dbt profile")
}

fn maybe_dbt_deps(config: &Config, project: &Path, profiles: &Path) -> Result<()> {
    let has_packages =
        project.join("packages.yml").exists() || project.join("dependencies.yml").exists();
    if has_packages && !project.join("dbt_packages").exists() {
        dbt_command(config, project, profiles, &["deps"])?;
    }
    Ok(())
}

fn dbt_command(config: &Config, project: &Path, profiles: &Path, args: &[&str]) -> Result<()> {
    let output = dbt_process(config, project, profiles, args)?;
    if !output.status.success() {
        bail!(
            "dbt {} failed ({}): {}",
            args.first().unwrap_or(&"command"),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn dbt_output(config: &Config, project: &Path, profiles: &Path, args: &[&str]) -> Result<String> {
    let output = dbt_process(config, project, profiles, args)?;
    if !output.status.success() {
        bail!(
            "dbt {} failed ({}): {}",
            args.first().unwrap_or(&"command"),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    String::from_utf8(output.stdout).context("dbt returned non-UTF-8 output")
}

fn dbt_process(config: &Config, project: &Path, profiles: &Path, args: &[&str]) -> Result<Output> {
    let mut command = Command::new(&config.dbt.command);
    command
        .args(args)
        .arg("--project-dir")
        .arg(project)
        .arg("--profiles-dir")
        .arg(profiles)
        .arg("--profile")
        .arg(&config.dbt.profile)
        .current_dir(project);
    command
        .output()
        .with_context(|| format!("could not run {}", config.dbt.command))
}

fn run_git(repo: &Path, args: &[&str]) -> Result<()> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .context("could not run git")?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args.first().unwrap_or(&"command"),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn git_output(repo: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .context("could not run git")?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args.first().unwrap_or(&"command"),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    String::from_utf8(output.stdout).context("git returned non-UTF-8 output")
}

fn path_str(path: &Path) -> Result<&str> {
    path.to_str().context("path contains invalid UTF-8")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str) -> ManifestNode {
        ManifestNode {
            unique_id: id.into(),
            name: id.into(),
            resource_type: "model".into(),
            database: None,
            schema: "s".into(),
            alias: id.into(),
        }
    }

    #[test]
    fn descendants_are_transitive() {
        let manifest = Manifest {
            nodes: BTreeMap::from([
                ("a".into(), node("a")),
                ("b".into(), node("b")),
                ("c".into(), node("c")),
            ]),
            exposures: BTreeMap::new(),
            child_map: BTreeMap::from([
                ("a".into(), vec!["b".into()]),
                ("b".into(), vec!["c".into()]),
            ]),
        };
        assert_eq!(
            manifest.descendants(&BTreeSet::from(["a".into()])),
            BTreeSet::from(["a".into(), "b".into(), "c".into()])
        );
    }

    #[test]
    fn run_results_preserve_exact_test_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run_results.json");
        fs::write(&path, r#"{"results":[{"unique_id":"test.analytics.not_null_orders_id","status":"fail","message":"Got 2 results, configured to fail if != 0"},{"unique_id":"model.analytics.orders","status":"success","message":null}]}"#).unwrap();
        let failures = load_run_failures(&path).unwrap();
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].unique_id, "test.analytics.not_null_orders_id");
        assert!(failures[0].message.contains("Got 2 results"));
    }
}
