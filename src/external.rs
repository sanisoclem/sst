use std::io::{self, Write as _};
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{
    self, Event, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};

pub type Tui = Terminal<CrosstermBackend<io::Stdout>>;

pub fn enter() -> Result<Tui> {
    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen)?;
    enhance_keyboard();
    Ok(Terminal::new(CrosstermBackend::new(io::stdout()))?)
}

pub fn leave() -> Result<()> {
    if enhanced() {
        let _ = execute!(io::stdout(), PopKeyboardEnhancementFlags);
    }
    execute!(io::stdout(), LeaveAlternateScreen)?;
    disable_raw_mode()?;
    Ok(())
}

static ENHANCED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn enhance_keyboard() {
    let supported = ratatui::crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false);
    if !supported {
        return;
    }
    let pushed = execute!(
        io::stdout(),
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    )
    .is_ok();
    ENHANCED.store(pushed, std::sync::atomic::Ordering::Relaxed);
}

pub fn enhanced() -> bool {
    ENHANCED.load(std::sync::atomic::Ordering::Relaxed)
}

pub fn edit(terminal: &mut Tui, contents: &str, extension: &str) -> Result<Option<String>> {
    let file = scratch_file(extension)?;
    std::fs::write(&file, contents)?;

    let command = program("VISUAL")
        .or_else(|| program("EDITOR"))
        .unwrap_or_else(|| vec!["vi".to_string()]);
    let status = suspended(terminal, || run(&command, &file));

    let edited = std::fs::read_to_string(&file);
    let _ = std::fs::remove_file(&file);
    status?;

    let edited = edited.context("reading the file back after editing")?;
    Ok((edited != contents).then_some(edited))
}

pub fn page(terminal: &mut Tui, contents: &str, extension: &str) -> Result<()> {
    let file = scratch_file(extension)?;
    std::fs::write(&file, contents)?;

    let command = program("PAGER").unwrap_or_else(|| vec!["less".to_string(), "-R".to_string()]);
    let status = suspended(terminal, || run(&command, &file));

    let _ = std::fs::remove_file(&file);
    status
}

fn suspended<T>(terminal: &mut Tui, body: impl FnOnce() -> Result<T>) -> Result<T> {
    leave()?;
    let outcome = body();

    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen)?;
    enhance_keyboard();
    io::stdout().flush()?;
    drain_input();
    terminal.clear()?;

    outcome
}

fn run(command: &[String], file: &PathBuf) -> Result<()> {
    let status = Command::new(&command[0])
        .args(&command[1..])
        .arg(file)
        .status()
        .with_context(|| format!("launching {:?}", command[0]))?;
    if !status.success() {
        bail!("{} exited with {status}", command[0]);
    }
    Ok(())
}

fn drain_input() {
    while matches!(event::poll(std::time::Duration::ZERO), Ok(true)) {
        if !matches!(event::read(), Ok(Event::Key(_) | Event::Mouse(_))) {
            break;
        }
    }
}

fn program(variable: &str) -> Option<Vec<String>> {
    let value = std::env::var(variable).ok()?;
    let parts: Vec<String> = value.split_whitespace().map(str::to_string).collect();
    (!parts.is_empty()).then_some(parts)
}

fn scratch_file(extension: &str) -> Result<PathBuf> {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    Ok(std::env::temp_dir().join(format!("sst-{}-{nanos}.{extension}", std::process::id())))
}
