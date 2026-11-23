//! Interactive terminal remote.
//!
//! The UI thread never touches the network: keypresses are queued to a
//! worker thread (which preserves ordering and handles reconnection), and a
//! poller thread keeps the device name / active app fresh. That keeps
//! rendering and input at a steady frame rate even on a slow or flaky LAN.

use std::io;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph},
    Frame, Terminal,
};

use crate::config::Config;
use crate::discover;
use crate::ecp::{self, Roku};

// ── palette ────────────────────────────────────────────────────────────────

const CARD_BG: Color = Color::Rgb(22, 20, 30);
const CARD_BORDER: Color = Color::Rgb(78, 66, 112);
const BTN_BG: Color = Color::Rgb(36, 33, 48);
const BTN_BORDER: Color = Color::Rgb(62, 58, 82);
const TEXT: Color = Color::Rgb(222, 218, 236);
const DIM: Color = Color::Rgb(118, 112, 140);
const ACCENT: Color = Color::Rgb(156, 108, 255);
const ACCENT_BG: Color = Color::Rgb(64, 38, 120);
const PRESS_BG: Color = Color::Rgb(124, 84, 230);
const DANGER: Color = Color::Rgb(236, 92, 104);
const GOOD: Color = Color::Rgb(112, 214, 148);
const WARN: Color = Color::Rgb(242, 192, 92);

const FLASH_MS: f32 = 280.0;
const STATUS_FADE: Duration = Duration::from_secs(4);
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

// ── entry point ────────────────────────────────────────────────────────────

pub fn run(ip_override: Option<String>) -> Result<()> {
    let ip = resolve_ip(ip_override)?;

    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    terminal.hide_cursor()?;

    let result = event_loop(&mut terminal, ip);

    let _ = terminal.show_cursor();
    disable_raw_mode()?;
    execute!(io::stdout(), LeaveAlternateScreen)?;
    result
}

// ── worker protocol ────────────────────────────────────────────────────────

#[derive(Clone)]
struct DeviceChoice {
    ip: String,
    name: String,
}

enum PendingAction {
    Keypress(String),
    TypeText(String),
}

impl PendingAction {
    fn execute(&self, roku: &Roku) -> Result<()> {
        match self {
            Self::Keypress(key) => roku.keypress(key),
            Self::TypeText(text) => roku.type_text(text),
        }
    }

    fn success_message(&self) -> String {
        match self {
            Self::Keypress(key) => format!("Sent {}", key),
            Self::TypeText(text) => format!("Typed “{}”", text),
        }
    }
}

enum Cmd {
    /// Check the configured device is reachable.
    Probe,
    Do(PendingAction),
    /// User picked a device from the recovery dialog.
    Choose(DeviceChoice, Option<PendingAction>),
    /// User dismissed the recovery dialog.
    Cancel,
}

enum Msg {
    // Each command produces exactly one of these…
    Done(String),
    Failed(String),
    Skipped,
    Choose {
        devices: Vec<DeviceChoice>,
        pending: Option<PendingAction>,
        error: String,
    },
    Connected {
        ip: String,
        text: String,
        ok: bool,
    },
    // …and these may arrive at any time.
    Discovering,
    Info {
        ip: String,
        online: bool,
        name: Option<String>,
        app: Option<String>,
    },
}

impl Msg {
    fn completes_job(&self) -> bool {
        !matches!(self, Msg::Discovering | Msg::Info { .. })
    }
}

struct Worker {
    roku: Roku,
    ip: Arc<Mutex<String>>,
    tx: Sender<Msg>,
    awaiting_choice: bool,
}

impl Worker {
    fn spawn(ip: Arc<Mutex<String>>, tx: Sender<Msg>) -> Sender<Cmd> {
        let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();
        let roku = Roku::new(&ip.lock().unwrap());
        let mut worker = Worker {
            roku,
            ip,
            tx,
            awaiting_choice: false,
        };
        thread::spawn(move || {
            while let Ok(cmd) = cmd_rx.recv() {
                if let Some(msg) = worker.handle(cmd) {
                    if worker.tx.send(msg).is_err() {
                        break;
                    }
                }
            }
        });
        cmd_tx
    }

