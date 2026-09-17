use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Deserializer, Serialize};
use tiberius::AuthMethod;

use crate::auth::Flow;

pub const DEFAULT_PORT: u16 = 1433;
pub const AZURE_CLI_CLIENT_ID: &str = "04b07795-8ddb-461a-bbee-02f9e1bf7b46";
pub const SQL_SCOPE: &str = "https://database.windows.net/.default";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
#[clap(rename_all = "kebab-case")]
pub enum Encryption {
    #[default]
    #[value(help = "TLS for the whole connection, failing if the server will not")]
    Required,
    #[value(help = "TLS for the whole connection where the server supports it")]
    On,
    #[value(help = "TLS for the login packet only, so the password is never in the clear")]
    LoginOnly,
    #[value(help = "No TLS at all, including the login packet")]
    Off,
}

impl Encryption {
    pub fn label(self) -> &'static str {
        match self {
            Encryption::Required => "required",
            Encryption::On => "on",
            Encryption::LoginOnly => "login-only",
            Encryption::Off => "off",
        }
    }

    pub fn next(self) -> Self {
        match self {
            Encryption::Required => Encryption::On,
            Encryption::On => Encryption::LoginOnly,
            Encryption::LoginOnly => Encryption::Off,
            Encryption::Off => Encryption::Required,
        }
    }
}

impl<'de> Deserialize<'de> for Encryption {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Written {
            Flag(bool),
            Name(String),
        }

        match Written::deserialize(deserializer)? {
            Written::Flag(true) => Ok(Encryption::Required),
            Written::Flag(false) => Ok(Encryption::Off),
            Written::Name(name) => match name.to_ascii_lowercase().replace('_', "-").as_str() {
                "required" => Ok(Encryption::Required),
                "on" | "true" => Ok(Encryption::On),
                "login-only" | "login" => Ok(Encryption::LoginOnly),
                "off" | "none" | "false" | "not-supported" => Ok(Encryption::Off),
                other => Err(serde::de::Error::custom(format!(
                    "unknown encryption {other:?} — use required, on, login-only or off"
                ))),
            },
        }
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct Config {
    pub default_profile: Option<String>,
    #[serde(default)]
    pub profiles: BTreeMap<String, Profile>,
    #[serde(skip)]
    pub saved: BTreeMap<String, Profile>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Profile {
    pub host: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth: Option<SignIn>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flow: Option<Flow>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encrypt: Option<Encryption>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trust_certificate: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_rows: Option<usize>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct Accounts {
    #[serde(default)]
    profiles: BTreeMap<String, Profile>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SignIn {
    Sql,
    Azure,
}

#[derive(Debug, Clone)]
pub enum Auth {
    Sql { username: String, password: String },
    Token(String),
}

#[derive(Debug, Clone)]
pub struct Credentials {
    pub host: String,
    pub port: u16,
    pub database: Option<String>,
    pub auth: Auth,
    pub encrypt: Encryption,
    pub trust_certificate: bool,
}

impl Credentials {
    pub fn auth_method(&self) -> AuthMethod {
        match &self.auth {
            Auth::Sql { username, password } => AuthMethod::sql_server(username, password),
            Auth::Token(token) => AuthMethod::AADToken(token.clone()),
        }
    }

    pub fn describe(&self) -> String {
        let who = match &self.auth {
            Auth::Sql { username, .. } => username.clone(),
            Auth::Token(_) => "azure".to_string(),
        };
        format!("{}@{}:{}", who, self.host, self.port)
    }
}

impl Profile {
    pub fn uses_azure(&self) -> bool {
        self.auth == Some(SignIn::Azure)
    }

    pub fn tenant(&self) -> String {
        self.tenant
            .clone()
            .unwrap_or_else(|| "organizations".into())
    }

    pub fn flow(&self) -> Flow {
        self.flow.unwrap_or(Flow::Browser)
    }

    pub fn client_id(&self) -> String {
        self.client_id
            .clone()
            .unwrap_or_else(|| AZURE_CLI_CLIENT_ID.to_string())
    }

    pub fn credentials(&self, token: Option<String>) -> Result<Credentials> {
        let host = self.host.clone().context("profile has no host")?;
        let auth = match (self.uses_azure(), token) {
            (true, Some(token)) => Auth::Token(token),
            (true, None) => bail!("this profile needs an Azure sign-in"),
            (false, _) => Auth::Sql {
                username: self.username.clone().context("profile has no username")?,
                password: self.password.clone().unwrap_or_default(),
            },
        };
        Ok(Credentials {
            host,
            port: self.port.unwrap_or(DEFAULT_PORT),
            database: self.database.clone(),
            auth,
            encrypt: self.encrypt.unwrap_or_default(),
            trust_certificate: self.trust_certificate.unwrap_or(true),
        })
    }
}

pub fn directory() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("sst"))
}

pub fn adopt_previous_name() {
    let moves = [
        (dirs::config_dir(), "sqlservertui", "sst"),
        (dirs::data_dir(), "sqlservertui", "sst"),
    ];
    for (base, from, to) in moves {
        let Some(base) = base else { continue };
        let (previous, current) = (base.join(from), base.join(to));
        if previous.is_dir() && !current.exists() {
            let _ = std::fs::rename(&previous, &current);
        }
    }
}

pub fn path() -> Option<PathBuf> {
    Some(directory()?.join("config.toml"))
}

pub fn accounts_path() -> Option<PathBuf> {
    Some(directory()?.join("accounts.toml"))
}

pub fn load() -> Result<Config> {
    let mut config: Config = match path().filter(|path| path.exists()) {
        Some(path) => {
            let text = read(&path)?;
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?
        }
        None => Config::default(),
    };
    config.saved = load_accounts()?;
    Ok(config)
}

fn load_accounts() -> Result<BTreeMap<String, Profile>> {
    let Some(path) = accounts_path().filter(|path| path.exists()) else {
        return Ok(BTreeMap::new());
    };
    let accounts: Accounts =
        toml::from_str(&read(&path)?).with_context(|| format!("parsing {}", path.display()))?;
    Ok(accounts.profiles)
}

fn read(path: &Path) -> Result<String> {
    std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))
}

