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
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame, Terminal,
};

use crate::config::Config;
use crate::discover;
use crate::ecp::Roku;

pub fn run(ip_override: Option<String>) -> Result<()> {
    let ip = resolve_ip(ip_override)?;
    let roku = Roku::new(&ip);

    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;

    let result = event_loop(&mut terminal, &roku, &ip);

    disable_raw_mode()?;
    execute!(io::stdout(), LeaveAlternateScreen)?;

    result
}

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    roku: &Roku,
    ip: &str,
) -> Result<()> {
    let mut status = "Ready".to_string();
    let mut highlight: Option<String> = None;

    loop {
        terminal.draw(|f| draw(f, ip, &status, highlight.as_deref()))?;

        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                let ecp_key = match key.code {
                    KeyCode::Char('q') => return Ok(()),
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        return Ok(())
                    }
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
                    _ => None,
                };
                if let Some(k) = ecp_key {
                    match roku.keypress(k) {
                        Ok(()) => status = format!("Sent: {}", k),
                        Err(e) => status = format!("Error: {}", e),
                    }
                    highlight = Some(k.to_string());
                }
            }
        }
    }
}

fn draw(f: &mut Frame, ip: &str, status: &str, highlight: Option<&str>) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(5),
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
        let is_hl = highlight.is_some_and(|h| {
            h == key || (key == "Power" && (h == "PowerOff" || h == "PowerOn"))
        });
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
        Line::from(vec![Span::raw("                  "), hl("Down", "  ▼ (↓)  ")]),
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

    let remote = Paragraph::new(lines)
        .block(Block::default().borders(Borders::LEFT | Borders::RIGHT));
    f.render_widget(remote, chunks[1]);

    let help = Paragraph::new(vec![
        Line::from(""),
        Line::from(Span::styled(
            format!("  {}", status),
            Style::default().fg(Color::Green),
        )),
        Line::from(Span::styled(
            "  ←↑↓→:Nav  Enter:OK  Bksp:Back  Esc/h:Home  Space/p:Play",
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(Span::styled(
            "  m:Mute  +/-:Vol  w:Rew  f:Fwd  i:Info  r:Replay  q:Quit",
            Style::default().fg(Color::DarkGray),
        )),
    ])
    .block(Block::default().borders(Borders::ALL));
    f.render_widget(help, chunks[2]);
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
