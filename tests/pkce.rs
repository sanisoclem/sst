use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64_URL;
use sha2::{Digest as _, Sha256};
use sst::auth::{self, Flow, Prompt};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};

const CODE: &str = "the-authorization-code";
const PAID_OUT_FOR_A_MATCHING_VERIFIER: &str = "header.payload.signature";

#[tokio::test]
async fn browser_flow_completes_the_pkce_exchange() {
    let authority = Authority::start().await;
    point_at_the_stand_in(&authority);

    let (tell, told) = tokio::sync::oneshot::channel();
    tokio::spawn(click_through(told));

    let mut seen = None;
    let token = auth::token("test-tenant", "test-client", Flow::Browser, |prompt| {
        let Prompt::Browser { url } = prompt else {
            panic!("asked for a device code, not a browser");
        };
        seen = Some(url.clone());
        let _ = tell.send(url);
    })
    .await
    .expect("sign-in");

    assert_eq!(token, PAID_OUT_FOR_A_MATCHING_VERIFIER);

    let url = seen.expect("a browser prompt");
    assert!(url.contains("code_challenge_method=S256"), "{url}");
    assert!(url.contains("response_type=code"), "{url}");
    assert!(
        url.contains("redirect_uri=http%3A%2F%2Flocalhost%3A"),
        "{url}"
    );
}

fn point_at_the_stand_in(authority: &Authority) {
    unsafe {
        std::env::set_var("SST_AUTHORITY", &authority.url);
        std::env::set_var("SST_BROWSER", "sst-opens-no-browser");
        std::env::set_var("XDG_CONFIG_HOME", scratch_directory());
    }
}

async fn click_through(told: tokio::sync::oneshot::Receiver<String>) {
    let Ok(url) = told.await else { return };
    let browser = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("http client");

    let page = browser.get(&url).send().await.expect("authorize");
    let back = page
        .headers()
        .get("location")
        .and_then(|location| location.to_str().ok())
        .expect("the authority redirects back")
        .to_string();
    let _ = browser.get(&back).send().await;
}

fn scratch_directory() -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("the clock")
        .as_nanos();
    let path = std::env::temp_dir().join(format!("sst-pkce-{nanos}"));
    std::fs::create_dir_all(&path).expect("scratch directory");
    path
}

struct Authority {
    url: String,
    challenge: Arc<Mutex<Option<String>>>,
}

impl Authority {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("a port");
        let authority = Authority {
            url: format!("http://{}", listener.local_addr().expect("address")),
            challenge: Arc::new(Mutex::new(None)),
        };

        tokio::spawn({
            let challenge = authority.challenge.clone();
            async move {
                while let Ok((socket, _)) = listener.accept().await {
                    answer(socket, &challenge).await;
                }
            }
        });
        authority
    }
}

async fn answer(mut socket: TcpStream, challenge: &Arc<Mutex<Option<String>>>) {
    let request = read_request(&mut socket).await;
    let response = match request.target.contains("/authorize") {
        true => redirect_back(&form(request.query()), challenge),
        false => pay_out(&form(&request.body), challenge),
    };
    let _ = socket.write_all(response.as_bytes()).await;
    let _ = socket.shutdown().await;
}

fn redirect_back(
    asked: &BTreeMap<String, String>,
    challenge: &Arc<Mutex<Option<String>>>,
) -> String {
    *challenge.lock().expect("lock") = asked.get("code_challenge").cloned();
    let back = format!(
        "{}?code={CODE}&state={}",
        asked["redirect_uri"], asked["state"]
    );
    format!(
        "HTTP/1.1 302 Found\r\nLocation: {back}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    )
}

fn pay_out(presented: &BTreeMap<String, String>, challenge: &Arc<Mutex<Option<String>>>) -> String {
    let verifier = presented.get("code_verifier").cloned().unwrap_or_default();
    let hashed = B64_URL.encode(Sha256::digest(verifier.as_bytes()));
    let matches = challenge
        .lock()
        .expect("lock")
        .as_deref()
        .is_some_and(|advertised| advertised == hashed);

    let body = match (presented.get("code").map(String::as_str), matches) {
        (Some(CODE), true) => {
            format!(
                r#"{{"access_token":"{PAID_OUT_FOR_A_MATCHING_VERIFIER}","refresh_token":"r","expires_in":3600}}"#
            )
        }
        _ => r#"{"error":"invalid_grant","error_description":"PKCE did not check out"}"#.into(),
    };
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

#[derive(Default)]
struct Request {
    target: String,
    body: String,
}

impl Request {
    fn query(&self) -> &str {
        self.target.split_once('?').map_or("", |(_, query)| query)
    }
}

async fn read_request(socket: &mut TcpStream) -> Request {
    let mut request = Vec::new();
    let mut buffer = [0u8; 4096];
    loop {
        let read = socket.read(&mut buffer).await.expect("request");
        request.extend_from_slice(&buffer[..read]);

        let text = String::from_utf8_lossy(&request).into_owned();
        if let Some((head, body)) = text.split_once("\r\n\r\n")
            && body.len() >= content_length(head)
        {
            return Request {
                target: target(head),
                body: body.to_string(),
            };
        }
        if read == 0 {
            return Request::default();
        }
    }
}

fn target(head: &str) -> String {
    head.lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or_default()
        .to_string()
}

fn content_length(head: &str) -> usize {
    head.lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse().ok())
        .unwrap_or(0)
}

fn form(text: &str) -> BTreeMap<String, String> {
    text.split('&')
        .filter_map(|pair| pair.split_once('='))
        .map(|(name, value)| {
            let value = urlencoding::decode(value).unwrap_or_default().into_owned();
            (name.to_string(), value)
        })
        .collect()
}
