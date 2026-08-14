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
    Oauth {
        #[serde(default = "default_oauth_env")]
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
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalChange {
    pub path: String,
    pub models: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
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
            if !account.account.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
            }) || account.account.contains("..")
            {
                bail!(
                    "account identifier {} contains invalid characters",
                    account.account
                );
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
                *private_key_path = expand_home(private_key_path)?;
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
fn default_row_threshold() -> f64 {
    0.001
}
fn default_rate_threshold() -> f64 {
    0.001
}
fn default_oauth_env() -> String {
    "SNOWFLAKE_OAUTH_TOKEN".into()
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
}
