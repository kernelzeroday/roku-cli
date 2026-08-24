use std::io;
use std::time::Duration;

use anyhow::{bail, Result};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph},
    Frame, Terminal,
};

use crate::config::Config;
use crate::discover;
use crate::ecp::{self, Roku};

pub fn run(ip_override: Option<String>) -> Result<()> {
    let ip = resolve_ip(ip_override)?;
    let roku = Roku::new(&ip);

    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;

    let result = event_loop(&mut terminal, roku, ip);

    disable_raw_mode()?;
    execute!(io::stdout(), LeaveAlternateScreen)?;

    result
}

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
            Self::Keypress(key) => format!("Sent: {}", key),
            Self::TypeText(text) => format!("Typed: {}", text),
        }
    }
}

struct Recovery {
    devices: Vec<DeviceChoice>,
    selected: usize,
    pending: Option<PendingAction>,
}

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    mut roku: Roku,
    mut ip: String,
) -> Result<()> {
    let mut status = "Connecting...".to_string();
    let mut highlight: Option<String> = None;
    let mut text_mode = false;
    let mut text_buffer = String::new();
    let mut recovery = None;

    terminal.draw(|f| draw(f, &ip, &status, None, None, None))?;
    match roku.device_info() {
        Ok(_) => status = "Ready".to_string(),
        Err(error) if is_connection_error(&error) => {
            begin_recovery(&mut roku, &mut ip, None, error, &mut status, &mut recovery);
        }
        Err(error) => status = format!("Error: {}", error),
    }

    loop {
        terminal.draw(|f| {
            draw(
                f,
                &ip,
                &status,
                highlight.as_deref(),
                if text_mode { Some(&text_buffer) } else { None },
                recovery.as_ref(),
            )
        })?;

        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                    return Ok(());
                }

                if recovery.is_some() {
                    match key.code {
                        KeyCode::Esc => {
                            recovery = None;
                            status = "Device selection cancelled".to_string();
                        }
                        KeyCode::Char('q') => return Ok(()),
                        KeyCode::Up => {
                            let recovery = recovery.as_mut().expect("checked above");
                            recovery.selected = recovery.selected.saturating_sub(1);
                        }
                        KeyCode::Down => {
                            let recovery = recovery.as_mut().expect("checked above");
                            recovery.selected =
                                (recovery.selected + 1).min(recovery.devices.len() - 1);
                        }
                        KeyCode::Enter => {
                            let recovery_state = recovery.take().expect("checked above");
                            let choice = &recovery_state.devices[recovery_state.selected];
                            activate_device(
                                &mut roku,
                                &mut ip,
                                choice,
                                recovery_state.pending,
                                &mut status,
                            );
                        }
                        _ => {}
                    }
                    continue;
                }

                if text_mode {
                    match key.code {
                        KeyCode::Esc => {
                            text_mode = false;
                            text_buffer.clear();
                            status = "Cancelled".to_string();
                        }
                        KeyCode::Enter => {
                            text_mode = false;
                            if !text_buffer.is_empty() {
                                let action = PendingAction::TypeText(text_buffer.clone());
                                send_or_recover(
                                    &mut roku,
                                    &mut ip,
                                    action,
                                    &mut status,
                                    &mut recovery,
                                );
                            }
                            text_buffer.clear();
                        }
                        KeyCode::Backspace => {
                            text_buffer.pop();
                        }
                        KeyCode::Char(c) => {
                            text_buffer.push(c);
                        }
                        _ => {}
                    }
                    continue;
                }

                let ecp_key = match key.code {
                    KeyCode::Char('q') => return Ok(()),
                    KeyCode::Up => Some("Up"),
                    KeyCode::Down => Some("Down"),
                    KeyCode::Left => Some("Left"),
                    KeyCode::Right => Some("Right"),
                    KeyCode::Enter => Some("Select"),
                    KeyCode::Backspace => Some("Back"),
                    KeyCode::Esc => Some("Home"),
                    KeyCode::Char(' ') | KeyCode::Char('p') => Some("Play"),
                    KeyCode::Char('h') => Some("Home"),
                    KeyCode::Char('m') => Some("VolumeMute"),
                    KeyCode::Char('+') | KeyCode::Char('=') => Some("VolumeUp"),
                    KeyCode::Char('-') | KeyCode::Char('_') => Some("VolumeDown"),
                    KeyCode::Char('i') => Some("Info"),
                    KeyCode::Char('r') => Some("InstantReplay"),
                    KeyCode::Char('s') => Some("Search"),
                    KeyCode::Char('f') => Some("Fwd"),
                    KeyCode::Char('w') => Some("Rev"),
                    KeyCode::Char('o') => Some("PowerOff"),
                    KeyCode::Char('O') => Some("PowerOn"),
                    KeyCode::Char('t') => {
                        text_mode = true;
                        status = "Typing (Enter:send  Esc:cancel)".to_string();
                        highlight = None;
                        None
                    }
                    _ => None,
                };
                if let Some(key) = ecp_key {
                    highlight = Some(key.to_string());
                    send_or_recover(
                        &mut roku,
                        &mut ip,
                        PendingAction::Keypress(key.to_string()),
                        &mut status,
                        &mut recovery,
                    );
                }
            }
        }
    }
}

