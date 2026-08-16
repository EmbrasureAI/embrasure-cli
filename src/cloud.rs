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
use keyring::Entry;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::net::TcpListener;
use url::Url;

use crate::{config::Config, git, loopback, report::Report, run::CheckOptions, style, update};

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

    let mut files = Vec::new();
    let mut total_bytes = 0;
    for (repo_path, status) in candidates {
        let relative = Path::new(&repo_path);
        let project_path = if dbt_root.is_empty() {
            relative
        } else if let Ok(path) = relative.strip_prefix(&dbt_root) {
            path
        } else {
            continue;
        };
        let normalized = normalize_snapshot_path(project_path)?;
        if !eligible(&normalized) {
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
    let config_bytes = fs::read(config_path).unwrap_or_default();
    let fingerprint = hex_digest(
        format!(
            "{}\0{}\0{}\0{}\0{}\0{}\0{:?}",
            repository_root.display(),
            dbt_root,
            base_sha,
            snapshot_hash,
            env!("CARGO_PKG_VERSION"),
            hex_digest(&config_bytes),
            options,
        )
        .as_bytes(),
    );
    Ok(PreparedSnapshot {
        dbt_root,
        owner,
        name,
        base_sha,
        head_sha,
        snapshot_hash,
        fingerprint,
        files,
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
    Ok(
        (cache.fingerprint == snapshot.fingerprint && cache_is_fresh(&cache.saved_at))
            .then_some(cache.report),
    )
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
    let payload = handoff_payload(snapshot, report, base_ref, intent);
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
) -> Value {
    json!({
        "idempotency_key": format!("cli:{}", snapshot.fingerprint),
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
    if let Ok(session) = load_session() {
        if let Ok(client) = update::http_client() {
            let _ = client
                .post(format!(
                    "{}/v1/auth/session/revoke",
                    session.api_base_url.trim_end_matches('/')
                ))
                .json(&json!({"refresh_token": session.refresh_token}))
                .send()
                .await;
        }
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
    // Cloud snapshots intentionally follow tracked Git diff output only.
    // Follow-up: decide whether cloud handoffs should include untracked files.
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
        let status = match code.chars().next().unwrap_or('M') {
            'A' => "added",
            'D' => "deleted",
            _ => "modified",
        };
        let path_index = if code.starts_with('R') || code.starts_with('C') {
            index + 2
        } else {
            index + 1
        };
        if path_index >= parts.len() {
            break;
        }
        rows.push((
            String::from_utf8(parts[path_index].to_vec())?,
            status.into(),
        ));
        index += if code.starts_with('R') || code.starts_with('C') {
            3
        } else {
            2
        };
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
        return false;
    }
    [".sql", ".yml", ".yaml", ".py", ".json", ".md"]
        .iter()
        .any(|suffix| lower.ends_with(suffix))
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
    }
}
