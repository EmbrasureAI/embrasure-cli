use std::{
    env, fs,
    io::Write,
    path::{Component, Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use directories::ProjectDirs;
use globset::Glob;
use keyring::Entry;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::net::TcpListener;
use url::Url;
use uuid::Uuid;

use crate::{
    config::{ComparisonMode, Config, DownstreamPolicy, IncrementalMode},
    git, loopback,
    report::Report,
    run::CheckOptions,
    style, update,
};

const KEYCHAIN_SERVICE: &str = "ai.embrasure.cli.cloud";
const KEYCHAIN_ACCOUNT: &str = "session";
const MAX_FILES: usize = 100;
const MAX_FILE_BYTES: usize = 256 * 1024;
const MAX_TOTAL_BYTES: usize = 2 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudSession {
    access_token: String,
    refresh_token: String,
    expires_at: String,
    pub workspace_id: String,
    api_base_url: String,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    expires_at: String,
    workspace_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandoffReceipt {
    pub handoff_id: String,
    pub run_id: String,
    pub run_url: String,
    pub snapshot_hash: String,
    pub base_sha: String,
    pub accepted_at: String,
}

#[derive(Debug, Clone, Serialize)]
struct ChangedFile {
    path: String,
    status: String,
    sha256: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content_utf8: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct ValidationConfigSnapshot {
    sha256: String,
    content_utf8: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    repository_path: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct ValidationOptionsSnapshot {
    mode: Option<ComparisonMode>,
    downstream: Option<DownstreamPolicy>,
    critical_tags: Option<Vec<String>>,
    incremental_mode: Option<IncrementalMode>,
    select: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct PreparedSnapshot {
    dbt_root: String,
    owner: String,
    name: String,
    base_sha: String,
    head_sha: Option<String>,
    snapshot_hash: String,
    fingerprint: String,
    files: Vec<ChangedFile>,
    validation_config: ValidationConfigSnapshot,
    validation_options: ValidationOptionsSnapshot,
    total_bytes: usize,
}

#[derive(Debug, Serialize, Deserialize)]
struct ReviewCache {
    fingerprint: String,
    report: Report,
    saved_at: String,
}

#[derive(Debug, Deserialize)]
struct HandoffResponse {
    handoff_id: String,
    run_id: String,
    run_url: String,
    accepted_at: String,
    snapshot_hash: String,
}

pub struct Progress {
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl Progress {
    pub fn start(label: &str) -> Self {
        if !style::animation_enabled() {
            eprintln!("{label}...");
            return Self {
                stop: Arc::new(AtomicBool::new(true)),
                worker: None,
            };
        }
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let label = label.to_owned();
        let worker = thread::spawn(move || {
            let frames = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
            let mut index = 0;
            while !worker_stop.load(Ordering::Relaxed) {
                eprint!("\r{} {}", frames[index % frames.len()], label);
                let _ = std::io::stderr().flush();
                index += 1;
                thread::sleep(Duration::from_millis(80));
            }
            eprint!("\r\x1b[2K");
            let _ = std::io::stderr().flush();
        });
        Self {
            stop,
            worker: Some(worker),
        }
    }
}

impl Drop for Progress {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

pub fn normalize_context(values: &[String], file: Option<&Path>) -> Result<String> {
    let mut parts = values
        .iter()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if let Some(path) = file {
        let value = fs::read_to_string(path)
            .with_context(|| format!("could not read context file {}", path.display()))?;
        if !value.trim().is_empty() {
            parts.push(value.trim().to_owned());
        }
    }
    if parts.is_empty() {
        bail!("--cloud requires --context <business intent> or --context-file <path>");
    }
    let intent = parts.join("\n\n");
    if intent.len() > 20_000 {
        bail!("cloud validation context exceeds 20,000 characters");
    }
    if contains_secret(&intent) {
        bail!("cloud validation context appears to contain a secret; remove credentials and retry");
    }
    Ok(intent)
}

pub fn prepare_snapshot(
    config_path: &Path,
    config: &Config,
    base_ref: &str,
    options: &CheckOptions,
) -> Result<PreparedSnapshot> {
    let repository_root = git_path(
        config_path.parent().unwrap_or_else(|| Path::new(".")),
        &["rev-parse", "--show-toplevel"],
    )?;
    let dbt_dir = config.dbt.project_dir.canonicalize().with_context(|| {
        format!(
            "could not resolve dbt project directory {}",
            config.dbt.project_dir.display()
        )
    })?;
    let dbt_root = dbt_dir
        .strip_prefix(&repository_root)
        .context("the dbt project must be inside the Git repository")?
        .to_string_lossy()
        .replace('\\', "/")
        .trim_matches('/')
        .to_owned();
    let base_sha = git::text(&repository_root, &["rev-parse", "--verify", base_ref])?;
    let head_sha = git::text(&repository_root, &["rev-parse", "--verify", "HEAD"]).ok();
    let remote = git::text(&repository_root, &["remote", "get-url", "origin"])?;
    let (owner, name) = parse_github_remote(&remote)?;
    let mut candidates = changed_paths(&repository_root, base_ref)?;
    candidates.sort_by(|a, b| a.0.cmp(&b.0));
    candidates.dedup_by(|a, b| a.0 == b.0);
    let external_matchers = config
        .external_changes
        .iter()
        .map(|mapping| {
            Glob::new(&mapping.path)
                .with_context(|| format!("invalid external change glob {}", mapping.path))
                .map(|glob| glob.compile_matcher())
        })
        .collect::<Result<Vec<_>>>()?;

    let mut files = Vec::new();
    let mut total_bytes = 0;
    for (repo_path, status) in candidates {
        let relative = Path::new(&repo_path);
        let normalized = normalize_snapshot_path(relative)?;
        let project_path = if dbt_root.is_empty() {
            Some(relative)
        } else {
            relative.strip_prefix(&dbt_root).ok()
        };
        let eligible_project_file = project_path
            .map(normalize_snapshot_path)
            .transpose()?
            .is_some_and(|path| eligible(&path));
        let configured_external_file = external_matchers
            .iter()
            .any(|matcher| matcher.is_match(&normalized));
        if excluded(&normalized) || (!eligible_project_file && !configured_external_file) {
            continue;
        }
        let absolute = repository_root.join(relative);
        let (content_utf8, sha256) = if status == "deleted" {
            (None, hex_digest(&[]))
        } else {
            let metadata = fs::symlink_metadata(&absolute)
                .with_context(|| format!("could not inspect snapshot file {repo_path}"))?;
            if metadata.file_type().is_symlink() {
                bail!("snapshot file is a symlink and cannot be uploaded: {repo_path}");
            }
            if !metadata.is_file() {
                continue;
            }
            let bytes = fs::read(&absolute)
                .with_context(|| format!("could not read snapshot file {repo_path}"))?;
            if bytes.len() > MAX_FILE_BYTES {
                bail!("snapshot file exceeds 256 KB: {repo_path}");
            }
            total_bytes += bytes.len();
            if total_bytes > MAX_TOTAL_BYTES {
                bail!("snapshot exceeds the 2 MB upload limit");
            }
            let content = String::from_utf8(bytes.clone())
                .with_context(|| format!("snapshot file is not UTF-8 text: {repo_path}"))?;
            if contains_secret(&content) {
                bail!("possible secret found in snapshot file: {repo_path}");
            }
            (Some(content), hex_digest(&bytes))
        };
        files.push(ChangedFile {
            path: normalized,
            status,
            sha256,
            content_utf8,
        });
    }
    if files.is_empty() {
        bail!("no eligible dbt working-tree changes were found for cloud handoff");
    }
    if files.len() > MAX_FILES {
        bail!("snapshot contains more than 100 eligible files");
    }
    let mut manifest = Sha256::new();
    for file in &files {
        manifest.update(file.path.as_bytes());
        manifest.update([0]);
        manifest.update(file.status.as_bytes());
        manifest.update([0]);
        manifest.update(file.sha256.as_bytes());
        manifest.update(b"\n");
    }
    let snapshot_hash = encode_hex(&manifest.finalize());
    let canonical_config_path = config_path.canonicalize().with_context(|| {
        format!(
            "could not resolve validation config {}",
            config_path.display()
        )
    })?;
    let config_bytes = fs::read(&canonical_config_path)
        .with_context(|| format!("could not read validation config {}", config_path.display()))?;
    if config_bytes.len() > MAX_FILE_BYTES {
        bail!("validation config exceeds the 256 KB upload limit");
    }
    let config_content =
        String::from_utf8(config_bytes.clone()).context("validation config is not UTF-8 text")?;
    if contains_secret(&config_content) {
        bail!(
            "validation config appears to contain a secret; use environment-variable references instead"
        );
    }
    let validation_config = ValidationConfigSnapshot {
        sha256: hex_digest(&config_bytes),
        content_utf8: config_content,
        repository_path: canonical_config_path
            .strip_prefix(&repository_root)
            .ok()
            .map(normalize_snapshot_path)
            .transpose()?,
    };
    let validation_options = ValidationOptionsSnapshot {
        mode: options.mode,
        downstream: options.downstream,
        critical_tags: options.critical_tags.clone(),
        incremental_mode: options.incremental_mode,
        select: options.select.clone(),
    };
    let fingerprint = snapshot_fingerprint(
        &repository_root,
        &dbt_root,
        &base_sha,
        &snapshot_hash,
        &validation_config,
        &validation_options,
    )?;
    Ok(PreparedSnapshot {
        dbt_root,
        owner,
        name,
        base_sha,
        head_sha,
        snapshot_hash,
        fingerprint,
        files,
        validation_config,
        validation_options,
        total_bytes,
    })
}

pub fn print_snapshot(snapshot: &PreparedSnapshot) {
    eprintln!(
        "Preparing a safe snapshot...\n{} changed files, {} bytes",
        snapshot.files.len(),
        snapshot.total_bytes
    );
    for file in &snapshot.files {
        eprintln!("  {} {}", file.status, file.path);
    }
}

pub fn cached_review(snapshot: &PreparedSnapshot) -> Result<Option<Report>> {
    let path = review_cache_path()?;
    let cache: ReviewCache = match fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).context("local review cache is invalid")?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("could not read {}", path.display()));
        }
    };
    Ok(cache_is_reusable(
        &cache.fingerprint,
        &snapshot.fingerprint,
        &cache.saved_at,
        cache.report.schema_version,
    )
    .then_some(cache.report))
}

fn cache_is_reusable(
    cached_fingerprint: &str,
    snapshot_fingerprint: &str,
    saved_at: &str,
    report_schema_version: u8,
) -> bool {
    cached_fingerprint == snapshot_fingerprint
        && cache_is_fresh(saved_at)
        && report_schema_version == 4
}

pub fn save_review(snapshot: &PreparedSnapshot, report: &Report) -> Result<()> {
    write_private_json(
        &review_cache_path()?,
        &ReviewCache {
            fingerprint: snapshot.fingerprint.clone(),
            report: report.clone(),
            saved_at: Utc::now().to_rfc3339(),
        },
    )
}

pub async fn handoff(
    snapshot: &PreparedSnapshot,
    report: &Report,
    base_ref: &str,
    intent: &str,
) -> Result<HandoffReceipt> {
    let mut session = valid_session().await?;
    let idempotency_key = format!("cli:{}", Uuid::new_v4());
    let payload = handoff_payload(snapshot, report, base_ref, intent, &idempotency_key);
    let mut response = send_handoff(&session, &payload).await?;
    if response.status() == StatusCode::UNAUTHORIZED {
        session = refresh_session(&session).await?;
        save_session(&session)?;
        response = send_handoff(&session, &payload).await?;
    }
    parse_handoff(response, snapshot).await
}

fn handoff_payload(
    snapshot: &PreparedSnapshot,
    report: &Report,
    base_ref: &str,
    intent: &str,
    idempotency_key: &str,
) -> Value {
    json!({
        "idempotency_key": idempotency_key,
        "repository": {
            "provider": "github",
            "owner": snapshot.owner,
            "name": snapshot.name,
            "dbt_root": snapshot.dbt_root,
            "base_ref": base_ref,
            "base_sha": snapshot.base_sha,
            "head_sha": snapshot.head_sha,
        },
        "snapshot_hash": snapshot.snapshot_hash,
        "intent_context": intent,
        "changed_files": snapshot.files,
        "local_review": report,
        "validation_config": snapshot.validation_config,
        "validation_options": snapshot.validation_options,
        "cli": {"version": env!("CARGO_PKG_VERSION"), "platform": env::consts::OS},
        "notify_slack": true,
    })
}

async fn send_handoff(session: &CloudSession, payload: &Value) -> Result<reqwest::Response> {
    update::http_client()?
        .post(format!(
            "{}/v1/agent/cloud-handoffs",
            session.api_base_url.trim_end_matches('/')
        ))
        .bearer_auth(&session.access_token)
        .json(&payload)
        .send()
        .await
        .context("could not reach Embrasure Cloud")
}

async fn parse_handoff(
    response: reqwest::Response,
    snapshot: &PreparedSnapshot,
) -> Result<HandoffReceipt> {
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("cloud handoff failed ({status}): {}", api_error(&body));
    }
    let result: HandoffResponse =
        serde_json::from_str(&body).context("cloud handoff returned an invalid response")?;
    if result.snapshot_hash != snapshot.snapshot_hash {
        bail!("cloud handoff receipt did not match the uploaded snapshot");
    }
    let receipt = HandoffReceipt {
        handoff_id: result.handoff_id,
        run_id: result.run_id,
        run_url: result.run_url,
        snapshot_hash: result.snapshot_hash,
        base_sha: snapshot.base_sha.clone(),
        accepted_at: result.accepted_at,
    };
    write_private_json(&receipt_path()?, &receipt)?;
    Ok(receipt)
}

pub async fn login() -> Result<CloudSession> {
    let api_base_url = api_base_url();
    let web_base_url = web_base_url(&api_base_url);
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .context("could not start the cloud login callback")?;
    let port = listener.local_addr()?.port();
    let return_url = format!("http://127.0.0.1:{port}/callback");
    let state = loopback::random_string(32);
    let (verifier, challenge) = loopback::pkce_pair();
    let mut url = Url::parse(&format!(
        "{}/cli-auth/complete",
        web_base_url.trim_end_matches('/')
    ))?;
    url.query_pairs_mut()
        .append_pair("state", &state)
        .append_pair("api_base_url", &api_base_url)
        .append_pair("return_url", &return_url)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair(
            "device_id",
            &format!("rust-cli-{}", loopback::random_string(24)),
        )
        .append_pair("device_name", "Embrasure CLI")
        .append_pair("platform", env::consts::OS)
        .append_pair("client", "cli")
        .append_pair("client_version", env!("CARGO_PKG_VERSION"));
    eprintln!(
        "Opening Embrasure Cloud sign-in in your browser.\n{}",
        url.as_str()
    );
    let _ = webbrowser::open(url.as_str());
    let (mut stream, request) = loopback::accept(
        &listener,
        Duration::from_secs(180),
        "cloud login timed out after 3 minutes",
        "cloud login callback timed out",
    )
    .await?;
    let request = String::from_utf8_lossy(&request);
    let target = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .context("cloud login callback was malformed")?;
    let callback = Url::parse(&format!("http://127.0.0.1{target}"))?;
    let callback_state = callback
        .query_pairs()
        .find(|(key, _)| key == "state")
        .map(|(_, value)| value.into_owned())
        .unwrap_or_default();
    let code = callback
        .query_pairs()
        .find(|(key, _)| key == "code")
        .map(|(_, value)| value.into_owned())
        .unwrap_or_default();
    if callback.path() != "/callback" || callback_state != state || code.is_empty() {
        loopback::respond(
            &mut stream,
            "400 Bad Request",
            "Cloud sign-in failed",
            "Return to the terminal and try again.",
        )
        .await?;
        bail!("cloud login returned an invalid callback");
    }
    loopback::respond(
        &mut stream,
        "200 OK",
        "Embrasure Cloud is connected",
        "You can close this tab and return to your terminal.",
    )
    .await?;
    let response = update::http_client()?
        .post(format!("{}/v1/auth/session/token", api_base_url.trim_end_matches('/')))
        .json(&json!({"grant_type":"authorization_code","code":code,"code_verifier":verifier,"redirect_uri":return_url}))
        .send().await.context("could not exchange the cloud authorization code")?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("cloud login failed: {}", api_error(&body));
    }
    let token: TokenResponse =
        serde_json::from_str(&body).context("cloud login returned an invalid token")?;
    let session = CloudSession {
        access_token: token.access_token,
        refresh_token: token.refresh_token,
        expires_at: token.expires_at,
        workspace_id: token.workspace_id,
        api_base_url,
    };
    save_session(&session)?;
    Ok(session)
}

pub async fn whoami() -> Result<Value> {
    let session = valid_session().await?;
    api_get(&session, "/v1/auth/whoami", &[]).await
}

pub async fn status(run_id: Option<&str>) -> Result<Value> {
    let session = valid_session().await?;
    let id = match run_id {
        Some(value) => value.to_owned(),
        None => read_receipt()?.run_id,
    };
    api_get(
        &session,
        &format!("/v1/agent/runs/{id}/cloud-summary"),
        &[("workspace_id", session.workspace_id.as_str())],
    )
    .await
}

pub async fn logout() -> Result<()> {
    if let Ok(session) = load_session()
        && let Ok(client) = update::http_client()
    {
        let _ = client
            .post(format!(
                "{}/v1/auth/session/revoke",
                session.api_base_url.trim_end_matches('/')
            ))
            .json(&json!({"refresh_token": session.refresh_token}))
            .send()
            .await;
    }
    match keychain()?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(error) => Err(keyring_error(
            error,
            "could not remove the Embrasure Cloud keychain session",
        )),
    }
}

async fn valid_session() -> Result<CloudSession> {
    if let Ok(access_token) = env::var("EMBRASURE_CLOUD_TOKEN")
        && !access_token.trim().is_empty()
    {
        return Ok(CloudSession {
            access_token,
            refresh_token: String::new(),
            expires_at: "9999-12-31T23:59:59Z".into(),
            workspace_id: env::var("EMBRASURE_CLOUD_WORKSPACE_ID").unwrap_or_default(),
            api_base_url: api_base_url(),
        });
    }
    let session =
        load_session().context("not signed in to Embrasure Cloud; run `embrasure cloud login`")?;
    let expires = DateTime::parse_from_rfc3339(&session.expires_at)?.with_timezone(&Utc);
    if expires > Utc::now() + chrono::Duration::seconds(30) {
        return Ok(session);
    }
    let refreshed = refresh_session(&session).await?;
    save_session(&refreshed)?;
    Ok(refreshed)
}

async fn refresh_session(session: &CloudSession) -> Result<CloudSession> {
    let response = update::http_client()?
        .post(format!(
            "{}/v1/auth/session/refresh",
            session.api_base_url.trim_end_matches('/')
        ))
        .json(&json!({"grant_type":"refresh_token","refresh_token":session.refresh_token}))
        .send()
        .await?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!(
            "cloud session expired: {}; run `embrasure cloud login`",
            api_error(&body)
        );
    }
    let token: TokenResponse = serde_json::from_str(&body)?;
    Ok(CloudSession {
        access_token: token.access_token,
        refresh_token: token.refresh_token,
        expires_at: token.expires_at,
        workspace_id: token.workspace_id,
        api_base_url: session.api_base_url.clone(),
    })
}

