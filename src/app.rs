use std::path::{Path, PathBuf};
use std::sync::Arc;

use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use tokio::runtime::Handle;
use tokio::sync::Mutex;
use tokio::sync::mpsc::UnboundedSender;
use tui_textarea::{CursorMove, TextArea};

use crate::auth;
use crate::complete::{self, Candidate};
use crate::config::{self, Config, Credentials, Encryption, Profile, SignIn};
use crate::db::{self, Client, ResultSet, Table};
use crate::export;
use crate::external::{self, Tui};
use crate::queries::{self, Library};

const NEW_CONNECTION: &str = "New connection…";
const DEFAULT_DATABASE: &str = "default";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Host,
    Port,
    Database,
    Username,
    Password,
    Tenant,
    Name,
}

const ALL_FIELDS: [Field; 7] = [
    Field::Host,
    Field::Port,
    Field::Database,
    Field::Username,
    Field::Password,
    Field::Tenant,
    Field::Name,
];
const SQL_FIELDS: [Field; 5] = [
    Field::Host,
    Field::Port,
    Field::Database,
    Field::Username,
    Field::Password,
];
const AZURE_FIELDS: [Field; 4] = [Field::Host, Field::Port, Field::Database, Field::Tenant];

impl Field {
    pub fn label(self) -> &'static str {
        match self {
            Field::Host => "Host",
            Field::Port => "Port",
            Field::Database => "Database",
            Field::Username => "Username",
            Field::Password => "Password",
            Field::Tenant => "Tenant (optional: GUID or domain, blank = your own)",
            Field::Name => "Save as (optional profile name)",
        }
    }
}

pub enum Msg {
    Connected {
        client: Arc<Mutex<Client>>,
        description: String,
        encryption: Encryption,
        database: Option<String>,
        server_root: Option<PathBuf>,
    },
    Schema(Vec<Table>),
    Databases(Vec<String>),
    Results {
        request: u64,
        sql: String,
        results: Vec<ResultSet>,
        elapsed: std::time::Duration,
    },
    DeviceCode(auth::Prompt),
    Note(String),
    Failed(String),
}

#[derive(PartialEq, Eq, Clone, Copy)]
pub enum Focus {
    Library,
    Query,
    Results,
}

pub enum Modal {
    None,
    Help,
    Picker(Picker),
    Connect(Box<Connect>),
    DeviceCode(auth::Prompt),
    Prompt {
        purpose: Asking,
        field: Box<TextArea<'static>>,
    },
    Cell {
        title: String,
        body: String,
        scroll: usize,
    },
    Confirm {
        prompt: String,
        action: Pending,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Asking {
    ExportPath,
    QueryName,
}

pub enum Pending {
    Forget { name: String },
    Export { path: String },
    DeleteQuery { path: PathBuf },
}

pub struct Picker {
    pub title: String,
    pub choices: Vec<Choice>,
    pub selected: usize,
    pub stage: Stage,
}

pub struct Choice {
    pub label: String,
    pub detail: String,
}

impl Choice {
    fn new(label: impl Into<String>) -> Self {
        Choice {
            label: label.into(),
            detail: String::new(),
        }
    }

    fn detailed(label: impl Into<String>, detail: impl Into<String>) -> Self {
        Choice {
            label: label.into(),
            detail: detail.into(),
        }
    }
}

#[derive(Clone)]
pub enum Stage {
    Account { profiles: usize },
    Database,
}

pub struct Connect {
    fields: Vec<TextArea<'static>>,
    pub active: Field,
    pub azure: bool,
    pub encrypt: Encryption,
    pub trust_certificate: bool,
    pub editing: Option<String>,
}

impl Default for Connect {
    fn default() -> Self {
        let mut form = Connect {
            fields: ALL_FIELDS.iter().map(|_| TextArea::default()).collect(),
            active: Field::Host,
            azure: false,
            encrypt: Encryption::default(),
            trust_certificate: true,
            editing: None,
        };
        form.field_mut(Field::Port)
            .insert_str(config::DEFAULT_PORT.to_string());
        form.field_mut(Field::Password).set_mask_char('•');
        form
    }
}

impl Connect {
    pub fn editing(name: &str, profile: &Profile) -> Self {
        let mut form = Connect {
            azure: profile.uses_azure(),
            encrypt: profile.encrypt.unwrap_or_default(),
            trust_certificate: profile.trust_certificate.unwrap_or(true),
            editing: Some(name.to_string()),
            ..Connect::default()
        };

        let written = [
            (Field::Host, profile.host.clone()),
            (
                Field::Port,
                Some(profile.port.unwrap_or(config::DEFAULT_PORT).to_string()),
            ),
            (Field::Database, profile.database.clone()),
            (Field::Username, profile.username.clone()),
            (Field::Password, profile.password.clone()),
            (Field::Tenant, profile.tenant.clone()),
            (Field::Name, Some(name.to_string())),
        ];
        for (which, value) in written {
            let field = form.field_mut(which);
            field.select_all();
            field.cut();
            field.insert_str(value.unwrap_or_default());
        }
        form.active = form.visible()[0];
        form
    }

    pub fn visible(&self) -> Vec<Field> {
        let mut rows = match self.azure {
            true => AZURE_FIELDS.to_vec(),
            false => SQL_FIELDS.to_vec(),
        };
        rows.push(Field::Name);
        rows
    }

    pub fn field(&self, which: Field) -> &TextArea<'static> {
        &self.fields[which as usize]
    }

    pub fn field_mut(&mut self, which: Field) -> &mut TextArea<'static> {
        &mut self.fields[which as usize]
    }

