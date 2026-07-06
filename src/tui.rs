//! Terminal settings UI (ratatui).
//! Edits the main settings + the whitelist (with suggestions), and saves them.
//! API keys go into the secrets file, not into the config.

use crate::config::{Config, DelayMode, Provider, Secrets};
use crate::{aur, deploy, t};
use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph},
    Terminal,
};
use std::io;

// Field indices of the main screen.
const F_DELAY: usize = 0;
const F_MODE: usize = 1;
const F_HELPER: usize = 2;
const F_SCAN: usize = 3;
const F_AI: usize = 4;
const F_PROVIDER: usize = 5;
const F_MODEL: usize = 6;
const F_APIKEY: usize = 7;
const F_VOTES: usize = 8;
const F_NOTIFY: usize = 9;
const F_NOTIFY_INTERVAL: usize = 10;
const F_NOTIFY_SILENT: usize = 11;
const F_NOTIFY_TEST: usize = 12;
const F_WHITELIST: usize = 13;
const FIELDS: usize = 14;

const DELAY_MAX: u64 = 365;
const VOTES_MIN: u32 = 1;
const VOTES_MAX: u32 = 9;
/// Bounds of the notification interval (hours): 1 h to one week.
const NOTIFY_INTERVAL_MIN: u64 = 1;
const NOTIFY_INTERVAL_MAX: u64 = 168;

#[derive(PartialEq)]
enum Screen {
    Main,
    Whitelist,
}

struct App {
    cfg: Config,
    installed: Vec<String>,
    sel: usize,
    screen: Screen,
    wl_sel: usize,
    input: Option<String>,
    status: String,
    dirty: bool,
}

impl App {
    fn new(cfg: Config) -> Self {
        App {
            cfg,
            installed: aur::installed_aur_packages(),
            sel: 0,
            screen: Screen::Main,
            wl_sel: 0,
            input: None,
            status: t!("↑/↓ move · ←/→ change · Enter edit/whitelist · s save · q quit"),
            dirty: false,
        }
    }

    /// Suggestions = installed AUR packages not in the whitelist.
    fn suggestions(&self) -> Vec<String> {
        self.installed
            .iter()
            .filter(|p| !self.cfg.is_whitelisted(p))
            .cloned()
            .collect()
    }

    fn adjust(&mut self, delta: i64) {
        match self.sel {
            F_DELAY => {
                let v = self.cfg.delay_days as i64 + delta;
                self.cfg.delay_days = v.clamp(0, DELAY_MAX as i64) as u64;
            }
            F_MODE => {
                self.cfg.delay_mode = match self.cfg.delay_mode {
                    DelayMode::Lag => DelayMode::Hold,
                    DelayMode::Hold => DelayMode::Lag,
                };
            }
            F_HELPER => {
                self.cfg.helper = if self.cfg.helper == "yay" {
                    "paru".into()
                } else {
                    "yay".into()
                };
            }
            F_SCAN => self.cfg.use_aur_scan = !self.cfg.use_aur_scan,
            F_AI => self.cfg.ai.enabled = !self.cfg.ai.enabled,
            F_PROVIDER => {
                self.cfg.ai.provider = cycle_provider(self.cfg.ai.provider, delta >= 0);
            }
            F_VOTES => {
                let v = self.cfg.ai.confirm_votes as i64 + delta;
                self.cfg.ai.confirm_votes = v.clamp(VOTES_MIN as i64, VOTES_MAX as i64) as u32;
            }
            F_NOTIFY => self.cfg.notify.enabled = !self.cfg.notify.enabled,
            F_NOTIFY_INTERVAL => {
                let v = self.cfg.notify.interval_hours as i64 + delta;
                self.cfg.notify.interval_hours =
                    v.clamp(NOTIFY_INTERVAL_MIN as i64, NOTIFY_INTERVAL_MAX as i64) as u64;
            }
            F_NOTIFY_SILENT => {
                self.cfg.notify.silent_when_up_to_date = !self.cfg.notify.silent_when_up_to_date;
            }
            _ => {}
        }
        self.dirty = true;
    }