async fn api_get(session: &CloudSession, path: &str, query: &[(&str, &str)]) -> Result<Value> {
    let response = update::http_client()?
        .get(format!(
            "{}{}",
            session.api_base_url.trim_end_matches('/'),
            path
        ))
        .bearer_auth(&session.access_token)
        .query(query)
        .send()
        .await?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!(
            "Embrasure Cloud request failed ({status}): {}",
            api_error(&body)
        );
    }
    serde_json::from_str(&body).context("Embrasure Cloud returned invalid JSON")
}

fn keychain() -> Result<Entry> {
    Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)
        .map_err(|error| keyring_error(error, "could not open the Embrasure Cloud keychain entry"))
}

fn save_session(session: &CloudSession) -> Result<()> {
    keychain()?
        .set_password(&serde_json::to_string(session)?)
        .map_err(|error| {
            keyring_error(
                error,
                "could not save the Embrasure Cloud session in the OS keychain",
            )
        })
}

fn load_session() -> Result<CloudSession> {
    let value = keychain()?
        .get_password()
        .map_err(|error| keyring_error(error, "no Embrasure Cloud session is stored"))?;
    serde_json::from_str(&value).context("the Embrasure Cloud keychain session is invalid")
}

fn keyring_error(error: keyring::Error, context: &str) -> anyhow::Error {
    match error {
        keyring::Error::PlatformFailure(_) | keyring::Error::NoStorageAccess(_) => anyhow::anyhow!(
            "{context}: secure storage is unavailable; on headless Linux, start and unlock Secret Service or set EMBRASURE_CLOUD_TOKEN and optional EMBRASURE_CLOUD_WORKSPACE_ID"
        ),
        other => anyhow::Error::new(other).context(context.to_owned()),
    }
}