    fn value(&self, which: Field) -> String {
        self.field(which).lines().join("").trim().to_string()
    }

    fn step(&mut self, forward: bool) {
        let rows = self.visible();
        let at = rows.iter().position(|row| *row == self.active).unwrap_or(0);
        let next = match forward {
            true => (at + 1) % rows.len(),
            false => (at + rows.len() - 1) % rows.len(),
        };
        self.active = rows[next];
    }

    fn profile(&self) -> anyhow::Result<Profile> {
        let port = match self.value(Field::Port) {
            text if text.is_empty() => None,
            text => Some(
                text.parse()
                    .map_err(|_| anyhow::anyhow!("{text:?} is not a port number"))?,
            ),
        };
        Ok(Profile {
            host: Some(self.value(Field::Host)),
            port,
            database: Some(self.value(Field::Database)).filter(|value| !value.is_empty()),
            auth: Some(match self.azure {
                true => SignIn::Azure,
                false => SignIn::Sql,
            }),
            username: Some(self.value(Field::Username)).filter(|value| !value.is_empty()),
            password: Some(self.value(Field::Password)).filter(|value| !value.is_empty()),
            tenant: Some(self.value(Field::Tenant)).filter(|value| !value.is_empty()),
            encrypt: Some(self.encrypt),
            trust_certificate: Some(self.trust_certificate),
            ..Profile::default()
        })
    }
}

pub struct Completion {
    pub items: Vec<Candidate>,
    pub selected: usize,
    pub start: usize,
}

pub struct App {
    pub client: Option<Arc<Mutex<Client>>>,
    pub config: Config,
    pub runtime: Handle,
    pub outbox: UnboundedSender<Msg>,

    pub connection: Option<String>,
    pub library: Library,
    pub library_selected: usize,
    pub sidebar: bool,
    server_root: Option<PathBuf>,
    pub encryption: Encryption,
    pub database: Option<String>,
    pub tables: Vec<Table>,
    pub max_rows: usize,

    pub query: TextArea<'static>,
    pub focus: Focus,
    pub completion: Option<Completion>,

    pub results: Vec<ResultSet>,
    pub active_result: usize,
    pub row: usize,
    pub column: usize,
    pub column_offset: usize,

    pub modal: Modal,
    pub status: String,
    pub failed: bool,
    pub busy: bool,
    pub request: u64,
    pub quit: bool,
    ran_sql: Option<String>,
    pending_save: Option<(String, Profile)>,
}

impl App {
    pub fn new(
        config: Config,
        max_rows: usize,
        runtime: Handle,
        outbox: UnboundedSender<Msg>,
    ) -> Self {
        App {
            client: None,
            config,
            runtime,
            outbox,
            connection: None,
            library: Library::default(),
            library_selected: 0,
            sidebar: true,
            server_root: None,
            encryption: Encryption::default(),
            database: None,
            tables: Vec::new(),
            max_rows,
            query: editor(""),
            focus: Focus::Query,
            completion: None,
            results: Vec::new(),
            active_result: 0,
            row: 0,
            column: 0,
            column_offset: 0,
            modal: Modal::None,
            status: String::new(),
            failed: false,
            busy: false,
            request: 0,
            quit: false,
            ran_sql: None,
            pending_save: None,
        }
    }

    pub fn start(&mut self, profile: Option<Profile>) {
        match profile {
            Some(profile) => self.open(profile, None),
            None => self.pick_account(),
        }
    }

    pub fn result(&self) -> Option<&ResultSet> {
        self.results.get(self.active_result)
    }

