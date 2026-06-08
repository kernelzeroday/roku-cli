use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use roku_cli::config::Config;
use roku_cli::discover;
use roku_cli::ecp::{self, Roku};

#[derive(Parser)]
#[command(name = "roku", about = "Control a Roku device directly over ECP (LAN)")]
struct Cli {
    /// Roku IP address (overrides config, ROKU_IP env, and discovery)
    #[arg(long, global = true)]
    ip: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Send one or more keypresses (friendly names or raw ECP keys)
    Key { keys: Vec<String> },
    /// Type a string into the focused field (e.g. a search box)
    Text { text: Vec<String> },
    /// Launch an app by ID, optionally deep-linking into content
    Launch {
        /// App ID (see `roku apps`)
        app: String,
        #[arg(long)]
        content_id: Option<String>,
        #[arg(long)]
        media_type: Option<String>,
        /// Extra launch parameter, repeatable: --param key=value
        #[arg(long = "param", value_parser = parse_kv)]
        params: Vec<(String, String)>,
    },
    /// List installed apps with their IDs
    Apps,
    /// Show the currently active app
    Active,
    /// Show device info (name, model, OS, power)
    Info,
    /// Show media player / playback state
    Media,
    /// Power the TV on or off
    Power {
        #[arg(value_parser = ["on", "off"])]
        state: String,
    },
    /// Discover Rokus on the LAN via SSDP
    Discover,
    /// Manage saved configuration
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// Press Home
    Home {
        #[arg(default_value = "1")]
        count: u32,
    },
    /// Press Back
    Back {
        #[arg(default_value = "1")]
        count: u32,
    },
    /// Press Select (OK)
    #[command(alias = "ok")]
    Select {
        #[arg(default_value = "1")]
        count: u32,
    },
    /// Press Up
    Up {
        #[arg(default_value = "1")]
        count: u32,
    },
    /// Press Down
    Down {
        #[arg(default_value = "1")]
        count: u32,
    },
    /// Press Left
    Left {
        #[arg(default_value = "1")]
        count: u32,
    },
    /// Press Right
    Right {
        #[arg(default_value = "1")]
        count: u32,
    },
    /// Play / Pause toggle
    Play {
        #[arg(default_value = "1")]
        count: u32,
    },
    /// Rewind
    Rewind {
        #[arg(default_value = "1")]
        count: u32,
    },
    /// Fast-forward
    Forward {
        #[arg(default_value = "1")]
        count: u32,
    },
    /// Volume up (repeatable: `roku vol-up 5`)
    #[command(alias = "volup")]
    VolUp {
        /// Number of steps (default 1)
        #[arg(default_value = "1")]
        steps: u32,
    },
    /// Volume down (repeatable: `roku vol-down 5`)
    #[command(alias = "voldown")]
    VolDown {
        /// Number of steps (default 1)
        #[arg(default_value = "1")]
        steps: u32,
    },
    /// Set volume to an absolute level (0–100)
    #[command(alias = "volset")]
    VolSet {
        /// Target volume level
        level: u32,
    },
    /// Toggle mute
    Mute {
        #[arg(default_value = "1")]
        count: u32,
    },
    /// Launch interactive TUI remote
    #[cfg(feature = "tui")]
    Tui,
    /// Launch graphical GUI remote
    #[cfg(feature = "gui")]
    Gui,
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Show current configuration
    Show,
    /// Set the Roku IP address
    SetIp { ip: String },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match &cli.command {
        Command::Discover => return cmd_discover(),
        Command::Config { action } => return cmd_config(action),
        #[cfg(feature = "tui")]
        Command::Tui => return roku_cli::tui_app::run(cli.ip),
        #[cfg(feature = "gui")]
        Command::Gui => return roku_cli::gui::run(cli.ip),
        _ => {}
    }

    let ip = resolve_ip(cli.ip.clone())?;
    let roku = Roku::new(&ip);