pub fn save_account(name: &str, profile: &Profile) -> Result<PathBuf> {
    let mut profiles = load_accounts()?;
    profiles.insert(name.to_string(), profile.clone());
    write_accounts(profiles)
}

pub fn forget_account(name: &str) -> Result<PathBuf> {
    let mut profiles = load_accounts()?;
    if profiles.remove(name).is_none() {
        bail!("{name:?} is not a saved account");
    }
    write_accounts(profiles)
}

fn write_accounts(profiles: BTreeMap<String, Profile>) -> Result<PathBuf> {
    let path = accounts_path().context("no config directory on this system")?;
    write_private(&path, &toml::to_string_pretty(&Accounts { profiles })?)?;
    Ok(path)
}

pub fn write_private(path: &Path, contents: &str) -> Result<()> {
    let directory = path.parent().context("path has no parent directory")?;
    std::fs::create_dir_all(directory)
        .with_context(|| format!("creating {}", directory.display()))?;
    restrict(directory, 0o700)?;

    let temporary = path.with_extension("new");
    let _ = std::fs::remove_file(&temporary);
    create_private(&temporary)?
        .write_all(contents.as_bytes())
        .with_context(|| format!("writing {}", temporary.display()))?;
    std::fs::rename(&temporary, path).with_context(|| format!("replacing {}", path.display()))
}

#[cfg(unix)]
fn create_private(path: &Path) -> Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("creating {}", path.display()))
}

#[cfg(not(unix))]
fn create_private(path: &Path) -> Result<std::fs::File> {
    std::fs::File::create(path).with_context(|| format!("creating {}", path.display()))
}

#[cfg(unix)]
fn restrict(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .with_context(|| format!("restricting {}", path.display()))
}

#[cfg(not(unix))]
fn restrict(_path: &Path, _mode: u32) -> Result<()> {
    Ok(())
}