    fn handle(&mut self, cmd: Cmd) -> Option<Msg> {
        Some(match cmd {
            Cmd::Cancel => {
                self.awaiting_choice = false;
                return None;
            }
            // Keys pressed while the device picker is open would only fail
            // against the stale address; drop them.
            Cmd::Do(_) | Cmd::Probe if self.awaiting_choice => Msg::Skipped,
            Cmd::Probe => match self.roku.device_info() {
                Ok(_) => Msg::Done("Connected".to_string()),
                Err(e) if is_connection_error(&e) => self.recover(None, e),
                Err(e) => Msg::Failed(e.to_string()),
            },
            Cmd::Do(action) => match action.execute(&self.roku) {
                Ok(()) => Msg::Done(action.success_message()),
                Err(e) if is_connection_error(&e) => self.recover(Some(action), e),
                Err(e) => Msg::Failed(e.to_string()),
            },
            Cmd::Choose(choice, pending) => {
                self.awaiting_choice = false;
                self.activate(&choice, pending)
            }
        })
    }

    fn recover(&mut self, pending: Option<PendingAction>, error: anyhow::Error) -> Msg {
        let _ = self.tx.send(Msg::Discovering);
        let found = match discover::discover(3) {
            Ok(found) => found,
            Err(e) => {
                return Msg::Failed(format!(
                    "Connection failed: {}; discovery failed: {}",
                    error, e
                ))
            }
        };
        let devices: Vec<DeviceChoice> = found
            .into_iter()
            .map(|found| {
                let name = Roku::new(&found.ip)
                    .device_info()
                    .ok()
                    .and_then(|xml| ecp::tag_text(&xml, "friendly-device-name"))
                    .unwrap_or_default();
                DeviceChoice { ip: found.ip, name }
            })
            .collect();

        match devices.len() {
            0 => Msg::Failed(format!("Connection failed: {}; no Rokus found", error)),
            1 => self.activate(&devices[0], pending),
            _ => {
                self.awaiting_choice = true;
                Msg::Choose {
                    devices,
                    pending,
                    error: error.to_string(),
                }
            }
        }
    }

    fn activate(&mut self, choice: &DeviceChoice, pending: Option<PendingAction>) -> Msg {
        self.roku = Roku::new(&choice.ip);
        *self.ip.lock().unwrap() = choice.ip.clone();

        let save_error = Config::load()
            .and_then(|mut config| {
                config.roku_ip = Some(choice.ip.clone());
                config.save()
            })
            .err();

        let result = pending.as_ref().map(|action| action.execute(&self.roku));
        let (text, ok) = match (result, save_error) {
            (Some(Ok(())), None) => (
                format!(
                    "Reconnected to {} · {}",
                    choice.ip,
                    pending
                        .as_ref()
                        .expect("result came from pending")
                        .success_message()
                ),
                true,
            ),
            (Some(Ok(())), Some(e)) => (
                format!("Reconnected to {}; could not save IP: {}", choice.ip, e),
                false,
            ),
            (Some(Err(e)), _) => (
                format!("Found {}, but retry failed: {}", choice.ip, e),
                false,
            ),
            (None, None) => (
                format!("Connected to {} (saved to config)", choice.ip),
                true,
            ),
            (None, Some(e)) => (
                format!("Connected to {}; could not save IP: {}", choice.ip, e),
                false,
            ),
        };
        Msg::Connected {
            ip: choice.ip.clone(),
            text,
            ok,
        }
    }
}

/// Polls the active app (and device name, once per address) in the background.
fn spawn_poller(ip: Arc<Mutex<String>>, tx: Sender<Msg>) {
    thread::spawn(move || {
        let mut named: Option<(String, Option<String>)> = None;
        loop {
            let current = ip.lock().unwrap().clone();
            let roku = Roku::new(&current);

            if named.as_ref().map(|(i, _)| i) != Some(&current) {
                let name = roku
                    .device_info()
                    .ok()
                    .and_then(|xml| ecp::tag_text(&xml, "friendly-device-name"));
                named = Some((current.clone(), name));
            }
            let app = roku.active_app_name();
            let online = app.is_ok();
            let msg = Msg::Info {
                ip: current.clone(),
                online,
                name: named.as_ref().and_then(|(_, n)| n.clone()),
                app: app.ok().map(|a| match a.split_once(" (id ") {
                    Some((name, _)) => name.to_string(),
                    None => a,
                }),
            };
            if !online {
                named = None;
            }
            if tx.send(msg).is_err() {
                return;
            }
            // Sleep ~3s, but wake early if the address changes.
            for _ in 0..12 {
                thread::sleep(Duration::from_millis(250));
                if *ip.lock().unwrap() != current {
                    break;
                }
            }
        }
    });
}

// ── UI state ───────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Debug)]
enum Tone {
    Info,
    Good,
    Bad,
    Busy,
}