    fn rows(&self) -> Vec<(String, String)> {
        vec![
            (t!("Security delay"), t!("{} days", self.cfg.delay_days)),
            (t!("Delay mode"), format!("{:?}", self.cfg.delay_mode)),
            (t!("AUR helper"), self.cfg.helper.clone()),
            (t!("Static scan (aur-scan)"), onoff(self.cfg.use_aur_scan)),
            (t!("AI review"), onoff(self.cfg.ai.enabled)),
            (t!("AI provider"), format!("{:?}", self.cfg.ai.provider)),
            (t!("Model"), self.model_display()),
            (t!("API key"), self.apikey_display()),
            (
                t!("Confirmation votes"),
                self.cfg.ai.confirm_votes.to_string(),
            ),
            (t!("Notifications"), onoff(self.cfg.notify.enabled)),
            (
                t!("Notify interval"),
                t!("{} hours", self.cfg.notify.interval_hours),
            ),
            (
                t!("Silent when up to date"),
                onoff(self.cfg.notify.silent_when_up_to_date),
            ),
            (t!("Test notification"), t!("[Enter to send]")),
            (
                t!("Whitelist"),
                t!("{} packages ▸", self.cfg.whitelist.len()),
            ),
        ]
    }

    fn model_display(&self) -> String {
        if self.cfg.ai.model.is_empty() {
            t!("(default: {})", self.cfg.ai.provider.default_model())
        } else {
            self.cfg.ai.model.clone()
        }
    }

    fn apikey_display(&self) -> String {
        let p = self.cfg.ai.provider;
        if std::env::var(p.default_key_env())
            .map(|v| !v.is_empty())
            .unwrap_or(false)
        {
            t!("set (${})", p.default_key_env())
        } else if Secrets::load().get(p).is_some() {
            t!("saved")
        } else {
            t!("not set")
        }
    }
}

fn cycle_provider(p: Provider, forward: bool) -> Provider {
    match (p, forward) {
        (Provider::Groq, true) => Provider::Anthropic,
        (Provider::Anthropic, true) => Provider::Openai,
        (Provider::Openai, true) => Provider::Groq,
        (Provider::Groq, false) => Provider::Openai,
        (Provider::Anthropic, false) => Provider::Groq,
        (Provider::Openai, false) => Provider::Anthropic,
    }
}

fn onoff(b: bool) -> String {
    if b {
        t!("✅ enabled")
    } else {
        t!("⬜ disabled")
    }
}

/// Launches the configuration TUI.
pub fn run(cfg: Config) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(cfg);
    let res = event_loop(&mut terminal, &mut app);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    res
}

fn event_loop<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
) -> Result<()> {
    loop {
        terminal.draw(|f| ui(f, app))?;
        if let Event::Key(key) = event::read()? {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            let quit = match app.screen {
                Screen::Main => main_keys(app, key.code),
                Screen::Whitelist => {
                    whitelist_keys(app, key.code);
                    false
                }
            };
            if quit {
                break;
            }
        }
    }
    Ok(())
}

/// Main screen keys. Returns true if we should quit.
fn main_keys(app: &mut App, code: KeyCode) -> bool {
    // Text-field input mode (model / API key).
    if let Some(buf) = app.input.as_mut() {
        match code {
            KeyCode::Char(c) => buf.push(c),
            KeyCode::Backspace => {
                buf.pop();
            }
            KeyCode::Enter => commit_text_field(app),
            KeyCode::Esc => {
                app.input = None;
                app.status = t!("Input cancelled");
            }
            _ => {}
        }
        return false;
    }

    match code {
        KeyCode::Char('q') | KeyCode::Esc => {
            if app.dirty {
                app.status = t!("Unsaved changes — 's' to save, 'Q' to quit without saving");
            } else {
                return true;
            }
        }
        KeyCode::Char('Q') => return true,
        KeyCode::Up => app.sel = (app.sel + FIELDS - 1) % FIELDS,
        KeyCode::Down | KeyCode::Tab => app.sel = (app.sel + 1) % FIELDS,
        KeyCode::Left => app.adjust(-1),
        KeyCode::Right | KeyCode::Char(' ') => app.adjust(1),
        KeyCode::Enter => match app.sel {
            F_WHITELIST => {
                app.screen = Screen::Whitelist;
                app.wl_sel = 0;
                app.status = t!("↑/↓ · a add · d remove · Enter adds a suggestion · Esc back");
            }
            F_MODEL => {
                app.input = Some(app.cfg.ai.model.clone());
                app.status = t!("Model name then Enter (Esc cancels)");
            }
            F_APIKEY => {
                app.input = Some(String::new());
                app.status = t!(
                    "{} API key then Enter (Esc cancels)",
                    app.cfg.ai.provider.default_key_env()
                );
            }
            F_NOTIFY_TEST => {
                deploy::send_test_notification();
                app.status = t!("Test notification sent");
            }
            _ => {}
        },
        KeyCode::Char('s') => save(app),
        _ => {}
    }
    false
}