    pub fn handle(&mut self, message: Msg) {
        let succeeded = !matches!(message, Msg::Failed(_));
        match message {
            Msg::Connected {
                client,
                description,
                encryption,
                database,
                server_root,
            } => self.connected(client, description, encryption, database, server_root),
            Msg::Schema(tables) => self.cache_schema(tables),
            Msg::Databases(databases) => self.offer_databases(databases),
            Msg::Results {
                request,
                sql,
                results,
                elapsed,
            } => self.show_results(request, sql, results, elapsed),
            Msg::DeviceCode(prompt) => self.modal = Modal::DeviceCode(prompt),
            Msg::Note(note) => {
                self.busy = false;
                self.note(note);
            }
            Msg::Failed(error) => self.failed(error),
        }
        if succeeded {
            self.commit_save();
        }
    }

    fn connected(
        &mut self,
        client: Arc<Mutex<Client>>,
        description: String,
        encryption: Encryption,
        database: Option<String>,
        server_root: Option<PathBuf>,
    ) {
        self.busy = false;
        self.client = Some(client);
        self.connection = Some(description.clone());
        self.encryption = encryption;
        self.database = database;
        self.server_root = server_root;
        self.modal = Modal::None;
        self.results.clear();
        self.reload_library();
        self.note(format!("connected as {description}"));
        self.load_schema();
    }

    fn cache_schema(&mut self, tables: Vec<Table>) {
        self.busy = false;
        let columns: usize = tables.iter().map(|table| table.columns.len()).sum();
        self.note(format!(
            "{} tables · {columns} columns cached for completion",
            tables.len()
        ));
        self.tables = tables;
    }

    fn offer_databases(&mut self, databases: Vec<String>) {
        self.busy = false;
        self.modal = Modal::Picker(Picker {
            title: "Database".into(),
            choices: databases.into_iter().map(Choice::new).collect(),
            selected: 0,
            stage: Stage::Database,
        });
    }

    fn show_results(
        &mut self,
        request: u64,
        sql: String,
        results: Vec<ResultSet>,
        elapsed: std::time::Duration,
    ) {
        if self.superseded(request) {
            return;
        }
        self.busy = false;
        self.active_result = 0;
        self.reset_cursor();
        self.ran_sql = Some(sql);
        self.reload_library();
        let summary = describe(&results, elapsed);
        self.results = results;
        self.focus = Focus::Results;
        self.note(summary);
    }

    fn superseded(&self, request: u64) -> bool {
        request != self.request
    }

    fn failed(&mut self, error: String) {
        self.busy = false;
        self.pending_save = None;
        self.fail(error);
        if self.client.is_none() && matches!(self.modal, Modal::None | Modal::DeviceCode(_)) {
            self.pick_account();
        }
    }

    pub fn on_key(&mut self, terminal: &mut Tui, event: Event) {
        let Event::Key(key) = event else { return };
        if key.kind != KeyEventKind::Press {
            return;
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.quit = true;
            return;
        }
        if matches!(self.modal, Modal::None) && is_run(&key) {
            self.run();
            return;
        }

        match &mut self.modal {
            Modal::None => match self.focus {
                Focus::Library => self.on_library_key(key),
                Focus::Query => self.on_query_key(terminal, key),
                Focus::Results => self.on_results_key(terminal, key),
            },
            Modal::Help => self.modal = Modal::None,
            Modal::DeviceCode(_) => {
                if key.code == KeyCode::Esc {
                    self.modal = Modal::None;
                }
            }
            Modal::Cell { .. } => self.on_cell_key(key),
            Modal::Confirm { .. } => self.on_confirm_key(key),
            Modal::Picker(_) => self.on_picker_key(key),
            Modal::Connect(_) => self.on_connect_key(key),
            Modal::Prompt { .. } => self.on_prompt_key(key),
        }
    }

