use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub version: u8,
    #[serde(default)]
    pub dbt: DbtConfig,
    #[serde(default)]
    pub safety: SafetyConfig,
    #[serde(default)]
    pub comparison: ComparisonConfig,
    #[serde(default)]
    pub validation: ValidationConfig,
    #[serde(default)]
    pub thresholds: Thresholds,
    pub accounts: Vec<AccountConfig>,
    #[serde(default)]
    pub models: BTreeMap<String, ModelConfig>,
    #[serde(default)]
    pub external_changes: Vec<ExternalChange>,
    #[serde(default)]
    pub cross_account_dependencies: Vec<CrossAccountDependency>,
    pub metabase: Option<MetabaseConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DbtConfig {
    #[serde(default = "dot")]
    pub project_dir: PathBuf,
    #[serde(default = "default_profile")]
    pub profile: String,
    pub state_dir: Option<PathBuf>,
    #[serde(default = "default_threads")]
    pub threads: u16,
    #[serde(default = "default_dbt_command")]
    pub command: String,
}

impl Default for DbtConfig {
    fn default() -> Self {
        Self {
            project_dir: dot(),
            profile: default_profile(),
            state_dir: None,
            threads: default_threads(),
            command: default_dbt_command(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SafetyConfig {
    #[serde(default = "default_schema_prefix")]
    pub schema_prefix: String,
    #[serde(default = "default_timeout")]
    pub statement_timeout_seconds: u64,
    #[serde(default = "default_max_models")]
    pub max_models: usize,
    #[serde(default = "default_max_columns")]
    pub max_columns_per_model: usize,
    #[serde(default = "default_pk_limit")]
    pub primary_key_sample_limit: usize,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComparisonConfig {
    #[serde(default)]
    pub mode: ComparisonMode,
    #[serde(default = "default_comparison_concurrency")]
    pub concurrency: usize,
    #[serde(default = "default_comparison_timeout")]
    pub timeout_seconds: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValidationConfig {
    #[serde(default)]
    pub downstream: DownstreamPolicy,
    #[serde(default = "default_critical_tags")]
    pub critical_tags: Vec<String>,
    #[serde(default)]
    pub incremental_mode: IncrementalMode,
}

impl Default for ValidationConfig {
    fn default() -> Self {
        Self {
            downstream: DownstreamPolicy::default(),
            critical_tags: default_critical_tags(),
            incremental_mode: IncrementalMode::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DownstreamPolicy {
    None,
    #[default]
    Critical,
    All,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IncrementalMode {
    #[default]
    Clone,
    FullRefresh,
}

impl Default for ComparisonConfig {
    fn default() -> Self {
        Self {
            mode: ComparisonMode::default(),
            concurrency: default_comparison_concurrency(),
            timeout_seconds: default_comparison_timeout(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ComparisonMode {
    Quick,
    #[default]
    Deep,
}

impl Default for SafetyConfig {
    fn default() -> Self {
        Self {
            schema_prefix: default_schema_prefix(),
            statement_timeout_seconds: default_timeout(),
            max_models: default_max_models(),
            max_columns_per_model: default_max_columns(),
            primary_key_sample_limit: default_pk_limit(),
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Thresholds {
    #[serde(default = "default_row_threshold")]
    pub row_count_relative: f64,
    #[serde(default = "default_rate_threshold")]
    pub null_rate_absolute: f64,
    #[serde(default = "default_rate_threshold")]
    pub cardinality_relative: f64,
    #[serde(default = "default_rate_threshold")]
    pub numeric_relative: f64,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            row_count_relative: default_row_threshold(),
            null_rate_absolute: default_rate_threshold(),
            cardinality_relative: default_rate_threshold(),
            numeric_relative: default_rate_threshold(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountConfig {
    pub name: String,
    pub account: String,
    pub user: String,
    pub role: String,
    pub database: String,
    pub warehouse: String,
    pub production_schema: String,
    pub selector: Option<String>,
    pub auth: AuthConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum AuthConfig {
    /// Interactive browser login through Snowflake's built-in local application.
    OauthLocal,
    Oauth {
        #[serde(default = "default_oauth_env")]
        token_env: String,
    },
    ProgrammaticAccessToken {
        #[serde(default = "default_pat_env")]
        token_env: String,
    },
    KeyPair {
        private_key_path: PathBuf,
        passphrase_env: Option<String>,
    },
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelConfig {
    #[serde(default)]
    pub primary_key: Vec<String>,
    #[serde(default)]
    pub allow_removal: bool,
    #[serde(default)]
    pub critical: bool,
    #[serde(default)]
    pub key_policy: KeyPolicy,
    #[serde(default)]
    pub thresholds: ThresholdOverrides,
    /// Optional SQL predicate applied to both CI and production comparisons.
    #[serde(default, rename = "where")]
    pub where_clause: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum KeyPolicy {
    #[default]
    Regression,
    Strict,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThresholdOverrides {
    pub row_count_relative: Option<f64>,
    pub null_rate_absolute: Option<f64>,
    pub cardinality_relative: Option<f64>,
    pub numeric_relative: Option<f64>,
}

impl ThresholdOverrides {
    pub fn apply(&self, defaults: Thresholds) -> Thresholds {
        Thresholds {
            row_count_relative: self
                .row_count_relative
                .unwrap_or(defaults.row_count_relative),
            null_rate_absolute: self
                .null_rate_absolute
                .unwrap_or(defaults.null_rate_absolute),
            cardinality_relative: self
                .cardinality_relative
                .unwrap_or(defaults.cardinality_relative),
            numeric_relative: self.numeric_relative.unwrap_or(defaults.numeric_relative),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalChange {
    pub path: String,
    pub models: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct CrossAccountDependency {
    pub from: String,
    pub to: String,
    #[serde(default)]
    pub columns: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetabaseConfig {
    pub url: String,
    #[serde(default = "default_metabase_env")]
    pub api_key_env: String,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let bytes =
            fs::read(path).with_context(|| format!("could not read config {}", path.display()))?;
        let config: Self = serde_yaml::from_slice(&bytes)
            .with_context(|| format!("invalid config {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        if self.version != 1 {
            bail!("unsupported config version {}; expected 1", self.version);
        }
        if self.accounts.is_empty() {
            bail!("at least one Snowflake account is required");
        }
        if self.dbt.profile.trim().is_empty() || self.dbt.command.trim().is_empty() {
            bail!("dbt profile and command must not be empty");
        }
        if self.dbt.threads == 0 {
            bail!("dbt.threads must be greater than zero");
        }
        if self.accounts.len() > 1
            && self
                .accounts
                .iter()
                .any(|account| account.selector.is_none())
        {
            bail!("every account needs a dbt selector when multiple accounts are configured");
        }
        if self.safety.max_models == 0 || self.safety.max_columns_per_model == 0 {
            bail!("safety model and column limits must be greater than zero");
        }
        if self.safety.statement_timeout_seconds == 0 {
            bail!("statement timeout must be greater than zero");
        }
        if self.safety.statement_timeout_seconds > 604_800 {
            bail!("statement timeout must not exceed Snowflake's 604800-second maximum");
        }
        if self.comparison.concurrency == 0 || self.comparison.concurrency > 32 {
            bail!("comparison.concurrency must be between 1 and 32");
        }
        if self.comparison.timeout_seconds == 0 || self.comparison.timeout_seconds > 604_800 {
            bail!("comparison.timeout_seconds must be between 1 and 604800");
        }
        if self
            .validation
            .critical_tags
            .iter()
            .any(|tag| tag.trim().is_empty())
        {
            bail!("validation.critical_tags must not contain empty tags");
        }
        if self.safety.schema_prefix.is_empty()
            || self.safety.schema_prefix.len() > 200
            || !self
                .safety
                .schema_prefix
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            bail!("schema_prefix must be 1-200 letters, numbers, or underscores");
        }
        let valid_rate = |value: f64| value.is_finite() && value >= 0.0;
        if !valid_rate(self.thresholds.row_count_relative)
            || !valid_rate(self.thresholds.null_rate_absolute)
            || !valid_rate(self.thresholds.cardinality_relative)
            || !valid_rate(self.thresholds.numeric_relative)
        {
            bail!("thresholds must be finite non-negative numbers");
        }
        let mut account_names = BTreeSet::new();
        for account in &self.accounts {
            if account.name.len() > 64
                || !account.name.chars().all(|character| {
                    character.is_ascii_alphanumeric() || matches!(character, '-' | '_')
                })
            {
                bail!(
                    "account name {} must be 1-64 letters, numbers, hyphens, or underscores",
                    account.name
                );
            }
            if !account_names.insert(account.name.to_ascii_lowercase()) {
                bail!("Snowflake account names must be unique");
            }
            for (field, value) in [
                ("name", &account.name),
                ("account", &account.account),
                ("user", &account.user),
                ("role", &account.role),
                ("database", &account.database),
                ("warehouse", &account.warehouse),
                ("production_schema", &account.production_schema),
            ] {
                if value.trim().is_empty() {
                    bail!("account {} has an empty {field}", account.name);
                }
            }
            if account.account.len() > 255
                || account.account.starts_with(['.', '-'])
                || account.account.ends_with(['.', '-'])
                || account
                    .account
                    .to_ascii_lowercase()
                    .ends_with(".snowflakecomputing.com")
                || !account.account.chars().all(|character| {
                    character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
                })
                || account.account.contains("..")
            {
                bail!(
                    "account identifier {} is invalid; use the identifier only, without https:// or .snowflakecomputing.com",
                    account.account
                );
            }
            if account
                .selector
                .as_ref()
                .is_some_and(|selector| selector.trim().is_empty())
            {
                bail!("account {} has an empty dbt selector", account.name);
            }
            match &account.auth {
                AuthConfig::OauthLocal => {}
                AuthConfig::Oauth { token_env }
                | AuthConfig::ProgrammaticAccessToken { token_env } => {
                    if token_env.trim().is_empty() {
                        bail!("account {} has an empty token_env", account.name);
                    }
                }
                AuthConfig::KeyPair {
                    private_key_path,
                    passphrase_env,
                } => {
                    if private_key_path.as_os_str().is_empty() {
                        bail!("account {} has an empty private_key_path", account.name);
                    }
                    if passphrase_env
                        .as_ref()
                        .is_some_and(|value| value.trim().is_empty())
                    {
                        bail!("account {} has an empty passphrase_env", account.name);
                    }
                }
            }
        }
        if let Some(metabase) = &self.metabase {
            let url = url::Url::parse(&metabase.url).context("metabase.url is invalid")?;
            if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
                bail!("metabase.url must be an absolute http or https URL");
            }
            if metabase.api_key_env.trim().is_empty() {
                bail!("metabase.api_key_env must not be empty");
            }
        }
        for (model, config) in &self.models {
            if let Some(predicate) = &config.where_clause
                && (predicate.trim().is_empty()
                    || predicate.contains(';')
                    || predicate.contains("--")
                    || predicate.contains("/*")
                    || predicate.contains("*/"))
            {
                bail!(
                    "models.{model}.where must be one non-empty SQL predicate without comments or semicolons"
                );
            }
            let effective = config.thresholds.apply(self.thresholds);
            if !valid_rate(effective.row_count_relative)
                || !valid_rate(effective.null_rate_absolute)
                || !valid_rate(effective.cardinality_relative)
                || !valid_rate(effective.numeric_relative)
            {
                bail!("models.{model}.thresholds must be finite non-negative numbers");
            }
        }
        Ok(())
    }

    pub fn resolve_from(&mut self, config_path: &Path) -> Result<()> {
        let root = config_path.parent().unwrap_or_else(|| Path::new("."));
        self.dbt.project_dir = absolute_from(root, &self.dbt.project_dir)?;
        if let Some(path) = &self.dbt.state_dir {
            self.dbt.state_dir = Some(absolute_from(root, path)?);
        }
        for account in &mut self.accounts {
            if let AuthConfig::KeyPair {
                private_key_path, ..
            } = &mut account.auth
            {
                let expanded = expand_home(private_key_path)?;
                *private_key_path = if expanded.is_absolute() {
                    expanded
                } else {
                    root.join(expanded)
                };
            }
        }
        Ok(())
    }
}

fn absolute_from(root: &Path, path: &Path) -> Result<PathBuf> {
    let joined = if path.is_absolute() {
        path.to_owned()
    } else {
        root.join(path)
    };
    joined
        .canonicalize()
        .with_context(|| format!("path does not exist: {}", joined.display()))
}

fn expand_home(path: &Path) -> Result<PathBuf> {
    let Some(value) = path.to_str() else {
        return Ok(path.to_owned());
    };
    if value == "~" || value.starts_with("~/") {
        let home = env::var_os("HOME").context("HOME is unavailable for key path expansion")?;
        let suffix = value.strip_prefix("~/").unwrap_or("");
        return Ok(PathBuf::from(home).join(suffix));
    }
    Ok(path.to_owned())
}

fn dot() -> PathBuf {
    PathBuf::from(".")
}
fn default_profile() -> String {
    "analytics".into()
}
fn default_threads() -> u16 {
    4
}
fn default_dbt_command() -> String {
    "dbt".into()
}
fn default_schema_prefix() -> String {
    "EMBRASURE_CHECK".into()
}
fn default_timeout() -> u64 {
    300
}
fn default_max_models() -> usize {
    25
}
fn default_max_columns() -> usize {
    100
}
fn default_pk_limit() -> usize {
    20
}
fn default_comparison_concurrency() -> usize {
    4
}
fn default_comparison_timeout() -> u64 {
    900
}
fn default_critical_tags() -> Vec<String> {
    vec!["critical".into()]
}
fn default_row_threshold() -> f64 {
    0.001
}
fn default_rate_threshold() -> f64 {
    0.001
}
fn default_oauth_env() -> String {
    "SNOWFLAKE_OAUTH_TOKEN".into()
}
fn default_pat_env() -> String {
    "SNOWFLAKE_PROGRAMMATIC_ACCESS_TOKEN".into()
}
fn default_metabase_env() -> String {
    "METABASE_API_KEY".into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_two_accounts_without_selectors() {
        let yaml = r#"
version: 1
accounts:
  - name: one
    account: org-one
    user: ci
    role: ci
    database: analytics
    warehouse: ci
    production_schema: prod
    auth: { type: oauth, token_env: TOKEN }
  - name: two
    account: org-two
    user: ci
    role: ci
    database: analytics
    warehouse: ci
    production_schema: prod
    auth: { type: oauth, token_env: TOKEN }
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("selector")
        );
    }

    #[test]
    fn example_config_stays_valid() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("embrasure-check.example.yml");
        Config::load(&path).unwrap();
    }

    #[test]
    fn enterprise_auth_methods_and_two_accounts_are_valid() {
        let yaml = r#"
version: 1
accounts:
  - name: developer
    account: org-one
    user: analyst
    role: dbt_ci
    database: analytics
    warehouse: dbt_ci
    production_schema: prod
    selector: tag:one
    auth: { type: oauth_local }
  - name: ci
    account: org-two
    user: service
    role: dbt_ci
    database: analytics
    warehouse: dbt_ci
    production_schema: prod
    selector: tag:two
    auth: { type: programmatic_access_token, token_env: SECOND_ACCOUNT_PAT }
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        config.validate().unwrap();
    }

    #[test]
    fn rejects_account_names_that_can_escape_temporary_paths() {
        let yaml = r#"
version: 1
accounts:
  - name: ../../outside
    account: org-account
    user: ci
    role: ci
    database: analytics
    warehouse: ci
    production_schema: prod
    auth: { type: oauth, token_env: TOKEN }
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("account name")
        );
    }

    #[test]
    fn key_paths_are_relative_to_the_config_file() {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("config/embrasure-check.yml");
        fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        fs::create_dir_all(directory.path().join("project")).unwrap();
        fs::write(
            &config_path,
            r#"
version: 1
dbt: { project_dir: ../project }
accounts:
  - name: one
    account: org-account
    user: ci
    role: ci
    database: analytics
    warehouse: ci
    production_schema: prod
    auth: { type: key_pair, private_key_path: secrets/key.p8 }
"#,
        )
        .unwrap();
        let mut config = Config::load(&config_path).unwrap();
        config.resolve_from(&config_path).unwrap();
        let AuthConfig::KeyPair {
            private_key_path, ..
        } = &config.accounts[0].auth
        else {
            panic!("expected key-pair auth");
        };
        assert_eq!(
            private_key_path,
            &directory.path().join("config/secrets/key.p8")
        );
    }

    #[test]
    fn comparison_limits_and_model_filters_are_validated() {
        let yaml = r#"
version: 1
comparison:
  mode: quick
  concurrency: 4
  timeout_seconds: 600
accounts:
  - name: primary
    account: org-account
    user: ci
    role: ci
    database: analytics
    warehouse: ci
    production_schema: prod
    auth: { type: oauth_local }
models:
  model.analytics.orders:
    where: "order_date >= current_date - 30"
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        config.validate().unwrap();
        assert_eq!(config.comparison.mode, ComparisonMode::Quick);

        let invalid = yaml.replace("current_date - 30", "current_date; DROP SCHEMA PROD");
        let config: Config = serde_yaml::from_str(&invalid).unwrap();
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("predicate")
        );
    }
}
