use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use ratatui::crossterm::event;

use sst::app::{self, App};
use sst::auth::Flow;
use sst::config::{self, Config, Encryption, Profile, SignIn};
use sst::{external, ui};

#[derive(Parser)]
#[command(version, about = "A terminal query editor for Microsoft SQL Server")]
struct Args {
    #[arg(short, long, help = "Profile or saved account to open")]
    profile: Option<String>,

    #[arg(short = 'H', long, help = "Server host, optionally host,port")]
    host: Option<String>,

    #[arg(long, help = "Server port")]
    port: Option<u16>,

    #[arg(short, long, help = "Database to open")]
    database: Option<String>,

    #[arg(short, long, help = "SQL Server login")]
    username: Option<String>,

    #[arg(
        long,
        env = "SQLSERVER_PASSWORD",
        hide_env_values = true,
        help = "SQL Server password"
    )]
    password: Option<String>,

    #[arg(long, help = "Sign in with Azure Entra ID instead of a SQL login")]
    azure: bool,

    #[arg(long, value_enum, help = "How much of the connection to encrypt")]
    encrypt: Option<Encryption>,

    #[arg(long, conflicts_with = "encrypt", help = "Shorthand for --encrypt off")]
    no_tls: bool,

    #[arg(long, help = "Verify the server certificate instead of trusting it")]
    verify_cert: bool,

    #[arg(long, help = "Entra tenant id, with --azure")]
    tenant: Option<String>,

    #[arg(long, value_enum, help = "Sign-in flow for --azure")]
    flow: Option<Flow>,

    #[arg(
        long,
        help = "Command printing a bearer token, instead of signing in here"
    )]
    token_command: Option<String>,

    #[arg(
        long,
        default_value_t = 1000,
        help = "Rows shown in the grid per result set; exports are never capped"
    )]
    max_rows: usize,

    #[arg(long, help = "Print where the config and saved accounts are read from")]
    config_path: bool,

    #[arg(long, help = "Forget cached Azure tokens and exit")]
    logout: bool,
}

impl Args {
    fn connection(&self, config: &Config) -> Result<Option<Profile>> {
        let Some(host) = self.host.clone() else {
            return Ok(config
                .profile(self.profile.as_deref())?
                .map(|profile| self.apply_flags(profile)));
        };
        Ok(Some(Profile {
            host: Some(host),
            port: self.port,
            database: self.database.clone(),
            auth: Some(match self.azure {
                true => SignIn::Azure,
                false => SignIn::Sql,
            }),
            username: self.username.clone(),
            password: self.password.clone(),
            tenant: self.tenant.clone(),
            flow: self.flow,
            token_command: self.token_command.clone(),
            encrypt: self.encryption(),
            trust_certificate: (!self.verify_cert).then_some(true),
            ..Profile::default()
        }))
    }

    fn apply_flags(&self, mut profile: Profile) -> Profile {
        if self.database.is_some() {
            profile.database = self.database.clone();
        }
        if let Some(encryption) = self.encryption() {
            profile.encrypt = Some(encryption);
        }
        if self.verify_cert {
            profile.trust_certificate = Some(false);
        }
        if self.flow.is_some() {
            profile.flow = self.flow;
        }
        if self.token_command.is_some() {
            profile.token_command = self.token_command.clone();
        }
        profile
    }

    fn encryption(&self) -> Option<Encryption> {
        match self.no_tls {
            true => Some(Encryption::Off),
            false => self.encrypt,
        }
    }
}

fn main() -> Result<()> {
    config::adopt_previous_name();

    let args = Args::parse();
    if args.config_path {
        show_paths();
        return Ok(());
    }
    if args.logout {
        sst::auth::forget_tokens()?;
        println!("cached Azure tokens removed");
        return Ok(());
    }

    let config = config::load()?;
    let profile = args.connection(&config)?;
    edit(config, profile, args.max_rows)
}

fn show_paths() {
    match (config::path(), config::accounts_path()) {
        (Some(config), Some(accounts)) => {
            println!("{}", config.display());
            println!("{}", accounts.display());
        }
        _ => println!("(no config directory on this system)"),
    }
}

fn edit(config: Config, profile: Option<Profile>, max_rows: usize) -> Result<()> {
    let runtime = tokio::runtime::Runtime::new()?;
    let (outbox, mut inbox) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(config, max_rows, runtime.handle().clone(), outbox);
    app.start(profile);

    let mut terminal = external::enter()?;
    let outcome = run(&mut terminal, &mut app, &mut inbox);
    external::leave()?;
    outcome
}

fn run(
    terminal: &mut external::Tui,
    app: &mut App,
    inbox: &mut tokio::sync::mpsc::UnboundedReceiver<app::Msg>,
) -> Result<()> {
    while !app.quit {
        terminal.draw(|frame| ui::draw(frame, app))?;

        while let Ok(message) = inbox.try_recv() {
            app.handle(message);
        }

        if event::poll(Duration::from_millis(50))? {
            let event = event::read()?;
            app.on_key(terminal, event);
        }
    }
    Ok(())
}
