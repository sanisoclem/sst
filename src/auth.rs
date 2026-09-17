use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64_URL;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::config::{SQL_SCOPE, write_private};

fn authority() -> String {
    std::env::var("SST_AUTHORITY")
        .unwrap_or_else(|_| "https://login.microsoftonline.com".to_string())
}
const REFRESH_EARLY_BY: u64 = 120;

#[derive(Debug, Clone)]
pub enum Prompt {
    DeviceCode {
        user_code: String,
        verification_uri: String,
    },
    Browser {
        url: String,
    },
}

#[derive(Debug, Deserialize)]
struct DeviceCode {
    device_code: String,
    user_code: String,
    verification_uri: String,
    #[serde(default = "default_interval")]
    interval: u64,
    expires_in: u64,
}

fn default_interval() -> u64 {
    5
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
    error: Option<String>,
    error_description: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Cache {
    #[serde(default)]
    tokens: BTreeMap<String, Cached>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Cached {
    access_token: String,
    expires_at: u64,
    refresh_token: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
#[clap(rename_all = "kebab-case")]
pub enum Flow {
    Browser,
    #[serde(alias = "device")]
    DeviceCode,
}

pub async fn token(
    tenant: &str,
    client_id: &str,
    flow: Flow,
    prompt: impl FnOnce(Prompt),
) -> Result<String> {
    let key = format!("{tenant}|{client_id}");
    let mut cache = load_cache();

    if let Some(cached) = cache.tokens.get(&key) {
        if cached.expires_at > now() + REFRESH_EARLY_BY {
            return Ok(cached.access_token.clone());
        }
        if let Some(refresh) = cached.refresh_token.clone()
            && let Ok(fresh) = refresh_token(tenant, client_id, &refresh).await
        {
            store(&mut cache, &key, fresh.clone());
            return Ok(fresh.access_token);
        }
    }

    let fresh = match flow {
        Flow::Browser => browser_flow(tenant, client_id, prompt).await?,
        Flow::DeviceCode => device_code_flow(tenant, client_id, prompt).await?,
    };
    store(&mut cache, &key, fresh.clone());
    Ok(fresh.access_token)
}

pub fn token_from_command(command: &str) -> Result<String> {
    let output = std::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .output()
        .with_context(|| format!("running {command:?}"))?;

    if !output.status.success() {
        let complaint = String::from_utf8_lossy(&output.stderr);
        bail!(
            "token command failed: {}",
            complaint.lines().next().unwrap_or("no output").trim()
        );
    }

    let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if token.is_empty() {
        bail!("token command printed nothing");
    }
    if token.split('.').count() != 3 {
        bail!("token command printed something that is not a bearer token");
    }
    Ok(token)
}

pub fn forget_tokens() -> Result<()> {
    let path = cache_path().context("no config directory on this system")?;
    if path.exists() {
        std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
    }
    Ok(())
}

async fn device_code_flow(
    tenant: &str,
    client_id: &str,
    prompt: impl FnOnce(Prompt),
) -> Result<Cached> {
    let http = reqwest::Client::new();
    let start: DeviceCode = http
        .post(format!("{}/{tenant}/oauth2/v2.0/devicecode", authority()))
        .form(&[("client_id", client_id), ("scope", &scope())])
        .send()
        .await?
        .json()
        .await
        .context("Azure did not return a device code")?;

    prompt(Prompt::DeviceCode {
        user_code: start.user_code.clone(),
        verification_uri: start.verification_uri.clone(),
    });

    let deadline = now() + start.expires_in;
    let mut interval = start.interval;
    while now() < deadline {
        tokio::time::sleep(Duration::from_secs(interval)).await;

        let response: TokenResponse = http
            .post(format!("{}/{tenant}/oauth2/v2.0/token", authority()))
            .form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("client_id", client_id),
                ("device_code", &start.device_code),
            ])
            .send()
            .await?
            .json()
            .await?;

        match response.error.as_deref() {
            None => return cached_from(response),
            Some("authorization_pending") => continue,
            Some("slow_down") => interval += 5,
            Some("expired_token") => bail!("the sign-in code expired"),
            Some("authorization_declined") => bail!("the sign-in was declined"),
            Some(other) => bail!(
                "{other}: {}",
                response.error_description.unwrap_or_default()
            ),
        }
    }
    bail!("timed out waiting for the sign-in")
}

async fn browser_flow(
    tenant: &str,
    client_id: &str,
    prompt: impl FnOnce(Prompt),
) -> Result<Cached> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .context("opening a loopback port for the sign-in redirect")?;
    let redirect = format!("http://localhost:{}", listener.local_addr()?.port());

    let verifier = random_string();
    let challenge = B64_URL.encode(Sha256::digest(verifier.as_bytes()));
    let state = random_string();

    let url = format!(
        "{}/{tenant}/oauth2/v2.0/authorize?client_id={}&response_type=code&redirect_uri={}\
         &response_mode=query&scope={}&state={}&code_challenge={challenge}&code_challenge_method=S256",
        authority(),
        encode(client_id),
        encode(&redirect),
        encode(&scope()),
        encode(&state),
    );

    let _ = open_browser(&url);
    prompt(Prompt::Browser { url });

    let code = wait_for_redirect(listener, &state).await?;
    let response: TokenResponse = reqwest::Client::new()
        .post(format!("{}/{tenant}/oauth2/v2.0/token", authority()))
        .form(&[
            ("grant_type", "authorization_code"),
            ("client_id", client_id),
            ("code", &code),
            ("redirect_uri", &redirect),
            ("code_verifier", &verifier),
        ])
        .send()
        .await?
        .json()
        .await?;
    cached_from(response)
}