    fn on_library_key(&mut self, key: KeyEvent) {
        let last = self.library.entries().len().saturating_sub(1);
        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('?') | KeyCode::F(1) => self.modal = Modal::Help,
            KeyCode::Char('j') | KeyCode::Down => {
                self.library_selected = (self.library_selected + 1).min(last)
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.library_selected = self.library_selected.saturating_sub(1)
            }
            KeyCode::Char('g') | KeyCode::Home => self.library_selected = 0,
            KeyCode::Char('G') | KeyCode::End => self.library_selected = last,
            KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right => self.load_query(),
            KeyCode::Char('s') => self.begin_save_query(),
            KeyCode::Char('d') => self.confirm_delete_query(),
            KeyCode::Char('r') => self.reload_library(),
            KeyCode::Tab | KeyCode::Esc => self.focus = Focus::Query,
            KeyCode::Char('A') => self.pick_account(),
            _ => {}
        }
    }

    fn load_query(&mut self) {
        let Some(entry) = self.library.get(self.library_selected) else {
            return;
        };
        self.query = editor(&entry.sql);
        self.focus = Focus::Query;
        self.note(format!("loaded {}", entry.label));
    }

    fn begin_save_query(&mut self) {
        if self.library_root().is_none() {
            self.fail("connect first — queries are saved per server and database".into());
            return;
        }
        if self.query.lines().join("").trim().is_empty() {
            self.fail("nothing to save".into());
            return;
        }
        self.modal = Modal::Prompt {
            purpose: Asking::QueryName,
            field: Box::new(TextArea::default()),
        };
    }

    fn save_query(&mut self, name: &str) {
        let Some(root) = self.library_root() else {
            return;
        };
        let sql = self.query.lines().join("\n");
        match queries::save(&root, name, &sql) {
            Ok(path) => {
                self.modal = Modal::None;
                self.note(format!("saved to {}", path.display()));
                self.reload_library();
            }
            Err(error) => self.fail(error.to_string()),
        }
    }

    fn confirm_delete_query(&mut self) {
        let Some(entry) = self.library.get(self.library_selected) else {
            return;
        };
        if !entry.saved {
            self.fail("that is history — it ages out on its own".into());
            return;
        }
        self.modal = Modal::Confirm {
            prompt: format!("Delete saved query {}?", entry.label),
            action: Pending::DeleteQuery {
                path: entry.path.clone(),
            },
        };
    }

    fn delete_query(&mut self, path: &Path) {
        match queries::forget(path) {
            Ok(()) => self.note("deleted".into()),
            Err(error) => self.fail(error.to_string()),
        }
        self.reload_library();
    }

    fn library_root(&self) -> Option<PathBuf> {
        let server = self.server_root.as_ref()?;
        let database = self.database.as_deref().unwrap_or(DEFAULT_DATABASE);
        Some(queries::database_root(server, database))
    }

    fn reload_library(&mut self) {
        self.library = queries::load(self.library_root());
        let last = self.library.entries().len().saturating_sub(1);
        self.library_selected = self.library_selected.min(last);
    }

    fn on_query_key(&mut self, terminal: &mut Tui, key: KeyEvent) {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);

        if self.completion.is_some() {
            match key.code {
                KeyCode::Esc => {
                    self.completion = None;
                    return;
                }
                KeyCode::Tab | KeyCode::Enter => return self.accept_completion(),
                KeyCode::Down => return self.move_completion(1),
                KeyCode::Up => return self.move_completion(-1),
                _ => {}
            }
        }

        match key.code {
            KeyCode::Tab => return self.open_completion(),
            KeyCode::Char(' ') if control => return self.open_completion(),
            KeyCode::Char('e') if control => return self.edit_query(terminal),
            KeyCode::Char('j') if control => {
                self.query.insert_newline();
                return;
            }
            KeyCode::Esc => {
                self.focus = Focus::Results;
                return;
            }
            KeyCode::Char('b') if control => {
                self.toggle_sidebar();
                return;
            }
            KeyCode::BackTab => {
                if self.sidebar {
                    self.focus = Focus::Library;
                }
                return;
            }
            KeyCode::Char('s') if control => return self.begin_save_query(),
            _ => {}
        }

        self.query.input(Event::Key(key));
        if self.completion.is_some() {
            self.open_completion();
        }
    }

    fn on_results_key(&mut self, terminal: &mut Tui, key: KeyEvent) {
        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('?') | KeyCode::F(1) => self.modal = Modal::Help,
            KeyCode::Char('i') | KeyCode::Char('/') => self.focus = Focus::Query,
            KeyCode::Tab | KeyCode::BackTab => {
                self.focus = match self.sidebar {
                    true => Focus::Library,
                    false => Focus::Query,
                }
            }
            KeyCode::Char('b') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.toggle_sidebar()
            }
            KeyCode::Char('S') => self.begin_save_query(),
            KeyCode::Char('j') | KeyCode::Down => self.move_row(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_row(-1),
            KeyCode::PageDown => self.move_row(20),
            KeyCode::PageUp => self.move_row(-20),
            KeyCode::Char('g') | KeyCode::Home => self.row = 0,
            KeyCode::Char('G') | KeyCode::End => self.row = self.last_row(),
            KeyCode::Char('l') | KeyCode::Right => self.move_column(1),
            KeyCode::Char('h') | KeyCode::Left => self.move_column(-1),
            KeyCode::Char('0') | KeyCode::Char('^') => self.column = 0,
            KeyCode::Char('$') => self.column = self.last_column(),
            KeyCode::Char(']') => self.show_result(self.active_result + 1),
            KeyCode::Char('[') => self.show_result(self.active_result.saturating_sub(1)),
            KeyCode::Enter => self.view_cell(),
            KeyCode::Char('x') => self.view_row(),
            KeyCode::Char('V') => self.page_cell(terminal),
            KeyCode::Char('e') => self.begin_export(),
            KeyCode::Char('d') => self.pick_database(),
            KeyCode::Char('A') => self.pick_account(),
            _ => {}
        }
    }

    fn last_row(&self) -> usize {
        self.result()
            .map_or(0, |result| result.rows.len())
            .saturating_sub(1)
    }

    fn last_column(&self) -> usize {
        self.result()
            .map_or(0, |result| result.columns.len())
            .saturating_sub(1)
    }

    fn move_row(&mut self, by: isize) {
        self.row = self.row.saturating_add_signed(by).min(self.last_row());
    }

    fn move_column(&mut self, by: isize) {
        self.column = self
            .column
            .saturating_add_signed(by)
            .min(self.last_column());
    }

    fn show_result(&mut self, index: usize) {
        self.active_result = index.min(self.results.len().saturating_sub(1));
        self.reset_cursor();
    }

    fn on_cell_key(&mut self, key: KeyEvent) {
        let Modal::Cell { body, scroll, .. } = &mut self.modal else {
            return;
        };
        let last = body.lines().count().saturating_sub(1);
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => self.modal = Modal::None,
            KeyCode::Char('j') | KeyCode::Down => *scroll = (*scroll + 1).min(last),
            KeyCode::Char('k') | KeyCode::Up => *scroll = scroll.saturating_sub(1),
            _ => {}
        }
    }

    fn on_confirm_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                let Modal::Confirm { action, .. } = std::mem::replace(&mut self.modal, Modal::None)
                else {
                    return;
                };
                match action {
                    Pending::Forget { name } => self.forget(name),
                    Pending::Export { path } => self.export_now(&path),
                    Pending::DeleteQuery { path } => self.delete_query(&path),
                }
            }
            _ => self.modal = Modal::None,
        }
    }

    fn on_picker_key(&mut self, key: KeyEvent) {
        let Modal::Picker(picker) = &mut self.modal else {
            return;
        };
        let last = picker.choices.len().saturating_sub(1);
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.modal = Modal::None;
                if self.client.is_none() {
                    self.quit = true;
                }
            }
            KeyCode::Char('j') | KeyCode::Down => picker.selected = (picker.selected + 1).min(last),
            KeyCode::Char('k') | KeyCode::Up => picker.selected = picker.selected.saturating_sub(1),
            KeyCode::Char('g') | KeyCode::Home => picker.selected = 0,
            KeyCode::Char('G') | KeyCode::End => picker.selected = last,
            KeyCode::Char('d') => self.confirm_forget(),
            KeyCode::Char('e') => self.edit_account(),
            KeyCode::Char('A') => self.pick_account(),
            KeyCode::Enter => {
                let Some(chosen) = picker.choices.get(picker.selected) else {
                    return;
                };
                let name = chosen.label.clone();
                let (selected, stage) = (picker.selected, picker.stage.clone());
                match stage {
                    Stage::Account { profiles } => self.choose_account(selected, profiles, name),
                    Stage::Database => self.use_database(name),
                }
            }
            _ => {}
        }
    }

    fn on_connect_key(&mut self, key: KeyEvent) {
        let Modal::Connect(form) = &mut self.modal else {
            return;
        };
        match key.code {
            KeyCode::Esc => self.pick_account(),
            KeyCode::Char('t') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                form.azure = !form.azure;
                form.active = form.visible()[0];
            }
            KeyCode::Char('l') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                form.encrypt = form.encrypt.next();
            }
            KeyCode::Char('k') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                form.trust_certificate = !form.trust_certificate;
            }
            KeyCode::Tab | KeyCode::Down => form.step(true),
            KeyCode::BackTab | KeyCode::Up => form.step(false),
            KeyCode::Enter => {
                let (profile, name) = (form.profile(), form.value(Field::Name));
                match profile {
                    Ok(profile) => self.submit_connection(profile, &name),
                    Err(error) => self.fail(error.to_string()),
                }
            }
            _ => {
                let active = form.active;
                form.field_mut(active).input(Event::Key(key));
            }
        }
    }

    fn on_prompt_key(&mut self, key: KeyEvent) {
        let Modal::Prompt { purpose, field } = &mut self.modal else {
            return;
        };
        let purpose = *purpose;
        match key.code {
            KeyCode::Esc => self.modal = Modal::None,
            KeyCode::Enter => {
                let answer = field.lines().join("").trim().to_string();
                match purpose {
                    Asking::ExportPath => self.export_to(&answer),
                    Asking::QueryName => self.save_query(&answer),
                }
            }
            _ => {
                field.input(Event::Key(key));
            }
        }
    }

    fn open_completion(&mut self) {
        if self.tables.is_empty() {
            self.note("no schema cached yet".into());
            return;
        }
        let (line_index, cursor) = self.query.cursor();
        let lines = self.query.lines();
        let line = lines.get(line_index).cloned().unwrap_or_default();
        let buffer = lines.join("\n");
        let byte = char_to_byte(&line, cursor);

        let items = complete::candidates(&self.tables, &buffer, &line, byte);
        let start = complete::word_at(&line, byte).start_byte;
        self.completion = (!items.is_empty()).then_some(Completion {
            items,
            selected: 0,
            start: byte_to_char(&line, start),
        });
    }

    fn move_completion(&mut self, delta: isize) {
        let Some(completion) = &mut self.completion else {
            return;
        };
        let last = completion.items.len() as isize - 1;
        completion.selected = (completion.selected as isize + delta).clamp(0, last) as usize;
    }

    fn accept_completion(&mut self) {
        let Some(completion) = self.completion.take() else {
            return;
        };
        let Some(candidate) = completion.items.get(completion.selected).cloned() else {
            return;
        };

        let (line, cursor) = self.query.cursor();
        self.query
            .move_cursor(CursorMove::Jump(line as u16, completion.start as u16));
        for _ in completion.start..cursor {
            self.query.delete_next_char();
        }
        self.query.insert_str(&candidate.text);
    }

    fn run(&mut self) {
        let sql = self.query.lines().join("\n");
        if sql.trim().is_empty() {
            self.note("nothing to run".into());
            return;
        }
        let Some(client) = self.client.clone() else {
            self.fail("not connected — press A".into());
            return;
        };

        self.request += 1;
        let request = self.request;
        let limit = self.max_rows;
        self.completion = None;
        if let Some(root) = self.library_root()
            && let Err(error) = queries::remember(&root, &sql)
        {
            self.fail(format!("could not record the query: {error}"));
        }
        self.spawn(async move {
            let started = std::time::Instant::now();
            let results = db::run_query(&mut *client.lock().await, &sql, limit).await?;
            let sql = sql.clone();
            Ok(Msg::Results {
                request,
                sql,
                results,
                elapsed: started.elapsed(),
            })
        });
    }

    fn load_schema(&mut self) {
        let Some(client) = self.client.clone() else {
            return;
        };
        self.spawn(async move { Ok(Msg::Schema(db::tables(&mut *client.lock().await).await?)) });
    }

    fn pick_database(&mut self) {
        let Some(client) = self.client.clone() else {
            return self.fail("not connected — press A".into());
        };
        self.spawn(async move {
            Ok(Msg::Databases(
                db::databases(&mut *client.lock().await).await?,
            ))
        });
    }

    fn use_database(&mut self, database: String) {
        let Some(client) = self.client.clone() else {
            return;
        };
        self.database = Some(database.clone());
        self.modal = Modal::None;
        self.reload_library();
        let outbox = self.outbox.clone();
        self.busy = true;
        self.runtime.spawn(async move {
            let mut guard = client.lock().await;
            let switched = guard
                .simple_query(format!("USE [{database}]"))
                .await
                .map(drop);
            let message = match switched {
                Ok(()) => match db::tables(&mut guard).await {
                    Ok(tables) => Msg::Schema(tables),
                    Err(error) => Msg::Failed(error.to_string()),
                },
                Err(error) => Msg::Failed(error.to_string()),
            };
            let _ = outbox.send(message);
        });
    }

    fn begin_export(&mut self) {
        if self.ran_sql.is_none() {
            self.fail("nothing has been run yet".into());
            return;
        }
        let mut field = TextArea::default();
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs())
            .unwrap_or(0);
        field.insert_str(format!("~/query-{stamp}.csv"));
        self.modal = Modal::Prompt {
            purpose: Asking::ExportPath,
            field: Box::new(field),
        };
    }

    fn export_to(&mut self, path: &str) {
        let Some(sql) = self.ran_sql.clone() else {
            self.fail("nothing has been run yet".into());
            return;
        };
        if export::is_read_only(&sql) {
            self.export_now(path);
            return;
        }
        self.modal = Modal::Confirm {
            prompt: "This query changes data and exporting runs it again. Continue?".into(),
            action: Pending::Export {
                path: path.to_string(),
            },
        };
    }

    fn export_now(&mut self, path: &str) {
        let (Some(sql), Some(client)) = (self.ran_sql.clone(), self.client.clone()) else {
            return;
        };
        let destination = export::resolve(path);
        let wanted = self.active_result;

        self.modal = Modal::None;
        self.note(format!("writing {}…", destination.display()));
        self.spawn(async move {
            let mut writer = export::Writer::create(&destination)?;
            let rows =
                db::stream_to_file(&mut *client.lock().await, &sql, wanted, &mut writer).await?;
            writer.finish(&destination)?;
            Ok(Msg::Note(format!(
                "wrote {rows} rows to {}",
                destination.display()
            )))
        });
    }

    fn selected_cell(&self) -> Option<String> {
        let cell = self.result()?.rows.get(self.row)?.get(self.column)?;
        Some(cell.clone().unwrap_or_else(|| "NULL".into()))
    }

    fn view_cell(&mut self) {
        let Some(body) = self.selected_cell() else {
            return;
        };
        let title = self
            .result()
            .and_then(|result| result.columns.get(self.column))
            .cloned()
            .unwrap_or_default();
        self.modal = Modal::Cell {
            title,
            body,
            scroll: 0,
        };
    }

    fn view_row(&mut self) {
        let (Some(result), row) = (self.result(), self.row) else {
            return;
        };
        let Some(cells) = result.rows.get(row) else {
            return;
        };
        let label = result
            .columns
            .iter()
            .map(|name| name.chars().count())
            .max()
            .unwrap_or(0);

        let body = result
            .columns
            .iter()
            .zip(&result.types)
            .zip(cells)
            .map(|((name, kind), cell)| {
                format!(
                    "{name:label$}  {:<14}  {}",
                    kind,
                    cell.as_deref().unwrap_or("NULL")
                )
            })
            .collect::<Vec<_>>()
            .join("\n");

        self.modal = Modal::Cell {
            title: format!("Row {} of {}", row + 1, result.rows.len()),
            body,
            scroll: 0,
        };
    }

    fn page_cell(&mut self, terminal: &mut Tui) {
        let Some(body) = self.selected_cell() else {
            return;
        };
        if let Err(error) = external::page(terminal, &body, "txt") {
            self.fail(error.to_string());
        }
    }

    fn edit_query(&mut self, terminal: &mut Tui) {
        let current = self.query.lines().join("\n");
        match external::edit(terminal, &current, "sql") {
            Ok(Some(edited)) => {
                self.query = editor(edited.trim_end());
                self.run();
            }
            Ok(None) => {}
            Err(error) => self.fail(error.to_string()),
        }
    }

    fn pick_account(&mut self) {
        let names = self.config.names();
        let profiles = names.len();
        let mut choices: Vec<Choice> = names
            .iter()
            .map(|name| Choice::detailed(name, self.describe_account(name)))
            .collect();
        choices.push(Choice::new(NEW_CONNECTION));

        self.modal = Modal::Picker(Picker {
            title: "Account".into(),
            choices,
            selected: 0,
            stage: Stage::Account { profiles },
        });
    }

    fn describe_account(&self, name: &str) -> String {
        let profile = self.config.lookup(name);
        let auth = match profile.is_some_and(Profile::uses_azure) {
            true => "azure",
            false => "sql",
        };
        let host = profile
            .and_then(|profile| profile.host.clone())
            .unwrap_or_default();
        format!("{auth}  {host}")
    }

    fn choose_account(&mut self, selected: usize, profiles: usize, name: String) {
        if selected >= profiles {
            self.modal = Modal::Connect(Box::default());
            return;
        }
        let Some(profile) = self.config.lookup(&name).cloned() else {
            return;
        };
        if let Some(max_rows) = profile.max_rows {
            self.max_rows = max_rows;
        }
        self.open(profile, None);
    }

    fn submit_connection(&mut self, profile: Profile, name: &str) {
        let replacing = match &self.modal {
            Modal::Connect(form) => form.editing.clone(),
            _ => None,
        };
        if let Some(previous) = replacing.filter(|previous| previous != name) {
            let _ = config::forget_account(&previous);
            self.config.saved.remove(&previous);
        }
        if self.config.profiles.contains_key(name) {
            self.fail(format!("{name:?} is already a profile in the config file"));
            return;
        }
        if profile.host.as_deref().unwrap_or_default().is_empty() {
            self.fail("a host is required".into());
            return;
        }
        let save = (!name.is_empty()).then(|| (name.to_string(), profile.clone()));
        self.open(profile, save);
    }

    fn open(&mut self, profile: Profile, save: Option<(String, Profile)>) {
        self.pending_save = save;
        let outbox = self.outbox.clone();
        self.busy = true;
        self.runtime.spawn(async move {
            let _ = outbox.send(sign_in_and_connect(profile, &outbox).await);
        });
    }

    fn edit_account(&mut self) {
        let Some(name) = self.selected_account() else {
            return;
        };
        if !self.config.is_saved(&name) {
            self.fail(format!(
                "{name:?} is written in the config file — edit it there"
            ));
            return;
        }
        let Some(profile) = self.config.lookup(&name).cloned() else {
            return;
        };
        self.modal = Modal::Connect(Box::new(Connect::editing(&name, &profile)));
    }

    fn selected_account(&self) -> Option<String> {
        let Modal::Picker(picker) = &self.modal else {
            return None;
        };
        if !matches!(picker.stage, Stage::Account { .. }) {
            return None;
        }
        Some(picker.choices.get(picker.selected)?.label.clone())
    }

    fn confirm_forget(&mut self) {
        let Some(name) = self.selected_account() else {
            return;
        };
        if !self.config.is_saved(&name) {
            self.fail(format!("{name:?} is not a saved account"));
            return;
        }
        self.modal = Modal::Confirm {
            prompt: format!("Forget saved account {name}?"),
            action: Pending::Forget { name },
        };
    }

    fn forget(&mut self, name: String) {
        match config::forget_account(&name) {
            Ok(_) => {
                self.config.saved.remove(&name);
                self.note(format!("forgot {name:?}"));
            }
            Err(error) => self.fail(error.to_string()),
        }
        self.pick_account();
    }

    fn commit_save(&mut self) {
        let Some((name, profile)) = self.pending_save.take() else {
            return;
        };
        match config::save_account(&name, &profile) {
            Ok(path) => {
                self.config.saved.insert(name.clone(), profile);
                self.note(format!("saved {name:?} to {}", path.display()));
            }
            Err(error) => self.fail(format!("could not save the account: {error}")),
        }
    }

    fn spawn<F>(&mut self, task: F)
    where
        F: std::future::Future<Output = anyhow::Result<Msg>> + Send + 'static,
    {
        self.busy = true;
        let outbox = self.outbox.clone();
        self.runtime.spawn(async move {
            let message = task
                .await
                .unwrap_or_else(|error| Msg::Failed(error.to_string()));
            let _ = outbox.send(message);
        });
    }

    fn toggle_sidebar(&mut self) {
        self.sidebar = !self.sidebar;
        if !self.sidebar && self.focus == Focus::Library {
            self.focus = Focus::Query;
        }
    }

    fn reset_cursor(&mut self) {
        self.row = 0;
        self.column = 0;
        self.column_offset = 0;
    }

    fn note(&mut self, text: String) {
        self.status = text;
        self.failed = false;
    }

    fn fail(&mut self, text: String) {
        self.status = text;
        self.failed = true;
    }
}