struct Recovery {
    devices: Vec<DeviceChoice>,
    selected: usize,
    pending: Option<PendingAction>,
}

struct App {
    ip: String,
    device_name: Option<String>,
    active_app: Option<String>,
    online: Option<bool>,
    status: String,
    tone: Tone,
    status_at: Instant,
    flash: Option<(&'static str, Instant)>,
    typing: Option<String>,
    recovery: Option<Recovery>,
    in_flight: usize,
    discovering: bool,
    started: Instant,
}

impl App {
    fn new(ip: String) -> Self {
        let now = Instant::now();
        App {
            ip,
            device_name: None,
            active_app: None,
            online: None,
            status: "Connecting…".to_string(),
            tone: Tone::Busy,
            status_at: now,
            flash: None,
            typing: None,
            recovery: None,
            in_flight: 0,
            discovering: false,
            started: now,
        }
    }

    fn set_status(&mut self, text: impl Into<String>, tone: Tone) {
        self.status = text.into();
        self.tone = tone;
        self.status_at = Instant::now();
    }

    /// 0.0..=1.0 strength of the press animation for a given button.
    fn flash_strength(&self, id: &str) -> f32 {
        match self.flash {
            Some((f, at)) if f == id => {
                let x = (at.elapsed().as_secs_f32() * 1000.0 / FLASH_MS).min(1.0);
                (1.0 - x) * (1.0 - x)
            }
            _ => 0.0,
        }
    }

    fn animating(&self) -> bool {
        self.flash
            .is_some_and(|(_, at)| at.elapsed().as_secs_f32() * 1000.0 < FLASH_MS)
            || self.in_flight > 0
            || self.discovering
            || self.status_at.elapsed() < STATUS_FADE + Duration::from_millis(600)
    }

    fn spinner(&self) -> &'static str {
        SPINNER[(self.started.elapsed().as_millis() / 80) as usize % SPINNER.len()]
    }

    fn apply(&mut self, msg: Msg) {
        if msg.completes_job() {
            self.in_flight = self.in_flight.saturating_sub(1);
            self.discovering = false;
        }
        match msg {
            Msg::Done(text) => self.set_status(text, Tone::Good),
            Msg::Failed(text) => self.set_status(text, Tone::Bad),
            Msg::Skipped => {}
            Msg::Discovering => {
                self.discovering = true;
                self.set_status("Connection lost — searching for Rokus…", Tone::Busy);
            }
            Msg::Choose {
                devices,
                pending,
                error,
            } => {
                self.set_status(format!("{} — choose a Roku", error), Tone::Bad);
                self.recovery = Some(Recovery {
                    devices,
                    selected: 0,
                    pending,
                });
            }
            Msg::Connected { ip, text, ok } => {
                if ip != self.ip {
                    self.device_name = None;
                    self.active_app = None;
                }
                self.ip = ip;
                self.online = Some(true);
                self.set_status(text, if ok { Tone::Good } else { Tone::Bad });
            }
            Msg::Info {
                ip,
                online,
                name,
                app,
            } => {
                if ip == self.ip {
                    self.online = Some(online);
                    if name.is_some() {
                        self.device_name = name;
                    }
                    self.active_app = app;
                }
            }
        }
    }
}

enum Outcome {
    Continue,
    Quit,
    Send(Cmd),
}

/// Maps a key to a remote button: (button id, ECP key).
fn remote_key(code: KeyCode) -> Option<(&'static str, &'static str)> {
    Some(match code {
        KeyCode::Up | KeyCode::Char('k') => ("Up", "Up"),
        KeyCode::Down | KeyCode::Char('j') => ("Down", "Down"),
        KeyCode::Left => ("Left", "Left"),
        KeyCode::Right | KeyCode::Char('l') => ("Right", "Right"),
        KeyCode::Enter => ("Select", "Select"),
        KeyCode::Backspace => ("Back", "Back"),
        KeyCode::Esc | KeyCode::Char('h') => ("Home", "Home"),
        KeyCode::Char(' ') | KeyCode::Char('p') => ("Play", "Play"),
        KeyCode::Char('m') => ("VolumeMute", "VolumeMute"),
        KeyCode::Char('+') | KeyCode::Char('=') => ("VolumeUp", "VolumeUp"),
        KeyCode::Char('-') | KeyCode::Char('_') => ("VolumeDown", "VolumeDown"),
        KeyCode::Char('i') | KeyCode::Char('*') => ("Info", "Info"),
        KeyCode::Char('r') => ("InstantReplay", "InstantReplay"),
        KeyCode::Char('s') | KeyCode::Char('/') => ("Search", "Search"),
        KeyCode::Char('f') | KeyCode::Char('.') => ("Fwd", "Fwd"),
        KeyCode::Char('w') | KeyCode::Char(',') => ("Rev", "Rev"),
        KeyCode::Char('o') => ("Power", "PowerOff"),
        KeyCode::Char('O') => ("Power", "PowerOn"),
        _ => return None,
    })
}