    match cli.command {
        Command::Key { keys } => {
            for k in &keys {
                roku.keypress(&normalize_key(k))?;
            }
            println!("sent: {}", keys.join(" "));
        }
        Command::Text { text } => {
            let s = text.join(" ");
            roku.type_text(&s)?;
            println!("typed: {}", s);
        }
        Command::Launch {
            app,
            content_id,
            media_type,
            mut params,
        } => {
            if let Some(c) = content_id {
                params.push(("contentId".to_string(), c));
            }
            if let Some(m) = media_type {
                params.push(("mediaType".to_string(), m));
            }
            roku.launch(&app, &params)?;
            println!("launched app {}", app);
        }
        Command::Apps => {
            for a in roku.apps()? {
                let ver = a.version.map(|v| format!(" (v{})", v)).unwrap_or_default();
                println!("  {:>10}  {}{}", a.id, a.name, ver);
            }
        }
        Command::Active => println!("{}", roku.active_app_name()?),
        Command::Info => print_info(&roku)?,
        Command::Media => println!("{}", roku.media_player()?),
        Command::Power { state } => {
            let key = if state == "on" { "PowerOn" } else { "PowerOff" };
            roku.keypress(key)?;
            println!("power {}", state);
        }
        Command::Home { count } => repeat(&roku, "Home", count)?,
        Command::Back { count } => repeat(&roku, "Back", count)?,
        Command::Select { count } => repeat(&roku, "Select", count)?,
        Command::Up { count } => repeat(&roku, "Up", count)?,
        Command::Down { count } => repeat(&roku, "Down", count)?,
        Command::Left { count } => repeat(&roku, "Left", count)?,
        Command::Right { count } => repeat(&roku, "Right", count)?,
        Command::Play { count } => repeat(&roku, "Play", count)?,
        Command::Rewind { count } => repeat(&roku, "Rev", count)?,
        Command::Forward { count } => repeat(&roku, "Fwd", count)?,
        Command::VolUp { steps } => repeat(&roku, "VolumeUp", steps)?,
        Command::VolDown { steps } => repeat(&roku, "VolumeDown", steps)?,
        Command::VolSet { level } => {
            let level = level.min(100);
            repeat(&roku, "VolumeDown", 100)?;
            if level > 0 {
                repeat(&roku, "VolumeUp", level)?;
            }
            println!("volume set to {}", level);
        }
        Command::Mute { count } => repeat(&roku, "VolumeMute", count)?,
        Command::Discover | Command::Config { .. } => unreachable!(),
        #[cfg(feature = "tui")]
        Command::Tui => unreachable!(),
        #[cfg(feature = "gui")]
        Command::Gui => unreachable!(),
    }
    Ok(())
}

fn repeat(roku: &Roku, key: &str, steps: u32) -> Result<()> {
    for _ in 0..steps {
        roku.keypress(key)?;
    }
    println!("sent {} x{}", key, steps);
    Ok(())
}

fn normalize_key(input: &str) -> String {
    match input.to_lowercase().as_str() {
        "home" => "Home",
        "back" => "Back",
        "select" | "ok" => "Select",
        "enter" => "Enter",
        "up" => "Up",
        "down" => "Down",
        "left" => "Left",
        "right" => "Right",
        "play" | "pause" => "Play",
        "rev" | "rewind" => "Rev",
        "fwd" | "forward" => "Fwd",
        "replay" => "InstantReplay",
        "info" | "star" => "Info",
        "backspace" => "Backspace",
        "search" => "Search",
        "volup" | "vol+" => "VolumeUp",
        "voldown" | "vol-" => "VolumeDown",
        "mute" => "VolumeMute",
        "poweron" => "PowerOn",
        "poweroff" => "PowerOff",
        "power" => "Power",
        "channelup" | "chup" => "ChannelUp",
        "channeldown" | "chdown" => "ChannelDown",
        _ => return input.to_string(),
    }
    .to_string()
}

fn parse_kv(s: &str) -> std::result::Result<(String, String), String> {
    match s.split_once('=') {
        Some((k, v)) => Ok((k.to_string(), v.to_string())),
        None => Err(format!("expected key=value, got '{}'", s)),
    }
}

fn resolve_ip(flag: Option<String>) -> Result<String> {
    if let Some(ip) = flag {
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
    eprintln!("No Roku IP set; discovering via SSDP...");
    match discover::discover(3)?.into_iter().next() {
        Some(found) => {
            let mut cfg = Config::load()?;
            cfg.roku_ip = Some(found.ip.clone());
            let _ = cfg.save();
            eprintln!("Found Roku at {} (saved to config)", found.ip);
            Ok(found.ip)
        }
        None => bail!("no Roku found; set one with `roku config set-ip <ip>` or pass --ip"),
    }
}

fn cmd_discover() -> Result<()> {
    let found = discover::discover(3)?;
    if found.is_empty() {
        println!("No Rokus found on the LAN.");
        return Ok(());
    }
    for f in &found {
        let name = Roku::new(&f.ip)
            .device_info()
            .ok()
            .and_then(|xml| ecp::tag_text(&xml, "friendly-device-name"))
            .unwrap_or_default();
        println!("  {}  {}", f.ip, name);
    }
    Ok(())
}

fn cmd_config(action: &ConfigAction) -> Result<()> {
    match action {
        ConfigAction::Show => {
            let cfg = Config::load()?;
            println!("roku_ip: {}", cfg.roku_ip.as_deref().unwrap_or("(unset)"));
        }
        ConfigAction::SetIp { ip } => {
            let mut cfg = Config::load()?;
            cfg.roku_ip = Some(ip.clone());
            cfg.save()?;
            println!("saved roku_ip = {}", ip);
        }
    }
    Ok(())
}

fn print_info(roku: &Roku) -> Result<()> {
    let xml = roku.device_info()?;
    let fields = [
        ("Name", "friendly-device-name"),
        ("Model", "model-name"),
        ("Model #", "model-number"),
        ("Serial", "serial-number"),
        ("Roku OS", "software-version"),
        ("Power", "power-mode"),
        ("Network", "network-type"),
        ("Resolution", "ui-resolution"),
    ];
    for (label, tag) in fields {
        if let Some(v) = ecp::tag_text(&xml, tag) {
            println!("  {:<11} {}", label, v);
        }
    }
    Ok(())
}
