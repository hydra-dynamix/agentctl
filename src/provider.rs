use std::io::Read;
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::config::{ProviderConfig, ProviderKind};

const MAX_ERROR_BODY_BYTES: u64 = 8_192;

#[derive(Debug, Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: [ChatMessage<'a>; 1],
    stream: bool,
}

#[derive(Debug, Serialize)]
struct ChatMessage<'a> {
    role: &'static str,
    content: &'a str,
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Debug, Deserialize)]
struct ChatChoice {
    message: AssistantMessage,
}

#[derive(Debug, Deserialize)]
struct AssistantMessage {
    content: Option<String>,
}

pub fn complete(config: &ProviderConfig, model: Option<&str>, prompt: &str) -> Result<String> {
    if prompt.trim().is_empty() {
        bail!("provider prompt cannot be empty");
    }

    let api_key = resolve_api_key(config)?;
    let model = model.unwrap_or(&config.model);
    match config.kind {
        ProviderKind::Openrouter => complete_openrouter(config, model, prompt, &api_key),
    }
}

fn resolve_api_key(config: &ProviderConfig) -> Result<String> {
    let key = match (&config.api_key_env, &config.api_key_command) {
        (Some(name), None) => std::env::var(name).with_context(|| {
            format!("reading provider API key from environment variable {name}")
        })?,
        (None, Some(argv)) => {
            let (program, args) = argv
                .split_first()
                .ok_or_else(|| anyhow!("provider API key command is empty"))?;
            let output = Command::new(program)
                .args(args)
                .stdin(Stdio::null())
                .stderr(Stdio::piped())
                .output()
                .with_context(|| format!("running provider API key command `{program}`"))?;
            if !output.status.success() {
                bail!(
                    "provider API key command `{program}` exited with {}",
                    output.status.code().unwrap_or(1)
                );
            }
            String::from_utf8(output.stdout)
                .context("provider API key command returned non-UTF-8 output")?
        }
        _ => bail!("provider API key source is invalid"),
    };
    let key = key.trim().to_owned();
    if key.is_empty() {
        bail!("provider API key source returned an empty value");
    }
    Ok(key)
}

fn complete_openrouter(
    config: &ProviderConfig,
    model: &str,
    prompt: &str,
    api_key: &str,
) -> Result<String> {
    let url = format!("{}/chat/completions", config.base_url.trim_end_matches('/'));
    let request = ChatRequest {
        model,
        messages: [ChatMessage {
            role: "user",
            content: prompt,
        }],
        stream: false,
    };
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(120))
        .build();
    let response = agent
        .post(&url)
        .set("Authorization", &format!("Bearer {api_key}"))
        .set("Content-Type", "application/json")
        .set("HTTP-Referer", "https://github.com/hydra-dynamix/agentctl")
        .set("X-Title", "agentctl")
        .send_json(serde_json::to_value(request).context("serializing OpenRouter request")?);

    let response = match response {
        Ok(response) => response,
        Err(ureq::Error::Status(status, response)) => {
            let mut body = String::new();
            response
                .into_reader()
                .take(MAX_ERROR_BODY_BYTES)
                .read_to_string(&mut body)
                .context("reading OpenRouter error response")?;
            let detail = openrouter_error_message(&body);
            bail!("OpenRouter returned HTTP {status}: {detail}");
        }
        Err(ureq::Error::Transport(error)) => {
            return Err(anyhow!(error)).context("sending request to OpenRouter");
        }
    };

    let response: ChatResponse = response
        .into_json()
        .context("parsing OpenRouter response")?;
    response
        .choices
        .into_iter()
        .next()
        .and_then(|choice| choice.message.content)
        .filter(|content| !content.is_empty())
        .ok_or_else(|| anyhow!("OpenRouter response did not contain assistant text"))
}

fn openrouter_error_message(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|value| value.pointer("/error/message")?.as_str().map(str::to_owned))
        .unwrap_or_else(|| {
            let compact = body.split_whitespace().collect::<Vec<_>>().join(" ");
            if compact.is_empty() {
                "empty response body".to_owned()
            } else {
                compact
            }
        })
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    use super::*;

    #[test]
    fn openrouter_completion_sends_auth_model_and_prompt() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
        let address = listener.local_addr().expect("mock address");
        let server = thread::spawn(move || {
            let (mut socket, _) = listener.accept().expect("accept request");
            let mut request = Vec::new();
            let mut buffer = [0_u8; 4096];
            loop {
                let read = socket.read(&mut buffer).expect("read request");
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    let text = String::from_utf8_lossy(&request);
                    let content_length = text
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .and_then(|value| value.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    let header_end = request
                        .windows(4)
                        .position(|window| window == b"\r\n\r\n")
                        .expect("header end")
                        + 4;
                    if request.len() >= header_end + content_length {
                        break;
                    }
                }
            }
            let body = br#"{"choices":[{"message":{"content":"mock answer"}}]}"#;
            write!(
                socket,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .expect("write response headers");
            socket.write_all(body).expect("write response body");
            String::from_utf8(request).expect("request UTF-8")
        });

        let config = ProviderConfig {
            kind: ProviderKind::Openrouter,
            base_url: format!("http://{address}"),
            api_key_env: None,
            api_key_command: None,
            model: "default-model".to_owned(),
        };
        let answer = complete_openrouter(&config, "test/model", "hello", "test-key")
            .expect("OpenRouter completion");
        let request = server.join().expect("mock server");

        assert_eq!(answer, "mock answer");
        assert!(request.contains("Authorization: Bearer test-key"));
        assert!(request.contains("\"model\":\"test/model\""));
        assert!(request.contains("\"content\":\"hello\""));
    }

    #[test]
    fn extracts_structured_openrouter_error() {
        assert_eq!(
            openrouter_error_message(r#"{"error":{"message":"bad model"}}"#),
            "bad model"
        );
    }
}
