use std::{fs, path::Path};

use anyhow::{Context, Result, bail};
use serde_json::Value;
use tokio::{
    io::AsyncWriteExt,
    net::{TcpListener, TcpStream},
};

const TEMPLATE: &str = include_str!("../assets/report-viewer.html");
const LOGO: &str = include_str!("../assets/embrasure-logo-light.b64");
const REPORT_PLACEHOLDER: &str = "__EMBRASURE_REPORT__";
const LOGO_PLACEHOLDER: &str = "__EMBRASURE_LOGO__";

pub async fn run(path: &Path, open_browser: bool) -> Result<()> {
    let report = load(path)?;
    let page = render(&report)?;
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .context("could not start the local report viewer")?;
    let address = listener.local_addr()?;
    let url = format!("http://{address}/");

    eprintln!("embrasure: viewing {} at {url}", path.display());
    eprintln!("Press Ctrl-C to stop the viewer.");
    if open_browser && webbrowser::open(&url).is_err() {
        eprintln!("embrasure: could not open a browser; open {url} manually");
    }

    loop {
        tokio::select! {
            result = listener.accept() => {
                let (stream, _) = result.context("report viewer stopped accepting connections")?;
                if let Err(error) = respond(stream, page.as_bytes()).await {
                    eprintln!("embrasure: could not serve the report: {error:#}");
                }
            }
            result = tokio::signal::ctrl_c() => {
                result.context("could not listen for Ctrl-C")?;
                return Ok(());
            }
        }
    }
}

fn load(path: &Path) -> Result<Value> {
    let source = fs::read_to_string(path)
        .with_context(|| format!("could not read report {}", path.display()))?;
    let report: Value = serde_json::from_str(&source)
        .with_context(|| format!("{} is not valid JSON", path.display()))?;
    validate(&report)?;
    Ok(report)
}

fn validate(report: &Value) -> Result<()> {
    if report.get("schema_version").and_then(Value::as_u64) != Some(4) {
        bail!(
            "the report viewer requires report version 4; create it with `embrasure check --json --report-version 4`"
        );
    }
    for field in ["models", "query_checks", "findings", "coverage_gaps"] {
        if !report.get(field).is_some_and(Value::is_array) {
            bail!("report version 4 field `{field}` must be an array");
        }
    }
    if !report.get("impact").is_some_and(Value::is_object) {
        bail!("report version 4 field `impact` must be an object");
    }
    Ok(())
}

fn render(report: &Value) -> Result<String> {
    let json = serde_json::to_string(report)?
        .replace('<', "\\u003c")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029");
    Ok(TEMPLATE
        .replacen(REPORT_PLACEHOLDER, &json, 1)
        .replacen(LOGO_PLACEHOLDER, LOGO.trim(), 1))
}

async fn respond(mut stream: TcpStream, page: &[u8]) -> Result<()> {
    let request = crate::loopback::read_http_request(&mut stream).await?;
    let is_page =
        request.starts_with(b"GET / HTTP/") || request.starts_with(b"GET /index.html HTTP/");
    let (status, content_type, body) = if is_page {
        ("200 OK", "text/html; charset=utf-8", page)
    } else if request.starts_with(b"GET /favicon.ico HTTP/") {
        ("204 No Content", "image/x-icon", &[][..])
    } else {
        ("404 Not Found", "text/plain; charset=utf-8", &[][..])
    };
    let headers = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nContent-Security-Policy: default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; img-src data:; connect-src 'none'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(headers.as_bytes()).await?;
    stream.write_all(body).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn embedded_report_cannot_close_its_script() {
        let report = json!({
            "schema_version": 4,
            "models": [],
            "query_checks": [],
            "impact": {},
            "findings": [{"message": "</script><script>alert(1)</script>"}],
            "coverage_gaps": []
        });

        let page = render(&report).unwrap();

        assert!(!page.contains("</script><script>alert(1)</script>"));
        assert!(page.contains("\\u003c/script>"));
        assert!(!page.contains(LOGO_PLACEHOLDER));
    }

    #[test]
    fn viewer_requires_the_current_report_contract() {
        let error = validate(&json!({"schema_version": 3})).unwrap_err();
        assert!(error.to_string().contains("requires report version 4"));
    }
}