fn api_base_url() -> String {
    env::var("EMBRASURE_API_URL")
        .unwrap_or_else(|_| "https://api.embrasure.ai".into())
        .trim_end_matches('/')
        .to_owned()
}

fn web_base_url(api: &str) -> String {
    env::var("EMBRASURE_WEB_URL").unwrap_or_else(|_| {
        if api.contains("localhost") || api.contains("127.0.0.1") {
            "http://localhost:3000".into()
        } else {
            "https://app.embrasure.ai".into()
        }
    })
}

fn project_dirs() -> Result<ProjectDirs> {
    ProjectDirs::from("ai", "Embrasure", "embrasure-cli")
        .context("could not resolve the OS application directory")
}

fn review_cache_path() -> Result<PathBuf> {
    Ok(project_dirs()?.cache_dir().join("cloud-review.json"))
}
fn receipt_path() -> Result<PathBuf> {
    Ok(project_dirs()?
        .data_local_dir()
        .join("last-cloud-handoff.json"))
}

fn read_receipt() -> Result<HandoffReceipt> {
    let path = receipt_path()?;
    serde_json::from_slice(
        &fs::read(&path)
            .with_context(|| format!("no cloud handoff receipt at {}", path.display()))?,
    )
    .context("the cloud handoff receipt is invalid")
}