fn handle_key(app: &mut App, key: KeyEvent) -> Outcome {
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        return Outcome::Quit;
    }

    if let Some(recovery) = app.recovery.as_mut() {
        match key.code {
            KeyCode::Esc => {
                app.recovery = None;
                app.set_status("Device selection cancelled", Tone::Info);
                return Outcome::Send(Cmd::Cancel);
            }
            KeyCode::Char('q') => return Outcome::Quit,
            KeyCode::Up | KeyCode::Char('k') => {
                recovery.selected = recovery.selected.saturating_sub(1)
            }
            KeyCode::Down | KeyCode::Char('j') => {
                recovery.selected = (recovery.selected + 1).min(recovery.devices.len() - 1)
            }
            KeyCode::Enter => {
                let r = app.recovery.take().expect("checked above");
                let choice = r.devices[r.selected].clone();
                app.set_status(format!("Connecting to {}…", choice.ip), Tone::Busy);
                return Outcome::Send(Cmd::Choose(choice, r.pending));
            }
            _ => {}
        }
        return Outcome::Continue;
    }

    if let Some(buffer) = app.typing.as_mut() {
        match key.code {
            KeyCode::Esc => {
                app.typing = None;
                app.set_status("Typing cancelled", Tone::Info);
            }
            KeyCode::Enter => {
                let text = app.typing.take().unwrap_or_default();
                if !text.is_empty() {
                    app.set_status(format!("Typing “{}”…", text), Tone::Busy);
                    return Outcome::Send(Cmd::Do(PendingAction::TypeText(text)));
                }
            }
            KeyCode::Backspace => {
                buffer.pop();
            }
            KeyCode::Char(c) => buffer.push(c),
            _ => {}
        }
        return Outcome::Continue;
    }

    match key.code {
        KeyCode::Char('q') => Outcome::Quit,
        KeyCode::Char('t') => {
            app.flash = Some(("Type", Instant::now()));
            app.typing = Some(String::new());
            Outcome::Continue
        }
        code => match remote_key(code) {
            Some((id, ecp_key)) => {
                app.flash = Some((id, Instant::now()));
                Outcome::Send(Cmd::Do(PendingAction::Keypress(ecp_key.to_string())))
            }
            None => Outcome::Continue,
        },
    }
}

fn event_loop(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, ip: String) -> Result<()> {
    let shared_ip = Arc::new(Mutex::new(ip.clone()));
    let (msg_tx, msg_rx): (Sender<Msg>, Receiver<Msg>) = mpsc::channel();
    let worker = Worker::spawn(shared_ip.clone(), msg_tx.clone());
    spawn_poller(shared_ip, msg_tx);

    let mut app = App::new(ip);
    let send = |app: &mut App, cmd: Cmd| {
        if !matches!(cmd, Cmd::Cancel) {
            app.in_flight += 1;
        }
        let _ = worker.send(cmd);
    };
    send(&mut app, Cmd::Probe);

    loop {
        while let Ok(msg) = msg_rx.try_recv() {
            app.apply(msg);
        }
        terminal.draw(|f| draw(f, &app))?;

        // ~60fps while something is moving, otherwise idle gently (the
        // cursor blink and poller updates only need a few frames a second).
        let timeout = if app.animating() { 16 } else { 120 };
        if !event::poll(Duration::from_millis(timeout))? {
            continue;
        }
        // Drain every pending event before redrawing, so held-down keys
        // don't fall behind the screen.
        loop {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    match handle_key(&mut app, key) {
                        Outcome::Quit => return Ok(()),
                        Outcome::Send(cmd) => send(&mut app, cmd),
                        Outcome::Continue => {}
                    }
                }
            }
            if !event::poll(Duration::ZERO)? {
                break;
            }
        }
    }
}

fn is_connection_error(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.downcast_ref::<io::Error>().is_some())
}

// ── drawing ────────────────────────────────────────────────────────────────