fn send_or_recover(
    roku: &mut Roku,
    ip: &mut String,
    action: PendingAction,
    status: &mut String,
    recovery: &mut Option<Recovery>,
) {
    match action.execute(roku) {
        Ok(()) => *status = action.success_message(),
        Err(error) if is_connection_error(&error) => {
            begin_recovery(roku, ip, Some(action), error, status, recovery);
        }
        Err(error) => *status = format!("Error: {}", error),
    }
}

fn begin_recovery(
    roku: &mut Roku,
    ip: &mut String,
    pending: Option<PendingAction>,
    connection_error: anyhow::Error,
    status: &mut String,
    recovery: &mut Option<Recovery>,
) {
    let found = match discover::discover(3) {
        Ok(found) => found,
        Err(error) => {
            *status = format!(
                "Connection failed: {}; discovery failed: {}",
                connection_error, error
            );
            return;
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
        0 => {
            *status = format!(
                "Connection failed: {}; no Rokus discovered",
                connection_error
            );
        }
        1 => activate_device(roku, ip, &devices[0], pending, status),
        _ => {
            *status = format!("Connection failed: {}; choose a Roku", connection_error);
            *recovery = Some(Recovery {
                devices,
                selected: 0,
                pending,
            });
        }
    }
}

fn activate_device(
    roku: &mut Roku,
    ip: &mut String,
    choice: &DeviceChoice,
    pending: Option<PendingAction>,
    status: &mut String,
) {
    *roku = Roku::new(&choice.ip);
    *ip = choice.ip.clone();

    let save_error = Config::load()
        .and_then(|mut config| {
            config.roku_ip = Some(choice.ip.clone());
            config.save()
        })
        .err();

    let result = pending.as_ref().map(|action| action.execute(roku));
    *status = match (result, save_error) {
        (Some(Ok(())), None) => format!(
            "Reconnected to {}; {}",
            choice.ip,
            pending
                .as_ref()
                .expect("result came from pending")
                .success_message()
        ),
        (Some(Ok(())), Some(error)) => {
            format!("Reconnected to {}; could not save IP: {}", choice.ip, error)
        }
        (Some(Err(error)), _) => format!("Found {}, but retry failed: {}", choice.ip, error),
        (None, None) => format!("Connected to {} (saved to config)", choice.ip),
        (None, Some(error)) => {
            format!("Connected to {}; could not save IP: {}", choice.ip, error)
        }
    };
}

fn is_connection_error(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.downcast_ref::<io::Error>().is_some())
}