async fn sign_in_and_connect(profile: Profile, outbox: &UnboundedSender<Msg>) -> Msg {
    let database = profile.database.clone();
    let signed_in = match sign_in(&profile, outbox).await {
        Ok(token) => token,
        Err(error) => return Msg::Failed(error.to_string()),
    };
    match connect(&profile, signed_in, database).await {
        Ok(connected) => connected,
        Err(error) => Msg::Failed(error.to_string()),
    }
}

async fn sign_in(
    profile: &Profile,
    outbox: &UnboundedSender<Msg>,
) -> anyhow::Result<Option<String>> {
    if !profile.uses_azure() {
        return Ok(None);
    }
    if let Some(command) = &profile.token_command {
        return auth::token_from_command(command).map(Some);
    }

    let prompt = outbox.clone();
    auth::token(
        &profile.tenant(),
        &profile.client_id(),
        profile.flow(),
        move |step| {
            let _ = prompt.send(Msg::DeviceCode(step));
        },
    )
    .await
    .map(Some)
}

async fn connect(
    profile: &Profile,
    token: Option<String>,
    database: Option<String>,
) -> anyhow::Result<Msg> {
    let credentials: Credentials = profile.credentials(token)?;
    let client = db::connect(&credentials, database.as_deref()).await?;
    Ok(Msg::Connected {
        server_root: queries::server_root(&credentials.host, credentials.port),
        description: credentials.describe(),
        encryption: credentials.encrypt,
        client: Arc::new(Mutex::new(client)),
        database,
    })
}