fn blend(a: Color, b: Color, t: f32) -> Color {
    match (a, b) {
        (Color::Rgb(r1, g1, b1), Color::Rgb(r2, g2, b2)) => {
            let mix = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
            Color::Rgb(mix(r1, r2), mix(g1, g2), mix(b1, b2))
        }
        _ => {
            if t > 0.5 {
                b
            } else {
                a
            }
        }
    }
}

#[derive(Clone, Copy)]
enum Kind {
    Normal,
    Primary,
    Danger,
}

struct Button {
    id: &'static str,
    label: &'static str,
    hint: &'static str,
    kind: Kind,
}

const fn btn(id: &'static str, label: &'static str, hint: &'static str) -> Button {
    Button {
        id,
        label,
        hint,
        kind: Kind::Normal,
    }
}

/// Row height, gap between rows, gap between sections.
#[derive(Clone, Copy)]
struct Density {
    row: u16,
    row_gap: u16,
    section_gap: u16,
}

const DENSITIES: [Density; 3] = [
    Density {
        row: 3,
        row_gap: 0,
        section_gap: 1,
    },
    Density {
        row: 1,
        row_gap: 1,
        section_gap: 0,
    },
    Density {
        row: 1,
        row_gap: 0,
        section_gap: 0,
    },
];

const HEADER_ROWS: u16 = 2;
const CARD_WIDTH: u16 = 52;

fn card_height(d: Density) -> u16 {
    // header + 3 section gaps + 8 button rows + 5 intra-section gaps + border
    HEADER_ROWS + 3 * d.section_gap + 8 * d.row + 5 * d.row_gap + 2
}

fn draw(f: &mut Frame, app: &App) {
    let area = f.area();
    f.render_widget(
        Block::default().style(Style::default().bg(Color::Reset)),
        area,
    );

    // status + footer below the card
    let chrome = 3;
    let density = DENSITIES
        .into_iter()
        .find(|d| card_height(*d) + chrome <= area.height)
        .unwrap_or(DENSITIES[2]);
    let card_h = card_height(density).min(area.height.saturating_sub(chrome));
    let card_w = CARD_WIDTH.min(area.width);

    let top = area.y + area.height.saturating_sub(card_h + chrome) / 2;
    let card = Rect {
        x: area.x + (area.width - card_w) / 2,
        y: top,
        width: card_w,
        height: card_h,
    };
    let status_area = Rect {
        x: card.x,
        y: (card.y + card.height + 1).min(area.bottom().saturating_sub(2)),
        width: card_w,
        height: 1,
    };
    let footer = Rect {
        x: area.x,
        y: area.bottom().saturating_sub(1),
        width: area.width,
        height: 1,
    };

    draw_card(f, app, card, density);
    draw_status(f, app, status_area);
    draw_footer(f, footer);

    if app.typing.is_some() {
        draw_typing(f, app, card);
    }
    if let Some(recovery) = &app.recovery {
        draw_recovery(f, recovery, area);
    }
}

