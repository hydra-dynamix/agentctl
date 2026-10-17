use std::collections::BTreeMap;
use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde::{Deserialize, Serialize};

use crate::config::GitHubAppConfig;

const MAX_SECRET_BYTES: u64 = 64 * 1024;
const MAX_ERROR_BYTES: u64 = 8 * 1024;

#[derive(Debug, Serialize)]
struct AppClaims<'a> {
    iat: u64,
    exp: u64,
    iss: &'a str,
}

#[derive(Debug, Deserialize)]
struct Installation {
    id: u64,
    permissions: BTreeMap<String, String>,
}

#[derive(Debug, Serialize)]
struct AccessTokenRequest<'a> {
    repositories: [&'a str; 1],
}

#[derive(Debug, Deserialize)]
struct AccessTokenResponse {
    token: String,
}

pub fn installation_token(config: &GitHubAppConfig, repository: &str) -> Result<String> {
    let (owner, name) = repository
        .split_once('/')
        .with_context(|| format!("invalid GitHub repository identity `{repository}`"))?;
    if owner.is_empty() || name.is_empty() || name.contains('/') {
        bail!("invalid GitHub repository identity `{repository}`");
    }

    let app_id = secret_command(&config.app_id_command, "GitHub App ID")?;
    let private_key = secret_command(&config.private_key_command, "GitHub App private key")?;
    let jwt = app_jwt(&app_id, &private_key)?;
    let api_url = config.api_url.trim_end_matches('/');
    let installation_url = format!("{api_url}/repos/{owner}/{name}/installation");
    let installation: Installation = request_json(
        ureq::get(&installation_url)
            .set("Authorization", &format!("Bearer {jwt}"))
            .set("Accept", "application/vnd.github+json")
            .set("X-GitHub-Api-Version", "2022-11-28")
            .set("User-Agent", "agentctl"),
        None,
        "locating GitHub App installation",
    )?;

    validate_permissions(&installation.permissions)?;
    let token_url = format!(
        "{api_url}/app/installations/{}/access_tokens",
        installation.id
    );
    let body = serde_json::to_value(AccessTokenRequest {
        repositories: [name],
    })
    .context("serializing GitHub installation-token request")?;
    let response: AccessTokenResponse = request_json(
        ureq::post(&token_url)
            .set("Authorization", &format!("Bearer {jwt}"))
            .set("Accept", "application/vnd.github+json")
            .set("X-GitHub-Api-Version", "2022-11-28")
            .set("User-Agent", "agentctl"),
        Some(body),
        "creating GitHub App installation token",
    )?;
    if response.token.trim().is_empty() {
        bail!("GitHub returned an empty installation token");
    }
    Ok(response.token)
}

fn validate_permissions(permissions: &BTreeMap<String, String>) -> Result<()> {
    let missing = [
        ("contents", "write"),
        ("pull_requests", "write"),
        ("checks", "read"),
        ("statuses", "read"),
    ]
    .into_iter()
    .filter_map(|(name, required)| {
        let actual = permissions.get(name).map(String::as_str).unwrap_or("none");
        let sufficient = actual == "write" || (required == "read" && actual == "read");
        (!sufficient).then(|| format!("{name}: {required} (current: {actual})"))
    })
    .collect::<Vec<_>>();
    if !missing.is_empty() {
        bail!(
            "GitHub App installation is missing required permissions: {}",
            missing.join(", ")
        );
    }
    Ok(())
}

fn secret_command(argv: &[String], label: &str) -> Result<String> {
    let (program, args) = argv
        .split_first()
        .with_context(|| format!("{label} command is empty"))?;
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("running {label} command `{program}`"))?;
    let stdout = child
        .stdout
        .take()
        .context("capturing secret command stdout")?;
    let stderr = child
        .stderr
        .take()
        .context("capturing secret command stderr")?;
    let stdout_handle = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.take(MAX_SECRET_BYTES + 1).read_to_end(&mut bytes)?;
        Ok::<_, std::io::Error>(bytes)
    });
    let stderr_handle = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stderr.take(MAX_ERROR_BYTES).read_to_end(&mut bytes)?;
        Ok::<_, std::io::Error>(bytes)
    });
    let status = child.wait().context("waiting for secret command")?;
    let stdout = stdout_handle
        .join()
        .map_err(|_| anyhow::anyhow!("secret command stdout reader panicked"))??;
    let stderr = stderr_handle
        .join()
        .map_err(|_| anyhow::anyhow!("secret command stderr reader panicked"))??;
    if !status.success() {
        bail!(
            "{label} command exited with {}: {}",
            status.code().unwrap_or(1),
            String::from_utf8_lossy(&stderr).trim()
        );
    }
    if stdout.len() > MAX_SECRET_BYTES as usize {
        bail!("{label} command output exceeded {MAX_SECRET_BYTES} bytes");
    }
    let value = String::from_utf8(stdout)
        .with_context(|| format!("{label} command returned non-UTF-8 output"))?;
    let value = value.trim().to_owned();
    if value.is_empty() {
        bail!("{label} command returned an empty value");
    }
    Ok(value)
}