fn editor(sql: &str) -> TextArea<'static> {
    let mut query = TextArea::from(sql.lines().collect::<Vec<_>>());
    query.set_cursor_line_style(Default::default());
    query.move_cursor(CursorMove::Bottom);
    query.move_cursor(CursorMove::End);
    query
}

fn describe(results: &[ResultSet], elapsed: std::time::Duration) -> String {
    let rows: usize = results.iter().map(|result| result.rows.len()).sum();
    let truncated = results.iter().any(|result| result.truncated);
    let sets = match results.len() {
        0 | 1 => String::new(),
        count => format!(" in {count} result sets"),
    };
    match (rows, truncated) {
        (0, _) => format!("statement completed in {elapsed:.2?}"),
        (rows, false) => format!("{rows} rows{sets} in {elapsed:.2?}"),
        (rows, true) => format!("{rows} rows{sets} (capped) in {elapsed:.2?}"),
    }
}

fn is_run(key: &KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Enter | KeyCode::Char('r'))
}

fn char_to_byte(line: &str, index: usize) -> usize {
    line.char_indices()
        .nth(index)
        .map(|(byte, _)| byte)
        .unwrap_or(line.len())
}

fn byte_to_char(line: &str, byte: usize) -> usize {
    line[..byte.min(line.len())].chars().count()
}