fn draw_card(f: &mut Frame, app: &App, card: Rect, d: Density) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(CARD_BORDER))
        .title(Line::from(vec![
            Span::styled(" ◉ ", Style::default().fg(ACCENT)),
            Span::styled(
                "ROKU ",
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            ),
        ]))
        .title_alignment(Alignment::Center)
        .style(Style::default().bg(CARD_BG));
    let inner = block.inner(card).inner(ratatui::layout::Margin::new(1, 0));
    f.render_widget(block, card);

    let mut constraints = vec![Constraint::Length(HEADER_ROWS)];
    let mut button_rows = Vec::new();
    for rows in [2, 3, 3] {
        constraints.push(Constraint::Length(d.section_gap));
        for r in 0..rows {
            if r > 0 {
                constraints.push(Constraint::Length(d.row_gap));
            }
            button_rows.push(constraints.len());
            constraints.push(Constraint::Length(d.row));
        }
    }
    constraints.push(Constraint::Min(0));
    let rows = Layout::vertical(constraints).split(inner);
    let row = |i: usize| rows[button_rows[i]];

    draw_header(f, app, rows[0]);
    let [r_power, r_nav, r_up, r_mid, r_down, r_media, r_vol, r_misc] =
        [0, 1, 2, 3, 4, 5, 6, 7].map(row);

    let none = None;
    let power = Button {
        kind: Kind::Danger,
        ..btn("Power", "⏻ Power", "o")
    };
    let ok = Button {
        kind: Kind::Primary,
        ..btn("Select", "OK", "⏎")
    };
    let layout: [(Rect, [Option<Button>; 3]); 8] = [
        (
            r_power,
            [Some(power), none, Some(btn("Home", "⌂ Home", "h"))],
        ),
        (
            r_nav,
            [
                Some(btn("Back", "← Back", "⌫")),
                Some(btn("Info", "✱ Info", "i")),
                Some(btn("InstantReplay", "↺ Replay", "r")),
            ],
        ),
        (r_up, [None, Some(btn("Up", "▲", "↑")), None]),
        (
            r_mid,
            [
                Some(btn("Left", "◀", "←")),
                Some(ok),
                Some(btn("Right", "▶", "→")),
            ],
        ),
        (r_down, [None, Some(btn("Down", "▼", "↓")), None]),
        (
            r_media,
            [
                Some(btn("Rev", "⏪", "w")),
                Some(btn("Play", "⏯", "␣")),
                Some(btn("Fwd", "⏩", "f")),
            ],
        ),
        (
            r_vol,
            [
                Some(btn("VolumeDown", "Vol −", "-")),
                Some(btn("VolumeMute", "Mute", "m")),
                Some(btn("VolumeUp", "Vol +", "+")),
            ],
        ),
        (
            r_misc,
            [
                Some(btn("Search", "⌕ Search", "s")),
                None,
                Some(btn("Type", "⌨ Type", "t")),
            ],
        ),
    ];

    for (row, buttons) in layout {
        let cols = Layout::horizontal([Constraint::Ratio(1, 3); 3])
            .spacing(1)
            .split(row);
        for (col, b) in cols.iter().zip(buttons) {
            if let Some(b) = b {
                draw_button(f, app, *col, &b);
            }
        }
    }
}

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let (dot, dot_color) = match app.online {
        Some(true) => ("●", GOOD),
        Some(false) => ("●", DANGER),
        None => (app.spinner(), WARN),
    };
    let name = app.device_name.as_deref().unwrap_or("Roku");
    let rows = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).split(area);
    let cols = Layout::horizontal([Constraint::Min(0), Constraint::Length(app.ip.len() as u16)])
        .split(rows[0]);

    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(format!("{} ", dot), Style::default().fg(dot_color)),
            Span::styled(name, Style::default().fg(TEXT).add_modifier(Modifier::BOLD)),
        ])),
        cols[0],
    );
    f.render_widget(
        Paragraph::new(Span::styled(app.ip.as_str(), Style::default().fg(DIM))),
        cols[1],
    );

    let now_playing = match (&app.active_app, app.online) {
        (_, Some(false)) => Line::from(Span::styled("  offline", Style::default().fg(DIM))),
        (Some(a), _) => Line::from(vec![
            Span::styled("  ▸ ", Style::default().fg(ACCENT)),
            Span::styled(
                a.as_str(),
                Style::default().fg(DIM).add_modifier(Modifier::ITALIC),
            ),
        ]),
        (None, _) => Line::from(""),
    };
    f.render_widget(Paragraph::new(now_playing), rows[1]);
}

fn draw_button(f: &mut Frame, app: &App, area: Rect, b: &Button) {
    let t = app.flash_strength(b.id);
    let (base_bg, base_border, label_fg) = match b.kind {
        Kind::Normal => (BTN_BG, BTN_BORDER, TEXT),
        Kind::Primary => (ACCENT_BG, ACCENT, TEXT),
        Kind::Danger => (BTN_BG, BTN_BORDER, DANGER),
    };
    let bg = blend(base_bg, PRESS_BG, t);
    let border = blend(base_border, ACCENT, t.max(if t > 0.0 { 0.6 } else { 0.0 }));
    let fg = blend(label_fg, Color::Rgb(255, 255, 255), t);
    let hint_fg = blend(DIM, Color::Rgb(230, 220, 255), t);

    let label = Line::from(vec![
        Span::styled(
            b.label,
            Style::default().fg(fg).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("  {}", b.hint), Style::default().fg(hint_fg)),
    ])
    .alignment(Alignment::Center);

    if area.height >= 3 {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(border).bg(CARD_BG))
            .style(Style::default().bg(bg));
        let inner = block.inner(area);
        f.render_widget(block, area);
        let y = inner.y + inner.height.saturating_sub(1) / 2;
        f.render_widget(
            Paragraph::new(label),
            Rect {
                y,
                height: 1,
                ..inner
            },
        );
    } else {
        f.render_widget(Paragraph::new(label).style(Style::default().bg(bg)), area);
    }
}

