use std::{
    env, fs,
    io::{self, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_yaml::Value;

#[derive(Debug, Default)]
pub struct Options {
    pub profile: Option<String>,
    pub account: Option<String>,
    pub user: Option<String>,
    pub role: Option<String>,
    pub database: Option<String>,
    pub warehouse: Option<String>,
    pub production_schema: Option<String>,
}

#[derive(Debug, Default)]
struct Defaults {
    profile: Option<String>,
    account: Option<String>,
    user: Option<String>,
    role: Option<String>,
    database: Option<String>,
    warehouse: Option<String>,
}

#[derive(Serialize)]
struct GeneratedConfig<'a> {
    version: u8,
    dbt: GeneratedDbt<'a>,
    accounts: Vec<GeneratedAccount<'a>>,
}

#[derive(Serialize)]
struct GeneratedDbt<'a> {
    project_dir: &'static str,
    profile: &'a str,
}

#[derive(Serialize)]
struct GeneratedAccount<'a> {
    name: &'static str,
    account: &'a str,
    user: &'a str,
    role: &'a str,
    database: &'a str,
    warehouse: &'a str,
    production_schema: &'a str,
    auth: GeneratedAuth,
}

#[derive(Serialize)]
struct GeneratedAuth {
    r#type: &'static str,
}

pub fn run(config_path: &Path, force: bool, options: Options) -> Result<()> {
    if config_path.exists() && !force {
        bail!(
            "{} already exists; use --force to replace it",
            config_path.display()
        );
    }

    let project_file = Path::new("dbt_project.yml");
    if !project_file.is_file() {
        bail!("dbt_project.yml was not found; run `embrasure init` from your dbt project root");
    }

    let defaults = discover_defaults(project_file);
    println!("Set up Embrasure for this dbt project.\n");

    let profile = detected_or_required("dbt profile", options.profile.or(defaults.profile))?;
    let account = detected_or_required(
        "Snowflake account identifier",
        options.account.or(defaults.account),
    )?;
    let user = detected_or_required("Snowflake user", options.user.or(defaults.user))?;
    let role = detected_or_required("Snowflake role", options.role.or(defaults.role))?;
    let database = detected_or_required(
        "Snowflake validation database",
        options.database.or(defaults.database),
    )?;
    let warehouse = detected_or_required(
        "Snowflake warehouse",
        options.warehouse.or(defaults.warehouse),
    )?;
    let production_schema = match options.production_schema {
        Some(value) if !value.trim().is_empty() => value,
        _ => required("Production schema", Some("PROD".to_owned()))?,
    };

    let generated = GeneratedConfig {
        version: 1,
        dbt: GeneratedDbt {
            project_dir: ".",
            profile: &profile,
        },
        accounts: vec![GeneratedAccount {
            name: "primary",
            account: &account,
            user: &user,
            role: &role,
            database: &database,
            warehouse: &warehouse,
            production_schema: &production_schema,
            auth: GeneratedAuth {
                r#type: "oauth_local",
            },
        }],
    };

    let yaml = serde_yaml::to_string(&generated).context("could not generate configuration")?;
    fs::write(config_path, yaml)
        .with_context(|| format!("could not write {}", config_path.display()))?;

    println!(
        "\nCreated {}.\n\nNext:\n  embrasure auth login\n  embrasure doctor\n  embrasure check",
        config_path.display()
    );
    Ok(())
}

fn detected_or_required(label: &str, value: Option<String>) -> Result<String> {
    match value {
        Some(value) if !value.trim().is_empty() => Ok(value),
        _ => required(label, None),
    }
}

fn required(label: &str, default: Option<String>) -> Result<String> {
    let default = default.filter(|value| !value.trim().is_empty());
    match &default {
        Some(value) => print!("{label} [{value}]: "),
        None => print!("{label}: "),
    }
    io::stdout().flush().context("could not write prompt")?;

    let mut input = String::new();
    let bytes = io::stdin()
        .read_line(&mut input)
        .context("could not read setup response")?;
    let value = input.trim();
    if !value.is_empty() {
        return Ok(value.to_owned());
    }
    if let Some(value) = default {
        return Ok(value);
    }
    if bytes == 0 {
        bail!("{label} is required; rerun `embrasure init` in an interactive terminal");
    }
    bail!("{label} is required");
}

fn discover_defaults(project_file: &Path) -> Defaults {
    let profile = fs::read(project_file)
        .ok()
        .and_then(|bytes| serde_yaml::from_slice::<Value>(&bytes).ok())
        .and_then(|value| mapping_string(&value, "profile"));

    let mut defaults = Defaults {
        profile,
        ..Defaults::default()
    };
    let Some(profile_name) = defaults.profile.as_deref() else {
        return defaults;
    };
    let Some(profiles_file) = profiles_file() else {
        return defaults;
    };
    let Some(profiles) = fs::read(profiles_file)
        .ok()
        .and_then(|bytes| serde_yaml::from_slice::<Value>(&bytes).ok())
    else {
        return defaults;
    };
    let Some(profile) = mapping_value(&profiles, profile_name) else {
        return defaults;
    };
    let target_name = env::var("DBT_TARGET")
        .ok()
        .or_else(|| mapping_string(profile, "target"));
    let Some(output) = mapping_value(profile, "outputs").and_then(|outputs| {
        target_name
            .as_deref()
            .and_then(|name| mapping_value(outputs, name))
    }) else {
        return defaults;
    };

    defaults.account = mapping_string(output, "account");
    defaults.user = mapping_string(output, "user");
    defaults.role = mapping_string(output, "role");
    defaults.database = mapping_string(output, "database");
    defaults.warehouse = mapping_string(output, "warehouse");
    defaults
}

fn profiles_file() -> Option<PathBuf> {
    if let Some(directory) = env::var_os("DBT_PROFILES_DIR") {
        return Some(PathBuf::from(directory).join("profiles.yml"));
    }
    env::var_os("HOME").map(|home| PathBuf::from(home).join(".dbt/profiles.yml"))
}

fn mapping_value<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    value.as_mapping()?.get(Value::String(key.to_owned()))
}

fn mapping_string(value: &Value, key: &str) -> Option<String> {
    let raw = mapping_value(value, key)?.as_str()?.trim();
    resolve_env_var(raw).or_else(|| {
        if raw.contains("{{") {
            None
        } else {
            Some(raw.to_owned())
        }
    })
}

fn resolve_env_var(value: &str) -> Option<String> {
    let expression = value.strip_prefix("{{")?.strip_suffix("}}")?.trim();
    let arguments = expression
        .strip_prefix("env_var(")?
        .strip_suffix(')')?
        .trim();
    let quote = arguments.chars().next()?;
    if !matches!(quote, '\'' | '"') {
        return None;
    }
    let rest = &arguments[quote.len_utf8()..];
    let end = rest.find(quote)?;
    env::var(&rest[..end]).ok()
}

#[cfg(test)]
mod tests {
    use super::resolve_env_var;

    #[test]
    fn ignores_non_env_var_templates() {
        assert_eq!(resolve_env_var("{{ target.name }}"), None);
    }
}