/// Commits the text field being edited (model or API key).
fn commit_text_field(app: &mut App) {
    let Some(buf) = app.input.take() else {
        return;
    };
    match app.sel {
        F_MODEL => {
            app.cfg.ai.model = buf.trim().to_string();
            app.dirty = true;
            app.status = t!("Model updated");
        }
        F_APIKEY => {
            if buf.trim().is_empty() {
                app.status = t!("Empty key ignored");
                return;
            }
            let mut secrets = Secrets::load();
            secrets.set(app.cfg.ai.provider, Some(buf));
            app.status = match secrets.save() {
                Ok(_) => t!("✔ API key saved (secrets.toml, 0600)"),
                Err(e) => t!("Secrets error: {}", e),
            };
        }
        _ => {}
    }
}

fn whitelist_keys(app: &mut App, code: KeyCode) {
    // Input mode for a new package.
    if let Some(buf) = app.input.as_mut() {
        match code {
            KeyCode::Char(c) => buf.push(c),
            KeyCode::Backspace => {
                buf.pop();
            }
            KeyCode::Enter => {
                let name = buf.trim().to_string();
                add_whitelist(app, &name);
                app.input = None;
                app.status = t!("a add · d remove · Enter adds a suggestion · Esc back");
            }
            KeyCode::Esc => {
                app.input = None;
                app.status = t!("Input cancelled");
            }
            _ => {}
        }
        return;
    }

    let wl_len = app.cfg.whitelist.len();
    let total = wl_len + app.suggestions().len();
    match code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.screen = Screen::Main;
            app.status = t!("↑/↓ · ←/→ change · Enter edit · s save · q quit");
        }
        KeyCode::Up if total > 0 => app.wl_sel = (app.wl_sel + total - 1) % total,
        KeyCode::Down if total > 0 => app.wl_sel = (app.wl_sel + 1) % total,
        KeyCode::Char('a') => {
            app.input = Some(String::new());
            app.status = t!("Package name then Enter (Esc to cancel)");
        }
        KeyCode::Char('d') if app.wl_sel < wl_len => {
            let removed = app.cfg.whitelist.remove(app.wl_sel);
            app.dirty = true;
            if app.wl_sel >= app.cfg.whitelist.len() && app.wl_sel > 0 {
                app.wl_sel -= 1;
            }
            app.status = t!("Removed: {}", removed);
        }
        // Enter on a suggestion: add it to the whitelist.
        KeyCode::Enter if app.wl_sel >= wl_len => {
            let sugg = app.suggestions();
            if let Some(name) = sugg.get(app.wl_sel - wl_len).cloned() {
                add_whitelist(app, &name);
                app.status = t!("Added: {}", name);
            }
        }
        KeyCode::Char('s') => save(app),
        _ => {}
    }
}

fn add_whitelist(app: &mut App, name: &str) {
    if !name.is_empty() && !app.cfg.is_whitelisted(name) {
        app.cfg.whitelist.push(name.to_string());
        app.cfg.whitelist.sort();
        app.dirty = true;
    }
}

fn save(app: &mut App) {
    app.status = match app.cfg.save() {
        Ok(_) => {
            app.dirty = false;
            // Sync the notification systemd timer with the settings.
            match deploy::apply_notify(&app.cfg.notify) {
                Ok(_) => t!("✔ Configuration saved"),
                Err(e) => t!("Saved, but notification setup failed: {}", e),
            }
        }
        Err(e) => t!("Save error: {}", e),
    };
}