async fn wait_for_redirect(listener: tokio::net::TcpListener, state: &str) -> Result<String> {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let deadline = std::time::Duration::from_secs(300);
    let accepted = tokio::time::timeout(deadline, listener.accept())
        .await
        .context("timed out waiting for the browser sign-in")?;
    let (mut socket, _) = accepted?;

    let mut buffer = [0u8; 8192];
    let read = socket.read(&mut buffer).await?;
    let request = String::from_utf8_lossy(&buffer[..read]);
    let target = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or_default();

    let outcome = read_code(target, state);
    let page = match &outcome {
        Ok(_) => "<h2>Signed in</h2><p>You can close this tab and go back to the terminal.</p>",
        Err(_) => "<h2>Sign-in failed</h2><p>The terminal has the details.</p>",
    };
    let _ = socket
        .write_all(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{page}",
                page.len()
            )
            .as_bytes(),
        )
        .await;
    let _ = socket.shutdown().await;
    outcome
}

fn read_code(target: &str, expected_state: &str) -> Result<String> {
    let query = target.split_once('?').map(|(_, query)| query).unwrap_or("");
    let mut code = None;
    let mut state = None;
    let mut error = None;
    let mut description = None;

    for pair in query.split('&') {
        let Some((name, value)) = pair.split_once('=') else {
            continue;
        };
        let value = urlencoding::decode(value).unwrap_or_default().into_owned();
        match name {
            "code" => code = Some(value),
            "state" => state = Some(value),
            "error" => error = Some(value),
            "error_description" => description = Some(value),
            _ => {}
        }
    }

    if let Some(error) = error {
        bail!(
            "{error}: {}",
            description.unwrap_or_default().replace('+', " ")
        );
    }
    if state.as_deref() != Some(expected_state) {
        bail!("the sign-in came back with the wrong state");
    }
    code.context("the sign-in came back without a code")
}

fn open_browser(url: &str) -> Result<()> {
    let opener = std::env::var("SST_BROWSER").unwrap_or_else(|_| {
        match std::env::consts::OS {
            "macos" => "open",
            "windows" => "explorer",
            _ => "xdg-open",
        }
        .to_string()
    });
    std::process::Command::new(opener)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    Ok(())
}

fn random_string() -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("the system random source");
    B64_URL.encode(bytes)
}

fn encode(value: &str) -> String {
    urlencoding::encode(value).into_owned()
}

async fn refresh_token(tenant: &str, client_id: &str, refresh: &str) -> Result<Cached> {
    let response: TokenResponse = reqwest::Client::new()
        .post(format!("{}/{tenant}/oauth2/v2.0/token", authority()))
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", client_id),
            ("refresh_token", refresh),
            ("scope", &scope()),
        ])
        .send()
        .await?
        .json()
        .await?;
    cached_from(response)
}

fn cached_from(response: TokenResponse) -> Result<Cached> {
    if let Some(error) = response.error {
        bail!(
            "{error}: {}",
            response.error_description.unwrap_or_default()
        );
    }
    Ok(Cached {
        access_token: response.access_token.context("no access token returned")?,
        expires_at: now() + response.expires_in.unwrap_or(3600),
        refresh_token: response.refresh_token,
    })
}

fn scope() -> String {
    format!("{SQL_SCOPE} offline_access")
}

fn store(cache: &mut Cache, key: &str, token: Cached) {
    cache.tokens.insert(key.to_string(), token);
    if let (Some(path), Ok(text)) = (cache_path(), serde_json::to_string_pretty(cache)) {
        let _ = write_private(&path, &text);
    }
}

fn load_cache() -> Cache {
    cache_path()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn cache_path() -> Option<PathBuf> {
    Some(crate::config::directory()?.join("tokens.json"))
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_code_out_of_a_redirect() {
        let code = read_code("/?code=abc123&state=xyz", "xyz").unwrap();
        assert_eq!(code, "abc123");
    }

    #[test]
    fn refuses_a_redirect_whose_state_does_not_match() {
        let error = read_code("/?code=abc123&state=somebody-else", "xyz")
            .unwrap_err()
            .to_string();
        assert!(error.contains("state"), "{error}");
        assert!(
            read_code("/?code=abc123", "xyz").is_err(),
            "no state at all"
        );
    }

    #[test]
    fn surfaces_the_error_the_sign_in_came_back_with() {
        let error = read_code(
            "/?error=access_denied&error_description=AADSTS53003%3A+Blocked+by+policy&state=xyz",
            "xyz",
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("access_denied"), "{error}");
        assert!(error.contains("Blocked by policy"), "{error}");
    }

    #[test]
    fn percent_escapes_in_the_code_are_decoded() {
        let code = read_code("/?code=a%2Fb%2Bc&state=xyz", "xyz").unwrap();
        assert_eq!(code, "a/b+c");
    }

    #[test]
    fn a_token_command_must_print_a_bearer_token() {
        assert!(token_from_command("echo aaa.bbb.ccc").is_ok());
        assert!(token_from_command("echo").is_err(), "empty output");
        assert!(token_from_command("echo not-a-token").is_err(), "not a JWT");
        assert!(
            token_from_command("echo '{\"accessToken\":\"x\"}'").is_err(),
            "raw JSON is a common mistake and should be caught"
        );
        assert!(token_from_command("exit 3").is_err(), "non-zero exit");
    }

    #[test]
    fn a_token_command_tolerates_trailing_whitespace() {
        assert_eq!(
            token_from_command("printf 'aaa.bbb.ccc\n'").unwrap(),
            "aaa.bbb.ccc"
        );
    }
}