fn app_jwt(app_id: &str, private_key: &str) -> Result<String> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock before unix epoch")?
        .as_secs();
    let claims = AppClaims {
        iat: now.saturating_sub(60),
        exp: now + 9 * 60,
        iss: app_id,
    };
    let key = EncodingKey::from_rsa_pem(private_key.as_bytes())
        .context("parsing GitHub App RSA private key")?;
    jsonwebtoken::encode(&Header::new(Algorithm::RS256), &claims, &key)
        .context("signing GitHub App JWT")
}

fn request_json<T: for<'de> Deserialize<'de>>(
    request: ureq::Request,
    body: Option<serde_json::Value>,
    action: &str,
) -> Result<T> {
    let response = match body {
        Some(body) => request.send_json(body),
        None => request.call(),
    };
    let response = match response {
        Ok(response) => response,
        Err(ureq::Error::Status(status, response)) => {
            let mut detail = String::new();
            response
                .into_reader()
                .take(MAX_ERROR_BYTES)
                .read_to_string(&mut detail)
                .context("reading GitHub API error response")?;
            let message = serde_json::from_str::<serde_json::Value>(&detail)
                .ok()
                .and_then(|value| value.get("message")?.as_str().map(str::to_owned))
                .unwrap_or_else(|| "request rejected without a message".to_owned());
            bail!("{action} failed with HTTP {status}: {message}");
        }
        Err(ureq::Error::Transport(error)) => {
            return Err(anyhow::Error::new(error)).with_context(|| action.to_owned());
        }
    };
    response
        .into_json()
        .with_context(|| format!("parsing response while {action}"))
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    use super::*;

    #[test]
    fn rejects_invalid_repository_identity_before_reading_secrets() {
        let config = GitHubAppConfig {
            app_id_command: vec!["unused".to_owned()],
            private_key_command: vec!["unused".to_owned()],
            api_url: "https://api.github.test".to_owned(),
        };
        let error = installation_token(&config, "missing-repository").unwrap_err();
        assert!(error.to_string().contains("invalid GitHub repository"));
    }

    #[test]
    #[cfg(unix)]
    fn mints_repository_scoped_token_against_mock_api() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for (body, expected_path) in [
                (
                    r#"{"id":77,"permissions":{"contents":"write","pull_requests":"write","checks":"read","statuses":"read"}}"#,
                    "/repos/owner/project/installation",
                ),
                (
                    r#"{"token":"installation-token"}"#,
                    "/app/installations/77/access_tokens",
                ),
            ] {
                let (mut socket, _) = listener.accept().unwrap();
                let mut request = Vec::new();
                let mut buffer = [0_u8; 4096];
                loop {
                    let count = socket.read(&mut buffer).unwrap();
                    if count == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..count]);
                    let Some(header_end) = request
                        .windows(4)
                        .position(|window| window == b"\r\n\r\n")
                        .map(|position| position + 4)
                    else {
                        continue;
                    };
                    let headers = String::from_utf8_lossy(&request[..header_end]);
                    let content_length = headers
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .and_then(|value| value.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if request.len() >= header_end + content_length {
                        break;
                    }
                }
                let text = String::from_utf8(request).unwrap();
                assert!(
                    text.starts_with(&format!("GET {expected_path} "))
                        || text.starts_with(&format!("POST {expected_path} "))
                );
                assert!(text.contains("Authorization: Bearer "));
                requests.push(text);
                write!(
                    socket,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .unwrap();
            }
            requests
        });
        let key_dir = tempfile::tempdir().unwrap();
        let key = key_dir.path().join("test-key.pem");
        let status = Command::new("openssl")
            .args(["genrsa", "-out"])
            .arg(&key)
            .arg("2048")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        let config = GitHubAppConfig {
            app_id_command: vec!["sh".to_owned(), "-c".to_owned(), "printf 123".to_owned()],
            private_key_command: vec!["cat".to_owned(), key.to_string_lossy().into_owned()],
            api_url: format!("http://{address}"),
        };

        let token = installation_token(&config, "owner/project").unwrap();
        let requests = server.join().unwrap();

        assert_eq!(token, "installation-token");
        assert!(requests[1].contains(r#"{"repositories":["project"]}"#));
        assert!(
            !requests
                .iter()
                .any(|request| request.contains("PRIVATE KEY"))
        );
    }
}