fn ui(f: &mut ratatui::Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(8),
            Constraint::Length(3),
        ])
        .split(f.area());

    let header = match app.screen {
        Screen::Main => t!("aurveto — settings"),
        Screen::Whitelist => t!("aurveto — whitelist"),
    };
    let title = Paragraph::new(header)
        .style(Style::default().add_modifier(Modifier::BOLD))
        .block(Block::default().borders(Borders::ALL));
    f.render_widget(title, chunks[0]);

    match app.screen {
        Screen::Main => render_main(f, app, chunks[1]),
        Screen::Whitelist => render_whitelist(f, app, chunks[1]),
    }

    let status = Paragraph::new(app.status.clone()).block(Block::default().borders(Borders::ALL));
    f.render_widget(status, chunks[2]);
}

fn render_main(f: &mut ratatui::Frame, app: &App, area: ratatui::layout::Rect) {
    let items: Vec<ListItem> = app
        .rows()
        .into_iter()
        .enumerate()
        .map(|(i, (label, value))| {
            let selected = i == app.sel;
            let editing = selected && app.input.is_some();
            let shown = if editing {
                edit_buffer_display(app, i)
            } else {
                value
            };
            let marker = if selected { "▶ " } else { "  " };
            let label_style = if selected {
                Style::default().fg(Color::Black).bg(Color::Cyan)
            } else {
                Style::default()
            };
            let value_style = if editing {
                Style::default().fg(Color::Green)
            } else {
                Style::default().fg(Color::Yellow)
            };
            ListItem::new(Line::from(vec![
                Span::raw(marker),
                Span::styled(format!("{label:<28}"), label_style),
                Span::raw("  "),
                Span::styled(shown, value_style),
            ]))
        })
        .collect();
    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .title(t!(" settings ")),
    );
    f.render_widget(list, area);
}

/// Displays the buffer being edited (API key masked).
fn edit_buffer_display(app: &App, field: usize) -> String {
    let buf = app.input.as_deref().unwrap_or("");
    if field == F_APIKEY {
        format!("{}_", "•".repeat(buf.chars().count()))
    } else {
        format!("{buf}_")
    }
}

fn render_whitelist(f: &mut ratatui::Frame, app: &App, area: ratatui::layout::Rect) {
    let wl_len = app.cfg.whitelist.len();
    let suggestions = app.suggestions();
    let mut items: Vec<ListItem> = Vec::new();

    for (i, pkg) in app.cfg.whitelist.iter().enumerate() {
        items.push(wl_line(
            pkg,
            i == app.wl_sel && app.input.is_none(),
            Color::Cyan,
            "",
        ));
    }
    if !suggestions.is_empty() {
        items.push(ListItem::new(Line::from(Span::styled(
            t!("  — suggestions (installed AUR packages) —"),
            Style::default().fg(Color::DarkGray),
        ))));
    }
    for (i, pkg) in suggestions.iter().enumerate() {
        let selected = app.wl_sel == wl_len + i && app.input.is_none();
        items.push(wl_line(pkg, selected, Color::Green, "+ "));
    }
    if let Some(buf) = &app.input {
        items.push(ListItem::new(Line::from(vec![
            Span::styled("+ ", Style::default().fg(Color::Green)),
            Span::styled(format!("{buf}_"), Style::default().fg(Color::Green)),
        ])));
    }

    let list = List::new(items).block(Block::default().borders(Borders::ALL).title(t!(
        " whitelist ({}) · suggestions ({}) ",
        wl_len,
        suggestions.len()
    )));
    f.render_widget(list, area);
}

fn wl_line(pkg: &str, selected: bool, accent: Color, prefix: &str) -> ListItem<'static> {
    let marker = if selected { "▶ " } else { "  " };
    let style = if selected {
        Style::default().fg(Color::Black).bg(accent)
    } else {
        Style::default()
    };
    ListItem::new(Line::from(vec![
        Span::raw(marker),
        Span::styled(format!("{prefix}{pkg}"), style),
    ]))
}