impl Config {
    pub fn profile(&self, name: Option<&str>) -> Result<Option<Profile>> {
        match name.or(self.default_profile.as_deref()) {
            None => Ok(None),
            Some(name) => self
                .lookup(name)
                .cloned()
                .map(Some)
                .with_context(|| format!("no profile or saved account named {name:?}")),
        }
    }

    pub fn lookup(&self, name: &str) -> Option<&Profile> {
        self.profiles.get(name).or_else(|| self.saved.get(name))
    }

    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .profiles
            .keys()
            .chain(self.saved.keys())
            .cloned()
            .collect();
        names.sort();
        names.dedup();
        names
    }

    pub fn is_saved(&self, name: &str) -> bool {
        self.saved.contains_key(name) && !self.profiles.contains_key(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Deserialize)]
    struct Holder {
        encrypt: Encryption,
    }

    fn parse(toml_text: &str) -> Encryption {
        toml::from_str::<Holder>(toml_text).expect("parses").encrypt
    }

    #[test]
    fn reads_every_spelling_of_the_encryption_setting() {
        assert_eq!(parse(r#"encrypt = "required""#), Encryption::Required);
        assert_eq!(parse(r#"encrypt = "on""#), Encryption::On);
        assert_eq!(parse(r#"encrypt = "login-only""#), Encryption::LoginOnly);
        assert_eq!(parse(r#"encrypt = "login""#), Encryption::LoginOnly);
        assert_eq!(parse(r#"encrypt = "off""#), Encryption::Off);
        assert_eq!(parse(r#"encrypt = "none""#), Encryption::Off);
        assert_eq!(parse(r#"encrypt = "LOGIN_ONLY""#), Encryption::LoginOnly);
    }

    #[test]
    fn still_reads_the_older_boolean_shape() {
        assert_eq!(parse("encrypt = true"), Encryption::Required);
        assert_eq!(parse("encrypt = false"), Encryption::Off);
    }

    #[test]
    fn rejects_a_spelling_it_does_not_know() {
        let error = toml::from_str::<Holder>(r#"encrypt = "maybe""#)
            .unwrap_err()
            .to_string();
        assert!(error.contains("maybe"), "{error}");
    }

    #[test]
    fn reads_the_sign_in_a_profile_asks_for() {
        let profile: Profile =
            toml::from_str("host = \"db\"\nauth = \"azure\"\nflow = \"device\"").expect("parses");
        assert!(profile.uses_azure());
        assert_eq!(profile.flow(), Flow::DeviceCode, "the older spelling");

        let login: Profile = toml::from_str("host = \"db\"\nauth = \"sql\"").expect("parses");
        assert!(!login.uses_azure());
        assert_eq!(
            login.flow(),
            Flow::Browser,
            "unless a profile asks otherwise"
        );
    }

    #[test]
    fn a_saved_account_is_written_back_the_way_it_is_read() {
        let profile = Profile {
            host: Some("db".into()),
            auth: Some(SignIn::Azure),
            flow: Some(Flow::DeviceCode),
            ..Profile::default()
        };
        let written = toml::to_string(&profile).expect("serialises");
        assert!(written.contains("auth = \"azure\""), "{written}");
        assert!(written.contains("flow = \"device-code\""), "{written}");
    }

    #[test]
    fn encryption_defaults_to_the_safe_end() {
        assert_eq!(Encryption::default(), Encryption::Required);
        assert!(
            Profile::default().credentials(None).is_err(),
            "a profile with no host cannot make credentials"
        );
    }

    #[test]
    fn cycling_the_toggle_visits_every_level_and_returns() {
        let mut seen = vec![Encryption::Required];
        let mut level = Encryption::Required;
        for _ in 0..3 {
            level = level.next();
            seen.push(level);
        }
        assert_eq!(
            seen,
            [
                Encryption::Required,
                Encryption::On,
                Encryption::LoginOnly,
                Encryption::Off
            ]
        );
        assert_eq!(level.next(), Encryption::Required, "wraps round");
    }
}