fn draw(
    f: &mut Frame,
    ip: &str,
    status: &str,
    highlight: Option<&str>,
    typing: Option<&str>,
    recovery: Option<&Recovery>,
) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(6),
        ])
        .split(f.area());

    let title = Paragraph::new(Line::from(vec![
        Span::styled(
            " Roku Remote",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" — {} ", ip), Style::default().fg(Color::DarkGray)),
    ]))
    .block(Block::default().borders(Borders::ALL));
    f.render_widget(title, chunks[0]);

    let hl = |key: &str, label: &str| -> Span {
        let is_hl = highlight
            .is_some_and(|h| h == key || (key == "Power" && (h == "PowerOff" || h == "PowerOn")));
        if is_hl {
            Span::styled(
                format!(" {} ", label),
                Style::default().fg(Color::Black).bg(Color::Cyan),
            )
        } else {
            Span::styled(format!(" {} ", label), Style::default().fg(Color::White))
        }
    };

    let lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::raw("   "),
            hl("Power", "⏻ Power (o/O)"),
            Span::raw("          "),
            hl("Home", "⌂ Home (h)"),
        ]),
        Line::from(vec![
            Span::raw("                            "),
            hl("Back", "← Back (bksp)"),
        ]),
        Line::from(""),
        Line::from(vec![Span::raw("                  "), hl("Up", "  ▲ (↑)  ")]),
        Line::from(vec![
            Span::raw("       "),
            hl("Left", "  ◀ (←)  "),
            Span::raw(""),
            hl("Select", " OK (⏎) "),
            Span::raw(""),
            hl("Right", "  ▶ (→)  "),
        ]),
        Line::from(vec![
            Span::raw("                  "),
            hl("Down", "  ▼ (↓)  "),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::raw("       "),
            hl("Rev", " ⏪ (w) "),
            Span::raw("   "),
            hl("Play", " ⏯ (space) "),
            Span::raw("   "),
            hl("Fwd", " ⏩ (f) "),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::raw("       "),
            hl("VolumeDown", " Vol- (-) "),
            Span::raw("  "),
            hl("VolumeMute", " Mute (m) "),
            Span::raw("  "),
            hl("VolumeUp", " Vol+ (+) "),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::raw("       "),
            hl("Info", " Info (i) "),
            Span::raw(" "),
            hl("InstantReplay", " Replay (r) "),
            Span::raw(" "),
            hl("Search", " Search (s) "),
        ]),
    ];

    let remote =
        Paragraph::new(lines).block(Block::default().borders(Borders::LEFT | Borders::RIGHT));
    f.render_widget(remote, chunks[1]);

    let status_line = match typing {
        Some(buf) => Line::from(vec![
            Span::styled("  Type: ", Style::default().fg(Color::Yellow)),
            Span::styled(buf, Style::default().fg(Color::White)),
            Span::styled("█", Style::default().fg(Color::Yellow)),
        ]),
        None => Line::from(Span::styled(
            format!("  {}", status),
            Style::default().fg(Color::Green),
        )),
    };

    let help = Paragraph::new(vec![
        Line::from(""),
        status_line,
        Line::from(Span::styled(
            "  ←↑↓→:Nav  Enter:OK  Bksp:Back  Esc/h:Home  Space/p:Play",
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(Span::styled(
            "  m:Mute  +/-:Vol  w:Rew  f:Fwd  i:Info  r:Replay  t:Type  q:Quit",
            Style::default().fg(Color::DarkGray),
        )),
    ])
    .block(Block::default().borders(Borders::ALL));
    f.render_widget(help, chunks[2]);
    if let Some(recovery) = recovery {
        let height = (recovery.devices.len() as u16 + 2).min(f.area().height);
        let area = centered_rect(56, height, f.area());
        let items = recovery.devices.iter().map(|device| {
            let label = if device.name.is_empty() {
                device.ip.clone()
            } else {
                format!("{} — {}", device.name, device.ip)
            };
            ListItem::new(label)
        });
        let list = List::new(items)
            .block(
                Block::default()
                    .title(" Choose Roku (↑/↓, Enter, Esc) ")
                    .borders(Borders::ALL),
            )
            .highlight_style(
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("▶ ");
        let mut state = ListState::default().with_selected(Some(recovery.selected));
        f.render_widget(Clear, area);
        f.render_stateful_widget(list, area, &mut state);
    }
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
        let recovery = Recovery {
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
        };
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| {
                draw(
                    frame,
                    "192.0.2.1",
                    "choose a Roku",
                    None,
                    None,
                    Some(&recovery),
                )
            })
            .unwrap();

        let rendered: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(rendered.contains("Living Room — 192.0.2.10"));
        assert!(rendered.contains("Bedroom — 192.0.2.11"));
    }

    #[test]
    fn help_lists_text_entry_shortcut() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| draw(frame, "192.0.2.1", "Ready", None, None, None))
            .unwrap();

        let rendered: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(rendered.contains("t:Type"));
    }
}
