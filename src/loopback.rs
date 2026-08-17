use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::{Rng, distributions::Alphanumeric};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::timeout,
};

pub fn random_string(length: usize) -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(length)
        .map(char::from)
        .collect()
}

pub fn pkce_pair() -> (String, String) {
    let verifier = random_string(64);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

pub async fn accept(
    listener: &TcpListener,
    accept_timeout: Duration,
    accept_context: &str,
    request_context: &str,
) -> Result<(TcpStream, Vec<u8>)> {
    let (mut stream, _) = timeout(accept_timeout, listener.accept())
        .await
        .with_context(|| accept_context.to_owned())??;
    let request = timeout(Duration::from_secs(10), read_http_request(&mut stream))
        .await
        .with_context(|| request_context.to_owned())??;
    Ok((stream, request))
}

pub async fn read_http_request(stream: &mut TcpStream) -> Result<Vec<u8>> {
    const MAX_REQUEST_BYTES: usize = 16 * 1024;
    let mut request = Vec::new();
    let mut buffer = [0_u8; 2048];
    loop {
        let length = stream.read(&mut buffer).await?;
        if length == 0 {
            break;
        }
        request.extend_from_slice(&buffer[..length]);
        if request.len() > MAX_REQUEST_BYTES {
            bail!("OAuth callback exceeded {MAX_REQUEST_BYTES} bytes");
        }
        if request.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    if request.is_empty() {
        bail!("OAuth callback closed without a request");
    }
    Ok(request)
}

pub async fn respond(
    stream: &mut TcpStream,
    status: &str,
    title: &str,
    message: &str,
) -> Result<()> {
    let body = format!("<html><body><h1>{title}</h1><p>{message}</p></body></html>");
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    Ok(())
}