fn draw_status(f: &mut Frame, app: &App, area: Rect) {
    let (icon, color) = match app.tone {
        _ if app.discovering || (app.in_flight > 0 && app.tone == Tone::Busy) => {
            (app.spinner(), WARN)
        }
        Tone::Busy => (app.spinner(), WARN),
        Tone::Good => ("✓", GOOD),
        Tone::Bad => ("✗", DANGER),
        Tone::Info => ("•", ACCENT),
    };
    // Successful sends fade back to a quiet grey; errors stay loud.
    let age = app.status_at.elapsed();
    let fade = if app.tone == Tone::Good && age > STATUS_FADE {
        ((age - STATUS_FADE).as_secs_f32() / 0.6).min(1.0)
    } else {
        0.0
    };
    let line = Line::from(vec![
        Span::styled(
            format!("{} ", icon),
            Style::default().fg(blend(color, DIM, fade)),
        ),
        Span::styled(
            app.status.as_str(),
            Style::default().fg(blend(TEXT, DIM, fade.max(0.25))),
        ),
    ])
    .alignment(Alignment::Center);
    f.render_widget(Paragraph::new(line), area);
}

fn draw_footer(f: &mut Frame, area: Rect) {
    let hints = [
        ("←↑↓→", "move"),
        ("⏎", "ok"),
        ("⌫", "back"),
        ("h", "home"),
        ("␣", "play"),
        ("+/-", "vol"),
        ("t", "type"),
        ("q", "quit"),
    ];
    let mut spans = Vec::new();
    for (i, (key, label)) in hints.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("  ", Style::default()));
        }
        spans.push(Span::styled(*key, Style::default().fg(ACCENT)));
        spans.push(Span::styled(
            format!(" {}", label),
            Style::default().fg(DIM),
        ));
    }
    f.render_widget(
        Paragraph::new(Line::from(spans).alignment(Alignment::Center)),
        area,
    );
}

fn draw_typing(f: &mut Frame, app: &App, card: Rect) {
    let buffer = app.typing.as_deref().unwrap_or("");
    let width = card.width.saturating_sub(4).max(20).min(f.area().width);
    let area = centered_rect(width, 5, f.area());
    let cursor_on = (app.started.elapsed().as_millis() / 530).is_multiple_of(2);

    // Keep the tail of long input visible.
    let room = width.saturating_sub(6) as usize;
    let chars: Vec<char> = buffer.chars().collect();
    let shown: String = chars[chars.len().saturating_sub(room)..].iter().collect();

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT))
        .title(Span::styled(
            " ⌨ Send text ",
            Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
        ))
        .title_bottom(
            Line::from(vec![
                Span::styled(" ⏎ ", Style::default().fg(ACCENT)),
                Span::styled("send  ", Style::default().fg(DIM)),
                Span::styled("esc ", Style::default().fg(ACCENT)),
                Span::styled("cancel ", Style::default().fg(DIM)),
            ])
            .alignment(Alignment::Right),
        )
        .style(Style::default().bg(CARD_BG));
    let text = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled(" › ", Style::default().fg(ACCENT)),
            Span::styled(shown, Style::default().fg(TEXT)),
            Span::styled(
                if cursor_on { "▏" } else { " " },
                Style::default().fg(ACCENT),
            ),
        ]),
    ];
    f.render_widget(Clear, area);
    f.render_widget(Paragraph::new(text).block(block), area);
}

fn draw_recovery(f: &mut Frame, recovery: &Recovery, screen: Rect) {
    let height = (recovery.devices.len() as u16 + 4).min(screen.height);
    let area = centered_rect(56, height, screen);
    let items = recovery.devices.iter().map(|device| {
        let label = if device.name.is_empty() {
            device.ip.clone()
        } else {
            format!("{} — {}", device.name, device.ip)
        };
        ListItem::new(Line::from(Span::styled(label, Style::default().fg(TEXT))))
    });
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(ACCENT))
                .title(Span::styled(
                    " Choose a Roku ",
                    Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
                ))
                .title_bottom(
                    Line::from(Span::styled(
                        " ↑↓ select · ⏎ connect · esc cancel ",
                        Style::default().fg(DIM),
                    ))
                    .alignment(Alignment::Center),
                )
                .padding(ratatui::widgets::Padding::vertical(1))
                .style(Style::default().bg(CARD_BG)),
        )
        .highlight_style(
            Style::default()
                .fg(Color::Rgb(255, 255, 255))
                .bg(PRESS_BG)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol(" ▶ ");
    let mut state = ListState::default().with_selected(Some(recovery.selected));
    f.render_widget(Clear, area);
    f.render_stateful_widget(list, area, &mut state);
}

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