fn write_private_json(path: &Path, value: &impl Serialize) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, serde_json::to_vec_pretty(value)?)
        .with_context(|| format!("could not write {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn cache_is_fresh(saved_at: &str) -> bool {
    let Ok(saved_at) = DateTime::parse_from_rfc3339(saved_at) else {
        return false;
    };
    let age = Utc::now().signed_duration_since(saved_at.with_timezone(&Utc));
    age >= chrono::Duration::zero() && age <= chrono::Duration::hours(1)
}

fn git_path(cwd: &Path, args: &[&str]) -> Result<PathBuf> {
    Ok(PathBuf::from(git::text(cwd, args)?).canonicalize()?)
}

fn changed_paths(root: &Path, base: &str) -> Result<Vec<(String, String)>> {
    let output = git::output(root, &["diff", "--name-status", "-z", base, "--"])?;
    if !output.status.success() {
        bail!(
            "could not inspect working-tree changes: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let parts = output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    let mut rows = Vec::new();
    let mut index = 0;
    while index + 1 < parts.len() {
        let code = String::from_utf8_lossy(parts[index]);
        if code.starts_with('R') {
            if index + 2 >= parts.len() {
                break;
            }
            rows.push((
                String::from_utf8(parts[index + 1].to_vec())?,
                "deleted".into(),
            ));
            rows.push((
                String::from_utf8(parts[index + 2].to_vec())?,
                "added".into(),
            ));
            index += 3;
            continue;
        }
        let (path_index, status, width) = if code.starts_with('C') {
            (index + 2, "added", 3)
        } else {
            (
                index + 1,
                match code.chars().next().unwrap_or('M') {
                    'A' => "added",
                    'D' => "deleted",
                    _ => "modified",
                },
                2,
            )
        };
        if path_index >= parts.len() {
            break;
        }
        rows.push((
            String::from_utf8(parts[path_index].to_vec())?,
            status.into(),
        ));
        index += width;
    }
    let untracked = git::output(root, &["ls-files", "--others", "--exclude-standard", "-z"])?;
    if !untracked.status.success() {
        bail!(
            "could not inspect untracked working-tree files: {}",
            String::from_utf8_lossy(&untracked.stderr).trim()
        );
    }
    for path in untracked
        .stdout
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
    {
        rows.push((String::from_utf8(path.to_vec())?, "added".into()));
    }
    Ok(rows)
}

fn parse_github_remote(remote: &str) -> Result<(String, String)> {
    let path = if let Some(value) = remote.strip_prefix("git@github.com:") {
        value.to_owned()
    } else {
        let url = Url::parse(remote).context("origin is not a supported GitHub URL")?;
        if !url
            .host_str()
            .is_some_and(|host| host.eq_ignore_ascii_case("github.com"))
        {
            bail!("origin must be a GitHub repository");
        }
        url.path().trim_matches('/').to_owned()
    };
    let path = path.strip_suffix(".git").unwrap_or(&path);
    let mut parts = path.split('/');
    let owner = parts.next().unwrap_or_default();
    let name = parts.next().unwrap_or_default();
    if owner.is_empty() || name.is_empty() || parts.next().is_some() {
        bail!("origin must identify one GitHub owner and repository");
    }
    Ok((owner.into(), name.into()))
}

fn normalize_snapshot_path(path: &Path) -> Result<String> {
    if path.is_absolute()
        || path
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        bail!("unsafe snapshot path: {}", path.display());
    }
    Ok(path.to_string_lossy().replace('\\', "/"))
}

fn eligible(path: &str) -> bool {
    if excluded(path) {
        return false;
    }
    let lower = path.to_ascii_lowercase();
    [".sql", ".yml", ".yaml", ".py", ".json", ".md", ".csv"]
        .iter()
        .any(|suffix| lower.ends_with(suffix))
}

fn excluded(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let parts = lower.split('/').collect::<Vec<_>>();
    if parts.iter().any(|part| {
        matches!(
            *part,
            ".git" | "target" | "logs" | "dbt_packages" | ".venv" | "venv" | "node_modules"
        )
    }) || parts.iter().any(|part| part.starts_with(".env"))
        || parts.last().is_some_and(|name| {
            matches!(
                *name,
                "profiles.yml"
                    | "profiles.yaml"
                    | "credentials"
                    | "credentials.json"
                    | "credentials.yml"
                    | "credentials.yaml"
            )
        })
    {
        return true;
    }
    false
}

fn contains_secret(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    lower.contains("-----begin private key-----")
        || [
            "password=",
            "password:",
            "client_secret=",
            "client_secret:",
            "private_key=",
            "access_token=",
            "xoxb-",
            "github_pat_",
            "ghp_",
        ]
        .iter()
        .any(|needle| lower.contains(needle))
}

fn hex_digest(bytes: &[u8]) -> String {
    encode_hex(&Sha256::digest(bytes))
}

fn snapshot_fingerprint(
    repository_root: &Path,
    dbt_root: &str,
    base_sha: &str,
    snapshot_hash: &str,
    validation_config: &ValidationConfigSnapshot,
    validation_options: &ValidationOptionsSnapshot,
) -> Result<String> {
    let config = serde_json::to_vec(validation_config)?;
    let options = serde_json::to_vec(validation_options)?;
    let parts = [
        repository_root.to_string_lossy().as_bytes().to_vec(),
        dbt_root.as_bytes().to_vec(),
        base_sha.as_bytes().to_vec(),
        snapshot_hash.as_bytes().to_vec(),
        env!("CARGO_PKG_VERSION").as_bytes().to_vec(),
        config,
        options,
    ];
    let mut fingerprint = Sha256::new();
    for part in parts {
        fingerprint.update((part.len() as u64).to_be_bytes());
        fingerprint.update(part);
    }
    Ok(encode_hex(&fingerprint.finalize()))
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

fn api_error(body: &str) -> String {
    let value: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    value
        .pointer("/detail/message")
        .or_else(|| value.get("detail"))
        .or_else(|| value.get("error"))
        .and_then(Value::as_str)
        .unwrap_or("request failed")
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_is_required_and_secret_checked() {
        assert!(normalize_context(&[], None).is_err());
        assert!(normalize_context(&["access_token=secret-value".into()], None).is_err());
        assert_eq!(
            normalize_context(&["one row per order".into()], None).unwrap(),
            "one row per order"
        );
    }

    #[test]
    fn context_values_and_file_are_normalized_in_order() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("intent.md");
        fs::write(&path, "  Daily totals reconcile within one cent.  \n").unwrap();
        assert_eq!(
            normalize_context(
                &[
                    "  Preserve one row per order. ".into(),
                    "Refunds reduce net revenue.".into(),
                ],
                Some(&path),
            )
            .unwrap(),
            "Preserve one row per order.\n\nRefunds reduce net revenue.\n\nDaily totals reconcile within one cent."
        );
    }

    #[test]
    fn github_remotes_are_parsed() {
        assert_eq!(
            parse_github_remote("git@github.com:EmbrasureAI/demo.git").unwrap(),
            ("EmbrasureAI".into(), "demo".into())
        );
        assert_eq!(
            parse_github_remote("https://github.com/EmbrasureAI/demo.git").unwrap(),
            ("EmbrasureAI".into(), "demo".into())
        );
    }

    #[test]
    fn snapshot_exclusions_and_animation_rules_are_stable() {
        assert!(!eligible("profiles.yml"));
        assert!(!eligible("config/profiles.yml"));
        assert!(!eligible("config/credentials.json"));
        assert!(!eligible("target/manifest.json"));
        assert!(eligible("models/finance/fct_order_margin.sql"));
        assert!(normalize_snapshot_path(Path::new("../outside.sql")).is_err());
        assert!(normalize_snapshot_path(Path::new("models/finance/model.sql")).is_ok());
    }

    #[test]
    fn receipts_never_serialize_source_or_tokens() {
        let receipt = HandoffReceipt {
            handoff_id: "handoff_123".into(),
            run_id: "run_123".into(),
            run_url: "https://app.embrasure.ai/agents/context/runs/run_123".into(),
            snapshot_hash: "a".repeat(64),
            base_sha: "b".repeat(40),
            accepted_at: "2026-08-15T00:00:00Z".into(),
        };
        let serialized = serde_json::to_string(&receipt).unwrap();
        assert!(!serialized.contains("access_token"));
        assert!(!serialized.contains("refresh_token"));
        assert!(!serialized.contains("content_utf8"));
    }

    #[test]
    fn handoff_includes_the_exact_hashed_validation_config() {
        let config = "version: 2\naccounts: []\n";
        let snapshot = PreparedSnapshot {
            dbt_root: "analytics".into(),
            owner: "EmbrasureAI".into(),
            name: "demo".into(),
            base_sha: "a".repeat(40),
            head_sha: Some("b".repeat(40)),
            snapshot_hash: "c".repeat(64),
            fingerprint: "d".repeat(64),
            files: vec![],
            validation_config: ValidationConfigSnapshot {
                sha256: hex_digest(config.as_bytes()),
                content_utf8: config.into(),
                repository_path: Some("embrasure-check.yml".into()),
            },
            validation_options: ValidationOptionsSnapshot {
                mode: Some(ComparisonMode::Quick),
                downstream: Some(DownstreamPolicy::All),
                critical_tags: Some(vec!["critical".into()]),
                incremental_mode: Some(IncrementalMode::FullRefresh),
                select: vec!["orders".into()],
            },
            total_bytes: 0,
        };
        let report = Report::empty("origin/main".into(), crate::config::Thresholds::default());
        let payload = handoff_payload(
            &snapshot,
            &report,
            "origin/main",
            "Preserve totals",
            "cli:request-1",
        );

        assert_eq!(payload["validation_config"]["content_utf8"], config);
        assert_eq!(
            payload["validation_config"]["repository_path"],
            "embrasure-check.yml"
        );
        assert_eq!(
            payload["validation_config"]["sha256"],
            hex_digest(config.as_bytes())
        );
        assert_eq!(payload["validation_options"]["mode"], "quick");
        assert_eq!(payload["validation_options"]["downstream"], "all");
        assert_eq!(
            payload["validation_options"]["incremental_mode"],
            "full_refresh"
        );
        assert_eq!(payload["validation_options"]["select"][0], "orders");
        assert_eq!(payload["idempotency_key"], "cli:request-1");
    }

    #[test]
    fn seed_files_are_eligible_for_the_exact_cloud_snapshot() {
        assert!(eligible("seeds/orders.csv"));
    }

    #[test]
    fn snapshot_fingerprint_uses_only_the_serialized_cloud_contract() {
        let repository = Path::new("/tmp/example-repository");
        let options = ValidationOptionsSnapshot {
            mode: Some(ComparisonMode::Quick),
            downstream: Some(DownstreamPolicy::All),
            critical_tags: Some(vec!["finance".into()]),
            incremental_mode: Some(IncrementalMode::FullRefresh),
            select: vec!["orders".into()],
        };
        let first_config = ValidationConfigSnapshot {
            sha256: "a".repeat(64),
            content_utf8: "version: 2\n".into(),
            repository_path: Some("embrasure-check.yml".into()),
        };
        let moved_config = ValidationConfigSnapshot {
            repository_path: Some("config/embrasure-check.yml".into()),
            ..first_config.clone()
        };
        let first = snapshot_fingerprint(
            repository,
            "analytics",
            &"b".repeat(40),
            &"c".repeat(64),
            &first_config,
            &options,
        )
        .unwrap();
        let repeated = snapshot_fingerprint(
            repository,
            "analytics",
            &"b".repeat(40),
            &"c".repeat(64),
            &first_config,
            &options,
        )
        .unwrap();
        let moved = snapshot_fingerprint(
            repository,
            "analytics",
            &"b".repeat(40),
            &"c".repeat(64),
            &moved_config,
            &options,
        )
        .unwrap();

        assert_eq!(first, repeated);
        assert_ne!(first, moved);
    }

    #[test]
    fn snapshot_includes_configured_external_changes_with_repository_paths() {
        fn run_git(root: &Path, args: &[&str]) {
            let output = std::process::Command::new("git")
                .args(args)
                .current_dir(root)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let repo = tempfile::tempdir().unwrap();
        run_git(repo.path(), &["init", "--quiet"]);
        run_git(repo.path(), &["config", "user.name", "Embrasure Test"]);
        run_git(repo.path(), &["config", "user.email", "test@embrasure.ai"]);
        run_git(
            repo.path(),
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/EmbrasureAI/demo.git",
            ],
        );
        fs::create_dir_all(repo.path().join("analytics/models")).unwrap();
        fs::create_dir_all(repo.path().join("shared")).unwrap();
        fs::write(
            repo.path().join("analytics/models/orders.sql"),
            "select 1\n",
        )
        .unwrap();
        fs::write(repo.path().join("shared/metrics.lock"), "one\n").unwrap();
        let config_path = repo.path().join("embrasure-check.yml");
        fs::write(
            &config_path,
            r#"
version: 1
dbt: { project_dir: analytics }
accounts:
  - name: primary
    account: org-account
    user: ci
    role: ci
    database: analytics
    warehouse: ci
    production_schema: prod
    auth: { type: oauth, token_env: TOKEN }
external_changes:
  - path: shared/*.lock
    models: [orders]
"#,
        )
        .unwrap();
        run_git(repo.path(), &["add", "."]);
        run_git(repo.path(), &["commit", "--quiet", "-m", "base"]);
        fs::write(
            repo.path().join("analytics/models/orders.sql"),
            "select 2\n",
        )
        .unwrap();
        fs::write(repo.path().join("shared/metrics.lock"), "two\n").unwrap();

        let mut config = Config::load(&config_path).unwrap();
        config.resolve_from(&config_path).unwrap();
        let snapshot =
            prepare_snapshot(&config_path, &config, "HEAD", &CheckOptions::default()).unwrap();
        let paths = snapshot
            .files
            .iter()
            .map(|file| file.path.as_str())
            .collect::<Vec<_>>();

        assert_eq!(
            paths,
            vec!["analytics/models/orders.sql", "shared/metrics.lock"]
        );
    }

    #[test]
    fn snapshot_changes_preserve_renames_and_untracked_files() {
        fn run_git(root: &Path, args: &[&str]) {
            let output = std::process::Command::new("git")
                .args(args)
                .current_dir(root)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let repo = tempfile::tempdir().unwrap();
        run_git(repo.path(), &["init", "--quiet"]);
        run_git(repo.path(), &["config", "user.name", "Embrasure Test"]);
        run_git(repo.path(), &["config", "user.email", "test@embrasure.ai"]);
        fs::create_dir(repo.path().join("models")).unwrap();
        fs::write(repo.path().join("models/old.sql"), "select 1\n").unwrap();
        run_git(repo.path(), &["add", "."]);
        run_git(repo.path(), &["commit", "--quiet", "-m", "base"]);
        run_git(repo.path(), &["mv", "models/old.sql", "models/new.sql"]);
        fs::write(repo.path().join("models/untracked.sql"), "select 2\n").unwrap();

        let mut changes = changed_paths(repo.path(), "HEAD").unwrap();
        changes.sort();
        assert_eq!(
            changes,
            vec![
                ("models/new.sql".into(), "added".into()),
                ("models/old.sql".into(), "deleted".into()),
                ("models/untracked.sql".into(), "added".into()),
            ]
        );
    }

    #[test]
    fn api_errors_use_actionable_server_messages() {
        assert_eq!(
            api_error(r#"{"detail":{"message":"Connect this GitHub dbt project."}}"#),
            "Connect this GitHub dbt project."
        );
        assert_eq!(api_error("not-json"), "request failed");
    }

    #[test]
    fn local_review_cache_is_only_reused_immediately() {
        assert!(cache_is_fresh(&Utc::now().to_rfc3339()));
        assert!(!cache_is_fresh(
            &(Utc::now() - chrono::Duration::hours(2)).to_rfc3339()
        ));
        assert!(!cache_is_fresh("not-a-timestamp"));
        let now = Utc::now().to_rfc3339();
        assert!(cache_is_reusable("same", "same", &now, 4));
        assert!(!cache_is_reusable("same", "same", &now, 3));
    }
}
