use std::{
    collections::BTreeMap,
    env, fs,
    io::Write,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::net::TcpListener;
use url::Url;

use crate::{
    config::{AccountConfig, AuthConfig, Config},
    loopback, update,
};

const LOCAL_CLIENT_ID: &str = "LOCAL_APPLICATION";

#[derive(Debug, Clone)]
pub enum ResolvedAuth {
    OAuth {
        token: String,
    },
    KeyPair {
        private_key_path: PathBuf,
        passphrase: Option<String>,
    },
    ProgrammaticAccessToken {
        token: String,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct AuthStatus {
    pub account: String,
    pub method: &'static str,
    pub ready: bool,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedToken {
    account: String,
    user: String,
    access_token: String,
    refresh_token: Option<String>,
    expires_at: u64,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    #[serde(default = "default_expires_in")]
    expires_in: u64,
}

pub async fn resolve_all(config: &Config) -> Result<BTreeMap<String, ResolvedAuth>> {
    let mut result = BTreeMap::new();
    for account in &config.accounts {
        result.insert(account.name.clone(), resolve(account).await?);
    }
    Ok(result)
}

pub async fn resolve(account: &AccountConfig) -> Result<ResolvedAuth> {
    match &account.auth {
        AuthConfig::OauthLocal => {
            let token = load_or_refresh_local_token(account).await.with_context(|| {
                format!(
                    "browser login is not ready for {}; run `embrasure auth login --account {}`",
                    account.name, account.name
                )
            })?;
            Ok(ResolvedAuth::OAuth { token })
        }
        AuthConfig::Oauth { token_env } => Ok(ResolvedAuth::OAuth {
            token: required_env(token_env, "OAuth token")?,
        }),
        AuthConfig::ProgrammaticAccessToken { token_env } => {
            Ok(ResolvedAuth::ProgrammaticAccessToken {
                token: required_env(token_env, "Snowflake PAT")?,
            })
        }
        AuthConfig::KeyPair {
            private_key_path,
            passphrase_env,
        } => Ok(ResolvedAuth::KeyPair {
            private_key_path: private_key_path.clone(),
            passphrase: passphrase_env
                .as_ref()
                .map(|name| required_env(name, "key passphrase"))
                .transpose()?,
        }),
    }
}

pub async fn login_from_config(config_path: &Path, requested: Option<&str>) -> Result<String> {
    let mut config = Config::load(config_path)?;
    config.resolve_from(config_path)?;
    let account = select_account(&config, requested)?;
    if !matches!(account.auth, AuthConfig::OauthLocal) {
        bail!(
            "account {} uses {}; browser login requires `auth: {{ type: oauth_local }}`",
            account.name,
            method_name(&account.auth)
        );
    }
    login(account).await?;
    Ok(account.name.clone())
}

pub fn logout_from_config(config_path: &Path, requested: Option<&str>) -> Result<String> {
    let mut config = Config::load(config_path)?;
    config.resolve_from(config_path)?;
    let account = select_account(&config, requested)?;
    if !matches!(account.auth, AuthConfig::OauthLocal) {
        bail!("account {} does not use browser login", account.name);
    }
    let path = token_path(account)?;
    if path.exists() {
        fs::remove_file(&path)
            .with_context(|| format!("could not remove cached token {}", path.display()))?;
    }
    Ok(account.name.clone())
}

pub fn status_from_config(config_path: &Path) -> Result<Vec<AuthStatus>> {
    let mut config = Config::load(config_path)?;
    config.resolve_from(config_path)?;
    config.accounts.iter().map(status).collect()
}

pub fn status(account: &AccountConfig) -> Result<AuthStatus> {
    let (method, ready, message) = match &account.auth {
        AuthConfig::OauthLocal => match load_cached(account) {
            Ok(token) if token.expires_at > now()? => ("oauth_local", true, "signed in".into()),
            Ok(token) if token.refresh_token.is_some() => (
                "oauth_local",
                true,
                "signed in; access token will refresh".into(),
            ),
            Ok(_) => (
                "oauth_local",
                false,
                "session expired; run auth login".into(),
            ),
            Err(_) => ("oauth_local", false, "not signed in; run auth login".into()),
        },
        AuthConfig::Oauth { token_env } => env_status("oauth", token_env),
        AuthConfig::ProgrammaticAccessToken { token_env } => {
            env_status("programmatic_access_token", token_env)
        }
        AuthConfig::KeyPair {
            private_key_path,
            passphrase_env,
        } => {
            let passphrase_ready = passphrase_env
                .as_ref()
                .is_none_or(|name| env::var(name).is_ok_and(|value| !value.trim().is_empty()));
            let ready = private_key_path.is_file() && passphrase_ready;
            let message = if !private_key_path.is_file() {
                format!("key file is missing: {}", private_key_path.display())
            } else if !passphrase_ready {
                format!(
                    "passphrase environment variable {} is missing",
                    passphrase_env.as_deref().unwrap_or_default()
                )
            } else {
                "key and optional passphrase are available".into()
            };
            ("key_pair", ready, message)
        }
    };
    Ok(AuthStatus {
        account: account.name.clone(),
        method,
        ready,
        status: message,
    })
}

fn env_status(method: &'static str, name: &str) -> (&'static str, bool, String) {
    match env::var(name) {
        Ok(value) if !value.trim().is_empty() => (method, true, format!("{name} is set")),
        Ok(_) => (method, false, format!("{name} is empty")),
        Err(_) => (method, false, format!("{name} is missing")),
    }
}

fn required_env(name: &str, label: &str) -> Result<String> {
    let value =
        env::var(name).with_context(|| format!("missing {label} environment variable {name}"))?;
    if value.trim().is_empty() {
        bail!("{label} environment variable {name} is empty");
    }
    Ok(value)
}

fn select_account<'a>(config: &'a Config, requested: Option<&str>) -> Result<&'a AccountConfig> {
    if let Some(name) = requested {
        return config
            .accounts
            .iter()
            .find(|account| account.name.eq_ignore_ascii_case(name))
            .with_context(|| format!("no configured account is named {name}"));
    }
    if config.accounts.len() != 1 {
        bail!("--account is required when multiple accounts are configured");
    }
    Ok(&config.accounts[0])
}

async fn login(account: &AccountConfig) -> Result<String> {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .context("could not start the local OAuth callback listener")?;
    let port = listener.local_addr()?.port();
    // Snowflake's built-in local application accepts dynamic loopback ports.
    // Keep the path at `/` for compatibility with accounts whose integration
    // was provisioned before optional callback paths rolled out.
    let redirect_uri = format!("http://127.0.0.1:{port}");
    let (verifier, challenge) = loopback::pkce_pair();
    let state = loopback::random_string(32);
    let scope = format!("refresh_token session:role:{}", account.role);
    let mut authorize = Url::parse(&format!(
        "https://{}/oauth/authorize",
        account_host(&account.account)
    ))?;
    authorize
        .query_pairs_mut()
        .append_pair("client_id", LOCAL_CLIENT_ID)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("state", &state)
        .append_pair("scope", &scope)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256");

    eprintln!("embrasure: opening Snowflake sign-in in your browser");
    if webbrowser::open(authorize.as_str()).is_err() {
        eprintln!("Open this URL to sign in:\n{authorize}");
    }
    let (mut stream, request) = loopback::accept(
        &listener,
        Duration::from_secs(300),
        "Snowflake browser login timed out after 5 minutes",
        "Snowflake OAuth callback did not finish sending its request",
    )
    .await?;
    let request = String::from_utf8_lossy(&request);
    let target = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .context("the OAuth callback request was malformed")?;
    let callback = Url::parse(&format!("http://127.0.0.1{target}"))?;
    let params = callback.query_pairs().collect::<BTreeMap<_, _>>();
    let valid_state = params.get("state").is_some_and(|value| value == &state);
    let outcome = if !valid_state {
        Err(anyhow::anyhow!("Snowflake OAuth state did not match"))
    } else if let Some(error) = params.get("error") {
        Err(anyhow::anyhow!("Snowflake denied login: {error}"))
    } else {
        params
            .get("code")
            .map(|value| value.to_string())
            .context("Snowflake callback omitted an authorization code")
    };
    let (status, message) = if outcome.is_ok() {
        (
            "200 OK",
            "Authorization received. Return to the terminal to finish sign-in.",
        )
    } else {
        (
            "400 Bad Request",
            "Snowflake sign-in failed. Return to the terminal for details.",
        )
    };
    loopback::respond(&mut stream, status, message, "").await?;
    let code = outcome?;
    let token = token_request(
        account,
        &[
            ("grant_type", "authorization_code"),
            ("client_id", LOCAL_CLIENT_ID),
            ("code", &code),
            ("redirect_uri", &redirect_uri),
            ("code_verifier", &verifier),
        ],
    )
    .await?;
    save_cached(account, &token)?;
    Ok(token.access_token)
}

async fn load_or_refresh_local_token(account: &AccountConfig) -> Result<String> {
    let cached = load_cached(account)?;
    if cached.expires_at > now()? + 30 {
        return Ok(cached.access_token);
    }
    let refresh = cached
        .refresh_token
        .as_deref()
        .context("cached Snowflake access token expired without a refresh token")?;
    let token = token_request(
        account,
        &[
            ("grant_type", "refresh_token"),
            ("client_id", LOCAL_CLIENT_ID),
            ("refresh_token", refresh),
        ],
    )
    .await?;
    let token = TokenResponse {
        refresh_token: token.refresh_token.or(cached.refresh_token),
        ..token
    };
    save_cached(account, &token)?;
    Ok(token.access_token)
}

async fn token_request(account: &AccountConfig, form: &[(&str, &str)]) -> Result<TokenResponse> {
    let response = update::http_client()?
        .post(format!(
            "https://{}/oauth/token-request",
            account_host(&account.account)
        ))
        .form(form)
        .send()
        .await
        .context("Snowflake OAuth token request failed")?;
    let status = response.status();
    let bytes = response.bytes().await?;
    if !status.is_success() {
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_default();
        let message = value
            .get("error_description")
            .or_else(|| value.get("error"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown OAuth error");
        bail!("Snowflake OAuth token exchange failed (HTTP {status}): {message}");
    }
    serde_json::from_slice(&bytes).context("Snowflake OAuth token response was invalid")
}

fn load_cached(account: &AccountConfig) -> Result<CachedToken> {
    let path = token_path(account)?;
    let bytes = fs::read(&path)
        .with_context(|| format!("no cached Snowflake session at {}", path.display()))?;
    let token: CachedToken = serde_json::from_slice(&bytes)
        .with_context(|| format!("invalid cached Snowflake session at {}", path.display()))?;
    if token.account != account.account || !token.user.eq_ignore_ascii_case(&account.user) {
        bail!("cached Snowflake session belongs to a different account or user");
    }
    Ok(token)
}

fn save_cached(account: &AccountConfig, response: &TokenResponse) -> Result<()> {
    let path = token_path(account)?;
    let parent = path.parent().context("OAuth cache path has no parent")?;
    fs::create_dir_all(parent)?;
    restrict_directory(parent)?;
    let token = CachedToken {
        account: account.account.clone(),
        user: account.user.clone(),
        access_token: response.access_token.clone(),
        refresh_token: response.refresh_token.clone(),
        expires_at: now()?.saturating_add(response.expires_in.saturating_sub(30)),
    };
    let bytes = serde_json::to_vec_pretty(&token)?;
    write_secret_file(&path, &bytes)
}

fn token_path(account: &AccountConfig) -> Result<PathBuf> {
    let root = if let Some(value) = env::var_os("EMBRASURE_CHECK_CONFIG_DIR") {
        PathBuf::from(value)
    } else if let Some(value) = env::var_os("XDG_CONFIG_HOME") {
        PathBuf::from(value).join("embrasure-check")
    } else {
        PathBuf::from(env::var_os("HOME").context("HOME is unavailable")?)
            .join(".config/embrasure-check")
    };
    let identity = format!("{}:{}", account.account, account.user.to_ascii_uppercase());
    let digest = Sha256::digest(identity.as_bytes());
    let name = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Ok(root.join("oauth").join(format!("{name}.json")))
}

#[cfg(unix)]
fn write_secret_file(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    file.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn write_secret_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)?;
    file.write_all(bytes)?;
    Ok(())
}

#[cfg(unix)]
fn restrict_directory(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn restrict_directory(_path: &Path) -> Result<()> {
    Ok(())
}

fn method_name(auth: &AuthConfig) -> &'static str {
    match auth {
        AuthConfig::OauthLocal => "oauth_local",
        AuthConfig::Oauth { .. } => "oauth",
        AuthConfig::ProgrammaticAccessToken { .. } => "programmatic_access_token",
        AuthConfig::KeyPair { .. } => "key_pair",
    }
}

pub fn account_host(account: &str) -> String {
    format!(
        "{}.snowflakecomputing.com",
        account.trim().to_ascii_lowercase().replace('_', "-")
    )
}

fn now() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}

fn default_expires_in() -> u64 {
    600
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_identifiers_become_hosts() {
        assert_eq!(
            account_host("MY_ORG-MY_ACCOUNT"),
            "my-org-my-account.snowflakecomputing.com"
        );
    }

    #[test]
    fn pkce_values_are_url_safe_and_long_enough() {
        let (verifier, challenge) = loopback::pkce_pair();
        assert_eq!(verifier.len(), 64);
        assert!(challenge.len() >= 43);
        assert!(!challenge.contains('='));
    }
}