fn resolve_ip(ip_override: Option<String>) -> Result<String> {
    if let Some(ip) = ip_override {
        return Ok(ip);
    }
    if let Ok(ip) = std::env::var("ROKU_IP") {
        if !ip.is_empty() {
            return Ok(ip);
        }
    }
    if let Some(ip) = Config::load()?.roku_ip {
        return Ok(ip);
    }
    eprintln!("No Roku IP configured, discovering via SSDP...");
    match discover::discover(3)?.into_iter().next() {
        Some(found) => {
            if let Ok(mut cfg) = Config::load() {
                cfg.roku_ip = Some(found.ip.clone());
                let _ = cfg.save();
            }
            eprintln!("Found Roku at {}", found.ip);
            Ok(found.ip)
        }
        None => bail!("no Roku found; set with `roku config set-ip <IP>` or ROKU_IP env var"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    fn render(app: &App, w: u16, h: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    fn press(app: &mut App, code: KeyCode) -> Outcome {
        handle_key(app, KeyEvent::new(code, KeyModifiers::NONE))
    }

    #[test]
    fn connection_errors_are_distinguished_from_protocol_errors() {
        let connection =
            anyhow::Error::new(io::Error::new(io::ErrorKind::ConnectionRefused, "refused"));
        assert!(is_connection_error(&connection));
        assert!(!is_connection_error(&anyhow::anyhow!(
            "Roku returned HTTP 403"
        )));
    }

    #[test]
    fn recovery_dialog_lists_each_discovered_device() {
        let mut app = App::new("192.0.2.1".to_string());
        app.recovery = Some(Recovery {
            devices: vec![
                DeviceChoice {
                    ip: "192.0.2.10".to_string(),
                    name: "Living Room".to_string(),
                },
                DeviceChoice {
                    ip: "192.0.2.11".to_string(),
                    name: "Bedroom".to_string(),
                },
            ],
            selected: 1,
            pending: None,
        });
        let rendered = render(&app, 80, 24);
        assert!(rendered.contains("Living Room — 192.0.2.10"));
        assert!(rendered.contains("Bedroom — 192.0.2.11"));
    }

    #[test]
    fn remote_shows_text_entry_shortcut_at_every_size() {
        let app = App::new("192.0.2.1".to_string());
        for (w, h) in [(80, 40), (80, 24), (50, 16)] {
            let rendered = render(&app, w, h);
            assert!(rendered.contains("Type"), "missing at {}x{}", w, h);
            assert!(rendered.contains("OK"), "missing OK at {}x{}", w, h);
        }
    }

    #[test]
    fn tiny_terminal_does_not_panic() {
        let app = App::new("192.0.2.1".to_string());
        render(&app, 10, 4);
        render(&app, 1, 1);
    }

    #[test]
    fn keypress_queues_command_and_flashes_button() {
        let mut app = App::new("192.0.2.1".to_string());
        match press(&mut app, KeyCode::Up) {
            Outcome::Send(Cmd::Do(PendingAction::Keypress(k))) => assert_eq!(k, "Up"),
            _ => panic!("expected keypress"),
        }
        assert!(app.flash_strength("Up") > 0.9);
        assert_eq!(app.flash_strength("Down"), 0.0);
    }

    #[test]
    fn typing_mode_buffers_until_enter() {
        let mut app = App::new("192.0.2.1".to_string());
        assert!(matches!(
            press(&mut app, KeyCode::Char('t')),
            Outcome::Continue
        ));
        for c in "hi q".chars() {
            assert!(matches!(
                press(&mut app, KeyCode::Char(c)),
                Outcome::Continue
            ));
        }
        match press(&mut app, KeyCode::Enter) {
            Outcome::Send(Cmd::Do(PendingAction::TypeText(t))) => assert_eq!(t, "hi q"),
            _ => panic!("expected text"),
        }
        assert!(app.typing.is_none());
    }

    #[test]
    fn info_for_stale_ip_is_ignored() {
        let mut app = App::new("192.0.2.1".to_string());
        app.apply(Msg::Info {
            ip: "192.0.2.99".to_string(),
            online: true,
            name: Some("Other".to_string()),
            app: None,
        });
        assert!(app.device_name.is_none());
        app.apply(Msg::Info {
            ip: "192.0.2.1".to_string(),
            online: true,
            name: Some("Den".to_string()),
            app: Some("Netflix".to_string()),
        });
        assert_eq!(app.device_name.as_deref(), Some("Den"));
        assert!(render(&app, 80, 40).contains("Netflix"));
    }
}
