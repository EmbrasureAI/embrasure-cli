use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use anyhow::{Context, Result, bail};
use chrono::Utc;
use globset::Glob;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tempfile::TempDir;
use uuid::Uuid;

use crate::{
    auth::ResolvedAuth,
    config::{AccountConfig, Config},
    git,
    report::{ImpactReport, ImpactedAsset, LineageChange, LineageChangeKind, LineageEdge},
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
    pub fqn: Vec<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub depends_on: DependsOn,
    #[serde(default)]
    pub config: NodeConfig,
    #[serde(default, alias = "compiled_sql")]
    pub compiled_code: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct NodeConfig {
    #[serde(default)]
    pub quoting: QuotePolicy,
    #[serde(default)]
    pub materialized: String,
    #[serde(default)]
    pub unique_key: Option<Value>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct QuotePolicy {
    pub database: Option<bool>,
    pub schema: Option<bool>,
    pub identifier: Option<bool>,
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
    scratch: TempDir,
    pub repo_root: PathBuf,
    pub profiles_dir: PathBuf,
    state_dirs: BTreeMap<String, PathBuf>,
    manifests: BTreeMap<String, Manifest>,
    production_manifests: BTreeMap<String, Manifest>,
    base_worktree: Option<WorktreeRegistration>,
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
    password: Option<String>,
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

    pub fn model_descendants(&self, roots: &BTreeSet<String>) -> BTreeSet<String> {
        self.descendants(roots)
            .into_iter()
            .filter(|id| {
                self.nodes
                    .get(id)
                    .is_some_and(|node| node.resource_type == "model")
            })
            .collect()
    }

    pub fn critical_targets(
        &self,
        impacted: &BTreeSet<String>,
        critical_tags: &BTreeSet<String>,
        configured: &BTreeSet<String>,
    ) -> BTreeSet<String> {
        let mut targets = impacted
            .iter()
            .filter(|id| {
                configured.contains(*id)
                    || self
                        .nodes
                        .get(*id)
                        .is_some_and(|node| node.tags.iter().any(|tag| critical_tags.contains(tag)))
            })
            .cloned()
            .collect::<BTreeSet<_>>();
        for exposure in self.exposures.values() {
            targets.extend(
                exposure
                    .depends_on
                    .nodes
                    .iter()
                    .filter(|id| impacted.contains(*id))
                    .cloned(),
            );
        }
        targets
    }

    pub fn paths_between(
        &self,
        roots: &BTreeSet<String>,
        targets: &BTreeSet<String>,
    ) -> BTreeSet<String> {
        let downstream = self.descendants(roots);
        let mut selected = targets
            .intersection(&downstream)
            .cloned()
            .collect::<BTreeSet<_>>();
        let mut queue = selected.iter().cloned().collect::<VecDeque<_>>();
        while let Some(id) = queue.pop_front() {
            let Some(node) = self.nodes.get(&id) else {
                continue;
            };
            for parent in &node.depends_on.nodes {
                if downstream.contains(parent) && selected.insert(parent.clone()) {
                    queue.push_back(parent.clone());
                }
            }
        }
        selected.extend(
            roots
                .iter()
                .filter(|id| self.nodes.contains_key(*id))
                .cloned(),
        );
        selected
            .into_iter()
            .filter(|id| {
                self.nodes
                    .get(id)
                    .is_some_and(|node| node.resource_type == "model")
            })
            .collect()
    }

    pub fn inferred_primary_key(&self, id: &str) -> Result<Option<Vec<String>>> {
        let Some(value) = self
            .nodes
            .get(id)
            .and_then(|node| node.config.unique_key.as_ref())
        else {
            return Ok(None);
        };
        let keys = match value {
            Value::String(key) => vec![key.clone()],
            Value::Array(values) => values
                .iter()
                .map(|value| {
                    value
                        .as_str()
                        .map(ToOwned::to_owned)
                        .context("dbt unique_key list contains a non-string value")
                })
                .collect::<Result<Vec<_>>>()?,
            _ => bail!("dbt unique_key is not a string or string list"),
        };
        if keys.is_empty() || keys.iter().any(|key| !is_simple_identifier(key)) {
            bail!("dbt unique_key is an expression rather than a column identifier list");
        }
        Ok(Some(keys))
    }

    pub fn is_incremental(&self, id: &str) -> bool {
        self.nodes
            .get(id)
            .is_some_and(|node| node.config.materialized.eq_ignore_ascii_case("incremental"))
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

    pub fn removed_models(&self, current: &Manifest) -> BTreeSet<String> {
        self.nodes
            .iter()
            .filter(|(id, node)| {
                node.resource_type == "model"
                    && current
                        .nodes
                        .get(*id)
                        .is_none_or(|current_node| current_node.resource_type != "model")
            })
            .map(|(id, _)| id.clone())
            .collect()
    }

    fn model_dependencies(&self, node_ids: &BTreeSet<String>) -> BTreeSet<String> {
        node_ids
            .iter()
            .filter_map(|id| self.nodes.get(id))
            .flat_map(|node| node.depends_on.nodes.iter())
            .filter(|id| {
                self.nodes
                    .get(*id)
                    .is_some_and(|node| node.resource_type == "model")
            })
            .cloned()
            .collect()
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
        let mut dbt_lineage = self
            .nodes
            .values()
            .filter(|node| node.resource_type == "model" && downstream.contains(&node.unique_id))
            .flat_map(|node| {
                node.depends_on.nodes.iter().filter_map(|dependency| {
                    let parent = self.nodes.get(dependency)?;
                    (parent.resource_type == "model" && downstream.contains(dependency)).then(
                        || LineageEdge {
                            from: parent.unique_id.clone(),
                            from_name: parent.name.clone(),
                            to: node.unique_id.clone(),
                            to_name: node.name.clone(),
                        },
                    )
                })
            })
            .collect::<Vec<_>>();
        dbt_lineage.extend(self.exposures.values().flat_map(|exposure| {
            exposure.depends_on.nodes.iter().filter_map(|dependency| {
                let parent = self.nodes.get(dependency)?;
                downstream.contains(dependency).then(|| LineageEdge {
                    from: parent.unique_id.clone(),
                    from_name: parent.name.clone(),
                    to: exposure.unique_id.clone(),
                    to_name: exposure.name.clone(),
                })
            })
        }));
        dbt_models.sort();
        dbt_exposures.sort();
        dbt_lineage.sort();
        ImpactReport {
            dbt_models,
            dbt_exposures,
            dbt_lineage,
            ..ImpactReport::default()
        }
    }

    pub fn lineage_changes(&self, production: &Manifest) -> Vec<LineageChange> {
        let current_edges = self.direct_lineage_edges();
        let production_edges = production.direct_lineage_edges();
        let current_keys = current_edges.keys().cloned().collect::<BTreeSet<_>>();
        let production_keys = production_edges.keys().cloned().collect::<BTreeSet<_>>();
        let mut changes = current_keys
            .difference(&production_keys)
            .filter_map(|key| current_edges.get(key))
            .cloned()
            .map(|edge| LineageChange {
                change: LineageChangeKind::Added,
                edge,
            })
            .chain(
                production_keys
                    .difference(&current_keys)
                    .filter_map(|key| production_edges.get(key))
                    .cloned()
                    .map(|edge| LineageChange {
                        change: LineageChangeKind::Removed,
                        edge,
                    }),
            )
            .collect::<Vec<_>>();
        changes.sort();
        changes
    }

    fn direct_lineage_edges(&self) -> BTreeMap<(String, String), LineageEdge> {
        let mut edges = self
            .nodes
            .values()
            .filter(|node| node.resource_type == "model")
            .flat_map(|node| {
                node.depends_on.nodes.iter().map(move |dependency| {
                    let from_name = self
                        .nodes
                        .get(dependency)
                        .map(|parent| parent.name.clone())
                        .unwrap_or_else(|| short_dbt_name(dependency));
                    (
                        (dependency.clone(), node.unique_id.clone()),
                        LineageEdge {
                            from: dependency.clone(),
                            from_name,
                            to: node.unique_id.clone(),
                            to_name: node.name.clone(),
                        },
                    )
                })
            })
            .collect::<BTreeMap<_, _>>();
        edges.extend(self.exposures.values().flat_map(|exposure| {
            exposure.depends_on.nodes.iter().map(move |dependency| {
                let from_name = self
                    .nodes
                    .get(dependency)
                    .map(|parent| parent.name.clone())
                    .unwrap_or_else(|| short_dbt_name(dependency));
                (
                    (dependency.clone(), exposure.unique_id.clone()),
                    LineageEdge {
                        from: dependency.clone(),
                        from_name,
                        to: exposure.unique_id.clone(),
                        to_name: exposure.name.clone(),
                    },
                )
            })
        }));
        edges
    }
}

fn short_dbt_name(unique_id: &str) -> String {
    unique_id.rsplit('.').next().unwrap_or(unique_id).to_owned()
}

pub fn prepare(
    config: &Config,
    auth: &BTreeMap<String, ResolvedAuth>,
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
        auth,
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
        git::success(
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
            auth,
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
    Ok(DbtContext {
        scratch,
        repo_root,
        profiles_dir,
        state_dirs,
        manifests,
        production_manifests,
        base_worktree: worktree_registration,
    })
}

impl DbtContext {
    #[cfg(test)]
    pub fn for_test(
        repo_root: PathBuf,
        manifests: BTreeMap<String, Manifest>,
        production_manifests: BTreeMap<String, Manifest>,
    ) -> Result<Self> {
        let scratch = tempfile::tempdir()?;
        let profiles_dir = scratch.path().join("profiles");
        fs::create_dir_all(&profiles_dir)?;
        let mut state_dirs = BTreeMap::new();
        for account in manifests.keys() {
            let state = scratch.path().join("state").join(account);
            fs::create_dir_all(&state)?;
            state_dirs.insert(account.clone(), state);
        }
        Ok(Self {
            scratch,
            repo_root,
            profiles_dir,
            state_dirs,
            manifests,
            production_manifests,
            base_worktree: None,
        })
    }

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
        self.scratch.path().join("build-target").join(account)
    }

    pub fn build_manifest(&self, account: &str) -> Result<Manifest> {
        Manifest::load(&self.build_target(account).join("manifest.json"))
    }

    pub fn cleanup_worktree(&mut self) -> Result<()> {
        let Some(mut registration) = self.base_worktree.take() else {
            return Ok(());
        };
        remove_worktree(&registration.repo, &registration.path)?;
        registration.active = false;
        Ok(())
    }

    pub fn changed_paths(&self, base: &str) -> Result<Vec<String>> {
        // Local validation includes untracked files because dbt can select newly-created models.
        let output = git::text(&self.repo_root, &["diff", "--name-only", base])?;
        let mut paths = output.lines().map(str::to_owned).collect::<Vec<_>>();
        let untracked = git::text(
            &self.repo_root,
            &["ls-files", "--others", "--exclude-standard"],
        )?;
        paths.extend(untracked.lines().map(str::to_owned));
        paths.sort();
        paths.dedup();
        Ok(paths)
    }
}

pub fn select_changed_models(
    config: &Config,
    context: &DbtContext,
    account: &AccountConfig,
    changed_paths: &[String],
) -> Result<BTreeSet<String>> {
    let manifest = context.manifest(&account.name)?;
    let mut selector = String::from("state:modified");
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
    let mut selected = unique_ids(&output)?;

    let modified_tests = dbt_output(
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
            "state:modified",
            "--resource-type",
            "test",
            "--output",
            "json",
            "--output-keys",
            "unique_id",
        ],
    )?;
    selected.extend(manifest.model_dependencies(&unique_ids(&modified_tests)?));

    for mapping in &config.external_changes {
        let matcher = Glob::new(&mapping.path)
            .with_context(|| format!("invalid external change glob {}", mapping.path))?
            .compile_matcher();
        if changed_paths.iter().any(|path| matcher.is_match(path)) {
            for configured in &mapping.models {
                let id = manifest.node_id(configured).with_context(|| {
                    format!("external change model {configured} is missing or ambiguous")
                })?;
                selected.insert(id);
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
        let allowed = unique_ids(&allowed_output)?;
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

fn is_simple_identifier(value: &str) -> bool {
    let mut characters = value.chars();
    characters
        .next()
        .is_some_and(|character| character.is_ascii_alphabetic() || character == '_')
        && characters.all(|character| character.is_ascii_alphanumeric() || character == '_')
}

fn unique_ids(output: &str) -> Result<BTreeSet<String>> {
    let mut ids = BTreeSet::new();
    for line in output.lines().filter(|line| !line.trim().is_empty()) {
        let value: Value = serde_json::from_str(line)
            .with_context(|| format!("dbt ls returned invalid JSON: {}", truncate(line, 240)))?;
        let id = value
            .get("unique_id")
            .and_then(Value::as_str)
            .context("dbt ls JSON omitted unique_id")?;
        ids.insert(id.to_owned());
    }
    Ok(ids)
}

fn truncate(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

pub fn build_models(
    config: &Config,
    context: &DbtContext,
    account: &AccountConfig,
    selected: &BTreeSet<String>,
    full_refresh: bool,
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
    ];
    if full_refresh {
        owned.push("--full-refresh".to_owned());
    }
    owned.push("--select".to_owned());
    let manifest = context.manifest(&account.name)?;
    for id in selected {
        let node = manifest
            .nodes
            .get(id)
            .with_context(|| format!("selected dbt model {id} is absent from current manifest"))?;
        owned.push(dbt_selector(node)?);
    }
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

fn dbt_selector(node: &ManifestNode) -> Result<String> {
    if node.fqn.is_empty() {
        bail!("dbt model {} has an empty fqn", node.unique_id);
    }
    Ok(format!("fqn:{}", node.fqn.join(".")))
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
    let random = Uuid::new_v4().simple().to_string()[..16].to_owned();
    let timestamp = Utc::now().format("%Y%m%d%H%M%S");
    Ok(format!(
        "{}_{}_{}_{}",
        prefix.to_ascii_uppercase(),
        sha.trim().to_ascii_uppercase(),
        timestamp,
        random.to_ascii_uppercase()
    ))
}

#[derive(Debug)]
struct WorktreeRegistration {
    repo: PathBuf,
    path: PathBuf,
    active: bool,
}

impl Drop for WorktreeRegistration {
    fn drop(&mut self) {
        if self.active {
            let _ = remove_worktree(&self.repo, &self.path);
        }
    }
}

fn remove_worktree(repo: &Path, path: &Path) -> Result<()> {
    git::success(repo, &["worktree", "remove", "--force", path_str(path)?])
}

fn write_profiles(
    config: &Config,
    auth: &BTreeMap<String, ResolvedAuth>,
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
        let resolved = auth
            .get(&account.name)
            .with_context(|| format!("resolved auth is missing for account {}", account.name))?;
        let (authenticator, token, password, private_key_path, private_key_passphrase) =
            match resolved {
                ResolvedAuth::OAuth { token } => {
                    (Some("oauth"), Some(token.clone()), None, None, None)
                }
                ResolvedAuth::KeyPair {
                    private_key_path,
                    passphrase,
                } => (
                    None,
                    None,
                    None,
                    Some(private_key_path.to_string_lossy().into_owned()),
                    passphrase.clone(),
                ),
                ResolvedAuth::ProgrammaticAccessToken { token } => {
                    (None, None, Some(token.clone()), None, None)
                }
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
                password,
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
    dbt_output(config, project, profiles, args).map(|_| ())
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

fn git_output(repo: &Path, args: &[&str]) -> Result<String> {
    git::text(repo, args)
}

fn path_str(path: &Path) -> Result<&str> {
    path.to_str().context("path contains invalid UTF-8")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{auth::ResolvedAuth, config::Config};

    fn node(id: &str) -> ManifestNode {
        ManifestNode {
            unique_id: id.into(),
            name: id.into(),
            resource_type: "model".into(),
            database: None,
            schema: "s".into(),
            alias: id.into(),
            fqn: vec!["test".into(), id.into()],
            tags: vec![],
            depends_on: DependsOn::default(),
            config: NodeConfig::default(),
            compiled_code: None,
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
    fn critical_selection_keeps_every_path_through_a_diamond() {
        let mut left = node("left");
        left.depends_on.nodes = vec!["root".into()];
        let mut right = node("right");
        right.depends_on.nodes = vec!["root".into()];
        let mut critical = node("critical");
        critical.tags = vec!["tier_1".into()];
        critical.depends_on.nodes = vec!["left".into(), "right".into()];
        let manifest = Manifest {
            nodes: BTreeMap::from([
                ("root".into(), node("root")),
                ("left".into(), left),
                ("right".into(), right),
                ("critical".into(), critical),
            ]),
            exposures: BTreeMap::new(),
            child_map: BTreeMap::from([
                ("root".into(), vec!["left".into(), "right".into()]),
                ("left".into(), vec!["critical".into()]),
                ("right".into(), vec!["critical".into()]),
            ]),
        };
        let roots = BTreeSet::from(["root".into()]);
        let impacted = manifest.model_descendants(&roots);
        let targets = manifest.critical_targets(
            &impacted,
            &BTreeSet::from(["tier_1".into()]),
            &BTreeSet::new(),
        );
        assert_eq!(
            manifest.paths_between(&roots, &targets),
            BTreeSet::from([
                "root".into(),
                "left".into(),
                "right".into(),
                "critical".into(),
            ])
        );
    }

    #[test]
    fn models_directly_supporting_exposures_are_critical() {
        let manifest = Manifest {
            nodes: BTreeMap::from([("dashboard_model".into(), node("dashboard_model"))]),
            exposures: BTreeMap::from([(
                "exposure.dashboard".into(),
                Exposure {
                    unique_id: "exposure.dashboard".into(),
                    name: "dashboard".into(),
                    url: None,
                    depends_on: DependsOn {
                        nodes: vec!["dashboard_model".into()],
                    },
                },
            )]),
            child_map: BTreeMap::new(),
        };
        let impacted = BTreeSet::from(["dashboard_model".into()]);
        assert_eq!(
            manifest.critical_targets(&impacted, &BTreeSet::new(), &BTreeSet::new()),
            impacted
        );
    }

    #[test]
    fn simple_dbt_unique_keys_are_inferred_but_expressions_are_rejected() {
        let mut scalar = node("scalar");
        scalar.config.unique_key = Some(Value::String("order_id".into()));
        let mut composite = node("composite");
        composite.config.unique_key = Some(serde_json::json!(["order_id", "line_id"]));
        let mut expression = node("expression");
        expression.config.unique_key = Some(Value::String("coalesce(order_id, -1)".into()));
        let manifest = Manifest {
            nodes: BTreeMap::from([
                ("scalar".into(), scalar),
                ("composite".into(), composite),
                ("expression".into(), expression),
            ]),
            exposures: BTreeMap::new(),
            child_map: BTreeMap::new(),
        };
        assert_eq!(
            manifest.inferred_primary_key("scalar").unwrap(),
            Some(vec!["order_id".into()])
        );
        assert_eq!(
            manifest.inferred_primary_key("composite").unwrap(),
            Some(vec!["order_id".into(), "line_id".into()])
        );
        assert!(manifest.inferred_primary_key("expression").is_err());
    }

    #[test]
    fn impact_contains_model_and_exposure_edges() {
        let mut summary = node("model.project.summary");
        summary.depends_on.nodes = vec!["model.project.orders".into()];
        let manifest = Manifest {
            nodes: BTreeMap::from([
                ("model.project.orders".into(), node("model.project.orders")),
                ("model.project.summary".into(), summary),
            ]),
            exposures: BTreeMap::from([(
                "exposure.project.dashboard".into(),
                Exposure {
                    unique_id: "exposure.project.dashboard".into(),
                    name: "dashboard".into(),
                    url: None,
                    depends_on: DependsOn {
                        nodes: vec!["model.project.summary".into()],
                    },
                },
            )]),
            child_map: BTreeMap::from([(
                "model.project.orders".into(),
                vec!["model.project.summary".into()],
            )]),
        };

        let impact = manifest.impact(&BTreeSet::from(["model.project.orders".into()]));
        assert!(impact.dbt_lineage.iter().any(|edge| {
            edge.from == "model.project.orders" && edge.to == "model.project.summary"
        }));
        assert!(impact.dbt_lineage.iter().any(|edge| {
            edge.from == "model.project.summary" && edge.to == "exposure.project.dashboard"
        }));
    }

    #[test]
    fn lineage_changes_compare_current_and_production_edges() {
        let mut production_summary = node("model.project.summary");
        production_summary.depends_on.nodes = vec!["model.project.orders".into()];
        let production = Manifest {
            nodes: BTreeMap::from([
                ("model.project.orders".into(), node("model.project.orders")),
                ("model.project.summary".into(), production_summary),
            ]),
            exposures: BTreeMap::new(),
            child_map: BTreeMap::new(),
        };
        let mut current_summary = node("model.project.summary");
        current_summary.depends_on.nodes = vec!["model.project.refunds".into()];
        let current = Manifest {
            nodes: BTreeMap::from([
                (
                    "model.project.refunds".into(),
                    node("model.project.refunds"),
                ),
                ("model.project.summary".into(), current_summary),
            ]),
            exposures: BTreeMap::new(),
            child_map: BTreeMap::new(),
        };

        let changes = current.lineage_changes(&production);
        assert!(changes.iter().any(|change| {
            change.change == LineageChangeKind::Added && change.edge.from == "model.project.refunds"
        }));
        assert!(changes.iter().any(|change| {
            change.change == LineageChangeKind::Removed
                && change.edge.from == "model.project.orders"
        }));
    }

    #[test]
    fn removed_models_and_their_current_descendants_are_detected() {
        let production = Manifest {
            nodes: BTreeMap::from([
                ("removed".into(), node("removed")),
                ("downstream".into(), node("downstream")),
            ]),
            exposures: BTreeMap::new(),
            child_map: BTreeMap::from([("removed".into(), vec!["downstream".into()])]),
        };
        let current = Manifest {
            nodes: BTreeMap::from([("downstream".into(), node("downstream"))]),
            exposures: BTreeMap::new(),
            child_map: BTreeMap::new(),
        };
        let removed = production.removed_models(&current);
        assert_eq!(removed, BTreeSet::from(["removed".into()]));
        assert!(production.descendants(&removed).contains("downstream"));
    }

    #[test]
    fn modified_tests_select_their_model_dependencies() {
        let mut test = node("test.orders_not_null");
        test.resource_type = "test".into();
        test.depends_on.nodes = vec!["model.orders".into()];
        let manifest = Manifest {
            nodes: BTreeMap::from([
                ("model.orders".into(), node("model.orders")),
                ("test.orders_not_null".into(), test),
            ]),
            exposures: BTreeMap::new(),
            child_map: BTreeMap::new(),
        };
        assert_eq!(
            manifest.model_dependencies(&BTreeSet::from(["test.orders_not_null".into()])),
            BTreeSet::from(["model.orders".into()])
        );
    }

    #[test]
    fn malformed_dbt_ls_output_is_not_silently_ignored() {
        assert!(unique_ids("not-json").is_err());
    }

    #[test]
    fn build_selector_uses_the_manifest_fqn() {
        assert_eq!(dbt_selector(&node("orders")).unwrap(), "fqn:test.orders");
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

    #[test]
    fn pat_is_written_as_a_temporary_dbt_password() {
        let yaml = r#"
version: 1
accounts:
  - name: ci
    account: org-account
    user: service
    role: dbt_ci
    database: analytics
    warehouse: dbt_ci
    production_schema: prod
    auth: { type: programmatic_access_token, token_env: PAT }
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        let auth = BTreeMap::from([(
            "ci".into(),
            ResolvedAuth::ProgrammaticAccessToken {
                token: "secret-pat".into(),
            },
        )]);
        let dir = tempfile::tempdir().unwrap();
        write_profiles(&config, &auth, dir.path(), |_| "CHECK".into(), "tag").unwrap();
        let profile = fs::read_to_string(dir.path().join("profiles.yml")).unwrap();
        assert!(profile.contains("password: secret-pat"));
        assert!(!profile.contains("authenticator:"));
    }
}
