//! Keyboard-first terminal connection launcher.
use crate::profiles::{Profile, Store};
use crossterm::{
    cursor::{Hide, MoveTo, Show},
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute, queue,
    style::{Attribute, Print, SetAttribute},
    terminal::{
        self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
        enable_raw_mode,
    },
};
use std::io::{self, IsTerminal, Stdout, Write};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

type Error = Box<dyn std::error::Error>;
const MIN_WIDTH: u16 = 80;
const MIN_HEIGHT: u16 = 24;

pub enum Outcome {
    Connect(Vec<String>),
    Quit,
}

pub struct CertificatePrompt {
    pub destination: String,
    pub subject: String,
    pub issuer: String,
    pub valid_from: String,
    pub valid_until: String,
    pub fingerprint: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CertificateChoice {
    Cancel,
    Once,
    Trust,
}

pub fn interactive_terminal() -> bool {
    io::stdin().is_terminal() && io::stdout().is_terminal()
}

pub fn run(message: Option<String>, initial: Option<&[String]>) -> Result<Outcome, Error> {
    let store = Store::discover()?;
    let profiles = store.load()?;
    let mut app = App::new(profiles, message, initial);
    let mut persisted = app.profiles.clone();
    let mut terminal = Terminal::enter()?;
    loop {
        app.draw(&mut terminal.out)?;
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            continue;
        }
        match app.key(key) {
            Command::None => {}
            Command::Persist => match store.save(&app.profiles) {
                Ok(()) => {
                    persisted.clone_from(&app.profiles);
                }
                Err(error) => {
                    app.profiles.clone_from(&persisted);
                    app.selected = app.selected.min(app.profiles.len().saturating_sub(1));
                    app.editing = app.profiles.get(app.selected).map(|_| app.selected);
                    app.status = format!("Could not save; no changes kept: {error}");
                }
            },
            Command::Connect(arguments) => return Ok(Outcome::Connect(arguments)),
            Command::Quit => return Ok(Outcome::Quit),
        }
    }
}

pub fn confirm_certificate(prompt: &CertificatePrompt) -> Result<CertificateChoice, Error> {
    let mut dialog = CertificateDialog::default();
    let mut terminal = Terminal::enter()?;
    loop {
        dialog.draw(&mut terminal.out, prompt)?;
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
            && let Some(choice) = dialog.key(key)
        {
            return Ok(choice);
        }
    }
}

struct Terminal {
    out: Stdout,
    raw: bool,
    alternate: bool,
}

impl Terminal {
    fn enter() -> Result<Self, Error> {
        enable_raw_mode()?;
        let mut terminal = Self {
            out: io::stdout(),
            raw: true,
            alternate: false,
        };
        execute!(terminal.out, EnterAlternateScreen)?;
        terminal.alternate = true;
        execute!(terminal.out, Hide)?;
        Ok(terminal)
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        if self.alternate {
            let _ = execute!(self.out, Show, LeaveAlternateScreen);
        }
        if self.raw {
            let _ = disable_raw_mode();
        }
    }
}

#[derive(Default)]
struct CertificateDialog {
    selected: usize,
}

impl CertificateDialog {
    fn key(&mut self, key: KeyEvent) -> Option<CertificateChoice> {
        match key.code {
            KeyCode::Esc | KeyCode::Char('c') | KeyCode::Char('C') => {
                Some(CertificateChoice::Cancel)
            }
            KeyCode::Char('o') | KeyCode::Char('O') => Some(CertificateChoice::Once),
            KeyCode::Char('t') | KeyCode::Char('T') => Some(CertificateChoice::Trust),
            KeyCode::Right | KeyCode::Tab => {
                self.selected = (self.selected + 1) % 3;
                None
            }
            KeyCode::Left | KeyCode::BackTab => {
                self.selected = self.selected.checked_sub(1).unwrap_or(2);
                None
            }
            KeyCode::Enter => Some(
                [
                    CertificateChoice::Cancel,
                    CertificateChoice::Once,
                    CertificateChoice::Trust,
                ][self.selected],
            ),
            _ => None,
        }
    }

    fn draw(&self, out: &mut Stdout, prompt: &CertificatePrompt) -> io::Result<()> {
        let (width, height) = terminal::size()?;
        queue!(out, MoveTo(0, 0), Clear(ClearType::All))?;
        if width < MIN_WIDTH || height < MIN_HEIGHT {
            queue!(
                out,
                Print("Certificate review needs a terminal at least 80×24. Resize or press Esc.")
            )?;
            return out.flush();
        }
        line(out, 2, 1, "Review remote desktop certificate", true)?;
        line(
            out,
            2,
            3,
            "This computer uses a certificate your system does not recognize.",
            false,
        )?;
        detail(out, 2, 5, width, "Destination", &prompt.destination)?;
        detail(out, 2, 7, width, "Subject", &prompt.subject)?;
        detail(out, 2, 9, width, "Issuer", &prompt.issuer)?;
        detail(
            out,
            2,
            11,
            width,
            "Valid",
            &format!("{} to {}", prompt.valid_from, prompt.valid_until),
        )?;
        line(out, 2, 13, "SHA-256 fingerprint", true)?;
        for (row, chunk) in prompt.fingerprint.as_bytes().chunks(76).enumerate() {
            line(
                out,
                2,
                14 + row as u16,
                std::str::from_utf8(chunk).unwrap_or("invalid fingerprint"),
                false,
            )?;
        }
        line(
            out,
            2,
            18,
            "No password sent. Trust and save remembers this computer's certificate.",
            false,
        )?;
        for (index, (x, label)) in [(2, "Cancel"), (18, "Connect once"), (40, "Trust and save")]
            .into_iter()
            .enumerate()
        {
            button(out, x, 20, label, self.selected == index)?;
        }
        line(
            out,
            2,
            22,
            "Left/Right or Tab chooses · Enter confirms · Esc cancels",
            false,
        )?;
        out.flush()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Focus {
    Saved,
    Computer,
    User,
    Connect,
    Reconnect,
    Options,
    Save,
    SaveAs,
    New,
    Delete,
    Port,
    Size,
    Dynamic,
    Graphics,
    Protocol,
    Clipboard,
    Trust,
    TrustValue,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Trust {
    System,
    Ca,
    SavedPin,
}

impl Trust {
    fn next(self) -> Self {
        match self {
            Self::System => Self::Ca,
            Self::Ca => Self::System,
            Self::SavedPin => Self::System,
        }
    }
    fn label(self) -> &'static str {
        match self {
            Self::System => "System trust",
            Self::Ca => "CA file",
            Self::SavedPin => "Saved fingerprint",
        }
    }
}

#[derive(Clone, Debug)]
struct Form {
    computer: String,
    user: String,
    port: String,
    size: String,
    dynamic: bool,
    h264: bool,
    vnc: bool,
    clipboard: bool,
    trust: Trust,
    trust_value: String,
    fingerprint: Option<String>,
}

impl Default for Form {
    fn default() -> Self {
        Self {
            computer: String::new(),
            user: String::new(),
            port: "3389".into(),
            size: "1024x768".into(),
            dynamic: true,
            h264: false,
            vnc: false,
            clipboard: true,
            trust: Trust::System,
            trust_value: String::new(),
            fingerprint: None,
        }
    }
}

impl Form {
    fn from_profile(profile: &Profile) -> Self {
        let (trust, trust_value) = if let Some(path) = &profile.ca {
            (Trust::Ca, path.clone())
        } else if profile.fingerprint.is_some() {
            (Trust::SavedPin, String::new())
        } else {
            (Trust::System, String::new())
        };
        Self {
            computer: profile.computer.clone(),
            user: profile.user.clone(),
            port: profile.port.to_string(),
            size: profile.size.clone().unwrap_or_else(|| "1024x768".into()),
            dynamic: profile.dynamic_resolution,
            h264: profile.h264,
            vnc: profile.vnc,
            clipboard: profile.clipboard,
            trust,
            trust_value,
            fingerprint: profile.fingerprint.clone(),
        }
    }

    fn profile(&self, name: String) -> Result<Profile, Error> {
        let port = self.port.parse()?;
        let profile = Profile {
            name,
            computer: self.computer.trim().into(),
            user: self.user.trim().into(),
            port,
            size: (!self.size.trim().is_empty()).then(|| self.size.trim().into()),
            dynamic_resolution: self.dynamic,
            h264: self.h264,
            vnc: self.vnc,
            clipboard: self.clipboard,
            ca: (self.trust == Trust::Ca).then(|| self.trust_value.trim().into()),
            fingerprint: (self.trust == Trust::SavedPin)
                .then(|| self.fingerprint.clone())
                .flatten(),
        };
        profile.validate()?;
        Ok(profile)
    }
}

#[derive(Debug, Eq, PartialEq)]
enum Modal {
    Name {
        value: String,
        replace: Option<usize>,
    },
    Delete,
}

struct App {
    profiles: Vec<Profile>,
    selected: usize,
    form: Form,
    focus: Focus,
    advanced: bool,
    status: String,
    modal: Option<Modal>,
    editing: Option<usize>,
    retry: Option<Vec<String>>,
}

#[derive(Debug, Eq, PartialEq)]
enum Command {
    None,
    Persist,
    Connect(Vec<String>),
    Quit,
}

impl App {
    fn new(profiles: Vec<Profile>, message: Option<String>, initial: Option<&[String]>) -> Self {
        let focus = if profiles.is_empty() {
            Focus::Computer
        } else {
            Focus::Saved
        };
        let mut app = Self {
            profiles,
            selected: 0,
            form: Form::default(),
            focus,
            advanced: false,
            status: message.unwrap_or_else(|| {
                "Tab/Arrows move · Enter chooses · Ctrl+S saves · Ctrl+N creates · Esc quits".into()
            }),
            modal: None,
            editing: None,
            retry: initial.map(<[String]>::to_vec),
        };
        if let Some(args) = initial
            && let Ok(options) = crate::Options::parse(args)
        {
            app.form = Form {
                computer: options.host,
                user: options.user.unwrap_or_default(),
                port: options.port.to_string(),
                size: options
                    .size
                    .map_or_else(|| "1024x768".into(), |(w, h)| format!("{w}x{h}")),
                dynamic: options.dynamic_resolution,
                h264: options.h264,
                vnc: options.vnc,
                clipboard: options.clipboard,
                trust: if options.ca_file.is_some() {
                    Trust::Ca
                } else if options.fingerprint.is_some() {
                    Trust::SavedPin
                } else {
                    Trust::System
                },
                trust_value: options
                    .ca_file
                    .map_or_else(String::new, |path| path.to_string_lossy().into_owned()),
                fingerprint: options.fingerprint.map(|pin| pin.to_string()),
            };
        }
        app
    }

    fn focuses(&self) -> &'static [Focus] {
        const BASIC: &[Focus] = &[
            Focus::Saved,
            Focus::Protocol,
            Focus::Computer,
            Focus::User,
            Focus::Connect,
            Focus::Reconnect,
            Focus::Options,
            Focus::Save,
            Focus::SaveAs,
            Focus::New,
            Focus::Delete,
        ];
        const ADVANCED: &[Focus] = &[
            Focus::Saved,
            Focus::Protocol,
            Focus::Computer,
            Focus::User,
            Focus::Connect,
            Focus::Reconnect,
            Focus::Options,
            Focus::Save,
            Focus::SaveAs,
            Focus::New,
            Focus::Delete,
            Focus::Port,
            Focus::Size,
            Focus::Dynamic,
            Focus::Clipboard,
            Focus::Graphics,
            Focus::Trust,
            Focus::TrustValue,
        ];
        const VNC: &[Focus] = &[
            Focus::Saved,
            Focus::Protocol,
            Focus::Computer,
            Focus::User,
            Focus::Connect,
            Focus::Reconnect,
            Focus::Options,
            Focus::Save,
            Focus::SaveAs,
            Focus::New,
            Focus::Delete,
            Focus::Port,
        ];
        if self.form.vnc {
            if self.advanced {
                VNC
            } else {
                &VNC[..VNC.len() - 1]
            }
        } else if self.advanced {
            ADVANCED
        } else {
            BASIC
        }
    }

    fn move_focus(&mut self, backwards: bool) {
        let fields = self.focuses();
        let current = fields
            .iter()
            .position(|field| *field == self.focus)
            .unwrap_or(0);
        let next = if backwards {
            current.checked_sub(1).unwrap_or(fields.len() - 1)
        } else {
            (current + 1) % fields.len()
        };
        self.focus = fields[next];
    }

    fn key(&mut self, key: KeyEvent) -> Command {
        if let Some(modal) = self.modal.take() {
            return self.modal_key(modal, key);
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Command::Quit;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('u') {
            if let Some(value) = self.active_text() {
                value.clear();
            }
            return Command::None;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('n') {
            return self.new_connection();
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('s') {
            return self.save();
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('r') {
            return self.reconnect();
        }
        match key.code {
            KeyCode::Esc => Command::Quit,
            KeyCode::Tab => {
                self.move_focus(key.modifiers.contains(KeyModifiers::SHIFT));
                Command::None
            }
            KeyCode::BackTab => {
                self.move_focus(true);
                Command::None
            }
            KeyCode::Up if self.focus == Focus::Saved => {
                self.selected = self.selected.saturating_sub(1);
                Command::None
            }
            KeyCode::Down if self.focus == Focus::Saved => {
                if self.selected + 1 < self.profiles.len() {
                    self.selected += 1;
                }
                Command::None
            }
            KeyCode::Home if self.focus == Focus::Saved => {
                self.selected = 0;
                Command::None
            }
            KeyCode::End if self.focus == Focus::Saved => {
                self.selected = self.profiles.len().saturating_sub(1);
                Command::None
            }
            KeyCode::PageUp if self.focus == Focus::Saved => {
                self.selected = self.selected.saturating_sub(10);
                Command::None
            }
            KeyCode::PageDown if self.focus == Focus::Saved => {
                self.selected = (self.selected + 10).min(self.profiles.len().saturating_sub(1));
                Command::None
            }
            KeyCode::Up | KeyCode::Left => {
                self.move_focus(true);
                Command::None
            }
            KeyCode::Down | KeyCode::Right => {
                self.move_focus(false);
                Command::None
            }
            KeyCode::Enter => self.activate(),
            KeyCode::Char(' ') if self.active_text().is_none() => self.activate(),
            KeyCode::Backspace => {
                if let Some(value) = self.active_text() {
                    value.pop();
                }
                Command::None
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                if let Some(value) = self.active_text()
                    && value.len() < 1024
                    && !character.is_control()
                {
                    value.push(character);
                }
                Command::None
            }
            _ => Command::None,
        }
    }

    fn activate(&mut self) -> Command {
        match self.focus {
            Focus::Saved => {
                if let Some(profile) = self.profiles.get(self.selected) {
                    self.form = Form::from_profile(profile);
                    self.editing = Some(self.selected);
                    self.status = format!("Editing {}. Update saves changes.", profile.name);
                    self.focus = Focus::Computer;
                } else {
                    self.status = "No saved connection selected.".into();
                }
                Command::None
            }
            Focus::New => self.new_connection(),
            Focus::Connect => match self.form.profile("Current connection".into()) {
                Ok(profile) => Command::Connect(profile.arguments()),
                Err(error) => {
                    self.status = format!("Check connection details: {error}");
                    Command::None
                }
            },
            Focus::Reconnect => self.reconnect(),
            Focus::Options => {
                self.advanced = !self.advanced;
                Command::None
            }
            Focus::Save => self.save(),
            Focus::SaveAs => {
                self.modal = Some(Modal::Name {
                    value: String::new(),
                    replace: None,
                });
                Command::None
            }
            Focus::Delete => {
                if self.profiles.get(self.selected).is_some() {
                    self.modal = Some(Modal::Delete);
                } else {
                    self.status = "No saved connection selected.".into();
                }
                Command::None
            }
            Focus::Dynamic => {
                self.form.dynamic = !self.form.dynamic;
                Command::None
            }
            Focus::Protocol => {
                self.form.vnc = !self.form.vnc;
                if self.form.port == "3389" && self.form.vnc {
                    self.form.port = "5900".into();
                } else if self.form.port == "5900" && !self.form.vnc {
                    self.form.port = "3389".into();
                }
                self.status = if self.form.vnc { "VNC: VeNCrypt or classic authentication; local scaling. RDP graphics/clipboard options do not apply." } else { "RDP connection selected." }.into();
                Command::None
            }
            Focus::Graphics => {
                self.form.h264 = !self.form.h264;
                Command::None
            }
            Focus::Clipboard => {
                self.form.clipboard = !self.form.clipboard;
                Command::None
            }
            Focus::Trust => {
                self.form.trust = self.form.trust.next();
                self.form.trust_value.clear();
                self.form.fingerprint = None;
                Command::None
            }
            _ => Command::None,
        }
    }

    fn new_connection(&mut self) -> Command {
        self.form = Form::default();
        self.editing = None;
        self.advanced = false;
        self.focus = Focus::Computer;
        self.status = "New connection. Enter a computer name or address.".into();
        Command::None
    }

    fn reconnect(&mut self) -> Command {
        match &self.retry {
            Some(arguments) => Command::Connect(arguments.clone()),
            None => {
                self.status = "No previous connection to reconnect. Choose Connect first.".into();
                Command::None
            }
        }
    }

    fn save(&mut self) -> Command {
        if let Some(index) = self.editing {
            let Some(name) = self.profiles.get(index).map(|profile| profile.name.clone()) else {
                self.editing = None;
                self.status = "The connection no longer exists. Save it with a new name.".into();
                return Command::None;
            };
            match self.form.profile(name.clone()) {
                Ok(profile) => {
                    self.profiles[index] = profile;
                    self.selected = index;
                    self.status = format!("Updated {name}.");
                    Command::Persist
                }
                Err(error) => {
                    self.status = format!("Could not update: {error}");
                    Command::None
                }
            }
        } else {
            self.modal = Some(Modal::Name {
                value: String::new(),
                replace: None,
            });
            Command::None
        }
    }

    fn modal_key(&mut self, mut modal: Modal, key: KeyEvent) -> Command {
        match (&mut modal, key.code) {
            (_, KeyCode::Esc) => Command::None,
            (Modal::Delete, KeyCode::Char('y') | KeyCode::Char('Y')) => {
                let name = self.profiles[self.selected].name.clone();
                self.profiles.remove(self.selected);
                self.selected = self.selected.min(self.profiles.len().saturating_sub(1));
                self.editing = None;
                self.status = format!("Deleted {name}.");
                Command::Persist
            }
            (Modal::Delete, _) => {
                self.modal = Some(modal);
                Command::None
            }
            (Modal::Name { value, replace }, KeyCode::Enter) => {
                let name = value.trim().to_owned();
                match self.form.profile(name) {
                    Ok(profile) => {
                        if self.profiles.iter().enumerate().any(|(index, saved)| {
                            Some(index) != *replace && saved.name == profile.name
                        }) {
                            self.status = "A saved connection already uses that name.".into();
                            self.modal = Some(modal);
                            return Command::None;
                        }
                        if let Some(index) = *replace {
                            self.profiles[index] = profile;
                            self.selected = index;
                        } else {
                            self.profiles.push(profile);
                            self.selected = self.profiles.len() - 1;
                        }
                        self.editing = Some(self.selected);
                        self.status = format!("Saved {}.", self.profiles[self.selected].name);
                        Command::Persist
                    }
                    Err(error) => {
                        self.status = format!("Could not save: {error}");
                        self.modal = Some(modal);
                        Command::None
                    }
                }
            }
            (Modal::Name { value, .. }, KeyCode::Backspace) => {
                value.pop();
                self.modal = Some(modal);
                Command::None
            }
            (Modal::Name { value, .. }, KeyCode::Char('u'))
                if key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                value.clear();
                self.modal = Some(modal);
                Command::None
            }
            (Modal::Name { value, .. }, KeyCode::Char(character))
                if value.len() < 1024 && !character.is_control() =>
            {
                value.push(character);
                self.modal = Some(modal);
                Command::None
            }
            (Modal::Name { .. }, _) => {
                self.modal = Some(modal);
                Command::None
            }
        }
    }

    fn active_text(&mut self) -> Option<&mut String> {
        match self.focus {
            Focus::Computer => Some(&mut self.form.computer),
            Focus::User => Some(&mut self.form.user),
            Focus::Port => Some(&mut self.form.port),
            Focus::Size => Some(&mut self.form.size),
            Focus::TrustValue if self.form.trust == Trust::Ca => Some(&mut self.form.trust_value),
            _ => None,
        }
    }

    fn draw(&self, out: &mut Stdout) -> io::Result<()> {
        let (width, height) = terminal::size()?;
        queue!(out, MoveTo(0, 0), Clear(ClearType::All))?;
        if width < MIN_WIDTH || height < MIN_HEIGHT {
            queue!(
                out,
                Print("Fjern needs a terminal at least 80×24. Resize or press Esc.")
            )?;
            return out.flush();
        }
        line(out, 2, 1, "Fjern  Remote Desktop Connection", true)?;
        line(
            out,
            2,
            2,
            "Choose a saved computer or enter a new connection.",
            false,
        )?;
        line(out, 2, 4, "Saved connections", true)?;
        if self.profiles.is_empty() {
            line(out, 3, 6, "No saved connections", false)?;
        } else {
            let (start, end) = self.visible_profiles(height);
            for (row, index) in (start..end).enumerate() {
                let profile = &self.profiles[index];
                let label = if self.editing == Some(index) {
                    format!("> {}", profile.name)
                } else {
                    format!("  {}", profile.name)
                };
                field(
                    out,
                    3,
                    6 + row as u16,
                    23,
                    &label,
                    self.focus == Focus::Saved && self.selected == index,
                )?;
            }
        }
        let connection_heading = self
            .editing
            .and_then(|index| self.profiles.get(index))
            .map_or_else(
                || "New connection".into(),
                |profile| format!("Edit {}", profile.name),
            );
        line(out, 29, 4, &fit(&connection_heading, 27), true)?;
        button(
            out,
            58,
            4,
            if self.form.vnc { "VNC" } else { "RDP" },
            self.focus == Focus::Protocol,
        )?;
        labeled(
            out,
            29,
            6,
            "Computer",
            &self.form.computer,
            self.focus == Focus::Computer,
        )?;
        labeled(
            out,
            29,
            8,
            if self.form.vnc {
                "User (optional)"
            } else {
                "User"
            },
            &self.form.user,
            self.focus == Focus::User,
        )?;
        button(out, 29, 10, "Connect", self.focus == Focus::Connect)?;
        button(out, 57, 10, "Reconnect", self.focus == Focus::Reconnect)?;
        button(
            out,
            41,
            10,
            if self.advanced {
                "Hide options"
            } else {
                "Options"
            },
            self.focus == Focus::Options,
        )?;
        button(
            out,
            29,
            12,
            if self.editing.is_some() {
                "Update"
            } else {
                "Save"
            },
            self.focus == Focus::Save,
        )?;
        button(out, 41, 12, "Save as", self.focus == Focus::SaveAs)?;
        button(out, 55, 12, "New", self.focus == Focus::New)?;
        button(out, 65, 12, "Delete", self.focus == Focus::Delete)?;
        if self.advanced {
            labeled(
                out,
                29,
                14,
                "Port",
                &self.form.port,
                self.focus == Focus::Port,
            )?;
            if !self.form.vnc {
                labeled(
                    out,
                    29,
                    16,
                    "Initial size",
                    &self.form.size,
                    self.focus == Focus::Size,
                )?;
                choice(
                    out,
                    29,
                    18,
                    "Dynamic resolution",
                    self.form.dynamic,
                    self.focus == Focus::Dynamic,
                )?;
                choice(
                    out,
                    58,
                    18,
                    "Clipboard",
                    self.form.clipboard,
                    self.focus == Focus::Clipboard,
                )?;
                labeled(
                    out,
                    29,
                    20,
                    "Trust",
                    self.form.trust.label(),
                    self.focus == Focus::Trust,
                )?;
                choice(
                    out,
                    58,
                    20,
                    "H.264",
                    self.form.h264,
                    self.focus == Focus::Graphics,
                )?;
                if self.form.trust == Trust::Ca {
                    labeled(
                        out,
                        29,
                        21,
                        "CA file",
                        &self.form.trust_value,
                        self.focus == Focus::TrustValue,
                    )?;
                }
            } else {
                line(
                    out,
                    29,
                    16,
                    "Server resolution; local window scaling",
                    false,
                )?;
                line(out, 29, 18, "VNC password is prompted when required", false)?;
            }
        }
        let status_y = height.saturating_sub(2);
        line(
            out,
            2,
            status_y,
            &fit(&self.status, width.saturating_sub(4) as usize),
            false,
        )?;
        if let Some(modal) = &self.modal {
            let prompt = match modal {
                Modal::Name { value, .. } => {
                    format!("Connection name: {value}_   Enter saves · Esc cancels")
                }
                Modal::Delete => format!(
                    "Delete {}? Press y to confirm · Esc cancels",
                    self.profiles[self.selected].name
                ),
            };
            queue!(
                out,
                MoveTo(8, height / 2),
                SetAttribute(Attribute::Reverse),
                Print(fit(
                    &format!(" {prompt} "),
                    width.saturating_sub(16) as usize
                )),
                SetAttribute(Attribute::Reset)
            )?;
        }
        out.flush()
    }

    fn visible_profiles(&self, height: u16) -> (usize, usize) {
        let count = usize::from(height.saturating_sub(9)).max(1);
        let start = if self.selected >= count {
            self.selected + 1 - count
        } else {
            0
        };
        (start, (start + count).min(self.profiles.len()))
    }
}

fn line(out: &mut Stdout, x: u16, y: u16, text: &str, bold: bool) -> io::Result<()> {
    queue!(out, MoveTo(x, y))?;
    if bold {
        queue!(out, SetAttribute(Attribute::Bold))?;
    }
    queue!(out, Print(text), SetAttribute(Attribute::Reset))
}
fn field(
    out: &mut Stdout,
    x: u16,
    y: u16,
    width: usize,
    value: &str,
    focused: bool,
) -> io::Result<()> {
    queue!(out, MoveTo(x, y))?;
    if focused {
        queue!(out, SetAttribute(Attribute::Reverse))?;
    }
    let value = fit(value, width);
    let padding = width.saturating_sub(UnicodeWidthStr::width(value.as_str()));
    queue!(
        out,
        Print(format!(" {value}{} ", " ".repeat(padding))),
        SetAttribute(Attribute::Reset)
    )
}
fn labeled(
    out: &mut Stdout,
    x: u16,
    y: u16,
    label: &str,
    value: &str,
    focused: bool,
) -> io::Result<()> {
    line(out, x, y, label, false)?;
    field(out, x + 18, y, 28, value, focused)
}
fn detail(
    out: &mut Stdout,
    x: u16,
    y: u16,
    width: u16,
    label: &str,
    value: &str,
) -> io::Result<()> {
    let value_width = usize::from(width.saturating_sub(x + 16));
    line(
        out,
        x,
        y,
        &format!("{label:<13}{}", fit(value, value_width)),
        false,
    )
}
fn button(out: &mut Stdout, x: u16, y: u16, label: &str, focused: bool) -> io::Result<()> {
    let value = format!("[ {label} ]");
    field(
        out,
        x,
        y,
        UnicodeWidthStr::width(value.as_str()),
        &value,
        focused,
    )
}
fn choice(
    out: &mut Stdout,
    x: u16,
    y: u16,
    label: &str,
    enabled: bool,
    focused: bool,
) -> io::Result<()> {
    field(
        out,
        x,
        y,
        label.len() + 7,
        &format!("{label}: {}", if enabled { "On" } else { "Off" }),
        focused,
    )
}
fn fit(value: &str, width: usize) -> String {
    if UnicodeWidthStr::width(value) <= width {
        return value.into();
    }
    if width == 0 {
        return String::new();
    }
    let target = width - 1;
    let mut used = 0;
    let mut text = String::new();
    for character in value.chars() {
        let cells = UnicodeWidthChar::width(character).unwrap_or(0);
        if used + cells > target {
            break;
        }
        used += cells;
        text.push(character);
    }
    text.push('…');
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }
    fn complete(app: &mut App) {
        app.form.computer = "host.example".into();
        app.form.user = "tester".into();
    }

    #[test]
    fn protocol_toggle_switches_default_ports_and_keeps_custom_ports() {
        let mut app = App::new(Vec::new(), None, None);
        app.focus = Focus::Protocol;
        app.key(key(KeyCode::Enter));
        assert!(app.form.vnc);
        assert_eq!(app.form.port, "5900");
        app.key(key(KeyCode::Enter));
        assert!(!app.form.vnc);
        assert_eq!(app.form.port, "3389");
        app.form.port = "5999".into();
        app.key(key(KeyCode::Enter));
        assert_eq!(app.form.port, "5999");
    }
    #[test]
    fn vnc_profiles_round_trip_with_optional_user() {
        let mut app = App::new(Vec::new(), None, None);
        app.form.computer = "vnc.example".into();
        app.form.vnc = true;
        app.form.port = "5901".into();
        app.form.user = "tester".into();
        let profile = app.form.profile("VNC desktop".into()).unwrap();
        assert_eq!(
            profile.arguments(),
            vec!["vnc", "vnc.example", "5901", "--user", "tester"]
        );
        let json = serde_json::to_string(&profile).unwrap();
        let saved: Profile = serde_json::from_str(&json).unwrap();
        assert!(Form::from_profile(&saved).vnc);
        let args = saved.arguments();
        let resumed = App::new(Vec::new(), None, Some(&args));
        assert!(resumed.form.vnc);
        assert_eq!(resumed.form.port, "5901");
        assert_eq!(resumed.form.user, "tester");
        assert!(resumed.focuses().contains(&Focus::User));
    }
    #[test]
    fn graphics_choice_survives_profile_and_connection_resume() {
        let mut app = App::new(Vec::new(), None, None);
        complete(&mut app);
        assert!(!app.form.h264);
        app.focus = Focus::Graphics;
        app.key(key(KeyCode::Enter));
        let saved = app.form.profile("Graphics test".into()).unwrap();
        let json = serde_json::to_string(&saved).unwrap();
        let saved: Profile = serde_json::from_str(&json).unwrap();
        assert!(saved.h264);
        let args = saved.arguments();
        assert!(crate::Options::parse(&args).unwrap().h264);
        let resumed = App::new(Vec::new(), None, Some(&args));
        assert!(resumed.form.h264);
        assert!(Form::from_profile(&saved).h264);
        app.key(key(KeyCode::Enter));
        let bitmap = app.form.profile("Bitmap test".into()).unwrap();
        assert!(!serde_json::to_string(&bitmap).unwrap().contains("h264"));
        assert!(!crate::Options::parse(&bitmap.arguments()).unwrap().h264);
    }
    #[test]
    fn keyboard_flow_builds_valid_cli_arguments() {
        let mut app = App::new(Vec::new(), None, None);
        complete(&mut app);
        app.focus = Focus::Connect;
        let Command::Connect(args) = app.key(key(KeyCode::Enter)) else {
            panic!()
        };
        let options = crate::Options::parse(&args).unwrap();
        assert_eq!(options.host, "host.example");
        assert!(options.dynamic_resolution && options.clipboard);
    }
    #[test]
    fn save_and_confirmed_delete_are_distinct_actions() {
        let mut app = App::new(Vec::new(), None, None);
        complete(&mut app);
        app.focus = Focus::Save;
        app.key(key(KeyCode::Enter));
        for character in "Work".chars() {
            app.key(key(KeyCode::Char(character)));
        }
        assert_eq!(app.key(key(KeyCode::Enter)), Command::Persist);
        assert_eq!(app.profiles.len(), 1);
        app.focus = Focus::Delete;
        app.key(key(KeyCode::Enter));
        app.key(key(KeyCode::Char('n')));
        assert_eq!(app.profiles.len(), 1);
        assert_eq!(app.key(key(KeyCode::Char('y'))), Command::Persist);
        assert!(app.profiles.is_empty());
    }
    #[test]
    fn options_and_saved_profile_editing_are_keyboard_accessible() {
        let mut app = App::new(Vec::new(), None, None);
        complete(&mut app);
        app.focus = Focus::Options;
        app.key(key(KeyCode::Enter));
        assert!(app.advanced);
        app.focus = Focus::Dynamic;
        app.key(key(KeyCode::Enter));
        assert!(!app.form.dynamic);
        app.focus = Focus::Trust;
        app.key(key(KeyCode::Enter));
        assert_eq!(app.form.trust, Trust::Ca);
    }

    #[test]
    fn editing_updates_existing_profile_and_space_edits_text() {
        let mut app = App::new(Vec::new(), None, None);
        complete(&mut app);
        app.focus = Focus::Save;
        app.key(key(KeyCode::Enter));
        for character in "Work".chars() {
            app.key(key(KeyCode::Char(character)));
        }
        assert_eq!(app.key(key(KeyCode::Enter)), Command::Persist);

        app.focus = Focus::Saved;
        app.key(key(KeyCode::Enter));
        assert_eq!(app.editing, Some(0));
        app.focus = Focus::User;
        app.key(key(KeyCode::Char(' ')));
        app.key(key(KeyCode::Char('x')));
        assert_eq!(app.form.user, "tester x");
        app.focus = Focus::Save;
        assert_eq!(app.key(key(KeyCode::Enter)), Command::Persist);
        assert_eq!(app.profiles.len(), 1);
        assert_eq!(app.profiles[0].user, "tester x");
    }

    #[test]
    fn save_as_does_not_replace_profile_being_edited() {
        let mut app = App::new(Vec::new(), None, None);
        complete(&mut app);
        let original = app.form.profile("Work".into()).unwrap();
        app.profiles.push(original);
        app.editing = Some(0);
        app.focus = Focus::SaveAs;
        app.key(key(KeyCode::Enter));
        for character in "Copy".chars() {
            app.key(key(KeyCode::Char(character)));
        }
        assert_eq!(app.key(key(KeyCode::Enter)), Command::Persist);
        assert_eq!(app.profiles.len(), 2);
        assert_eq!(app.profiles[1].name, "Copy");
        assert_eq!(app.editing, Some(1));
    }

    #[test]
    fn navigation_starts_at_saved_profiles_and_follows_screen_order() {
        let form = Form {
            computer: "host.example".into(),
            user: "tester".into(),
            ..Form::default()
        };
        let profile = form.profile("Work".into()).unwrap();
        let mut app = App::new(vec![profile], None, None);
        assert_eq!(app.focus, Focus::Saved);
        app.key(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Protocol);
        app.key(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Computer);
        app.key(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::User);
        app.key(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Connect);
    }

    #[test]
    fn arrow_keys_move_focus_from_text_fields() {
        let mut app = App::new(Vec::new(), None, None);
        assert_eq!(app.focus, Focus::Computer);

        app.key(key(KeyCode::Down));
        assert_eq!(app.focus, Focus::User);
        app.key(key(KeyCode::Right));
        assert_eq!(app.focus, Focus::Connect);
        app.key(key(KeyCode::Up));
        assert_eq!(app.focus, Focus::User);
        app.key(key(KeyCode::Left));
        assert_eq!(app.focus, Focus::Computer);
    }

    #[test]
    fn save_and_new_shortcuts_update_the_active_profile() {
        let mut app = App::new(Vec::new(), None, None);
        complete(&mut app);
        app.focus = Focus::Save;
        app.key(key(KeyCode::Enter));
        for character in "Work".chars() {
            app.key(key(KeyCode::Char(character)));
        }
        assert_eq!(app.key(key(KeyCode::Enter)), Command::Persist);
        app.form.user = "updated".into();
        let save = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL);
        assert_eq!(app.key(save), Command::Persist);
        assert_eq!(app.profiles[0].user, "updated");

        let new = KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL);
        assert_eq!(app.key(new), Command::None);
        assert_eq!(app.focus, Focus::Computer);
        assert!(app.form.computer.is_empty());
        assert_eq!(app.editing, None);
    }

    #[test]
    fn selected_saved_profile_remains_in_scrolled_view() {
        let mut app = App::new(Vec::new(), None, None);
        complete(&mut app);
        for index in 0..30 {
            app.profiles
                .push(app.form.profile(format!("Computer {index}")).unwrap());
        }
        app.selected = 29;
        let (start, end) = app.visible_profiles(MIN_HEIGHT);
        assert!(start <= app.selected && app.selected < end);
        assert_eq!(end - start, usize::from(MIN_HEIGHT - 9));
    }

    #[test]
    fn minimum_layout_and_wide_text_stay_within_terminal_cells() {
        // The rightmost control ends before column 80; advanced content ends on
        // row 21 and leaves row 22 for status in a 24-row terminal.
        assert!(65 + "[ Delete ]".width() + 2 <= usize::from(MIN_WIDTH));
        assert!(21 < usize::from(MIN_HEIGHT - 2));
        let clipped = fit("電腦名前", 5);
        assert!(clipped.width() <= 5);
        assert!(clipped.ends_with('…'));
    }

    #[test]
    fn deleting_while_editing_cannot_update_a_stale_index() {
        let mut app = App::new(Vec::new(), None, None);
        complete(&mut app);
        app.profiles.push(app.form.profile("First".into()).unwrap());
        app.profiles
            .push(app.form.profile("Second".into()).unwrap());
        app.selected = 1;
        app.focus = Focus::Saved;
        app.key(key(KeyCode::Enter));
        assert_eq!(app.editing, Some(1));
        app.focus = Focus::Delete;
        app.key(key(KeyCode::Enter));
        assert_eq!(app.key(key(KeyCode::Char('y'))), Command::Persist);
        assert_eq!(app.editing, None);
        app.focus = Focus::Save;
        assert!(matches!(app.key(key(KeyCode::Enter)), Command::None));
        assert!(matches!(app.modal, Some(Modal::Name { replace: None, .. })));
    }

    #[test]
    fn control_u_clears_fields_and_profile_name_prompt() {
        let clear = KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL);
        let mut app = App::new(Vec::new(), None, None);
        app.form.computer = "old host".into();
        assert_eq!(app.key(clear), Command::None);
        assert!(app.form.computer.is_empty());
        complete(&mut app);
        app.focus = Focus::Save;
        app.key(key(KeyCode::Enter));
        app.key(key(KeyCode::Char('x')));
        app.key(clear);
        assert!(matches!(
            app.modal,
            Some(Modal::Name { ref value, .. }) if value.is_empty()
        ));
    }

    #[test]
    fn certificate_dialog_defaults_to_cancel_and_requires_a_choice() {
        let mut dialog = CertificateDialog::default();
        assert_eq!(
            dialog.key(key(KeyCode::Enter)),
            Some(CertificateChoice::Cancel)
        );
        assert_eq!(
            dialog.key(key(KeyCode::Esc)),
            Some(CertificateChoice::Cancel)
        );
        assert_eq!(dialog.key(key(KeyCode::Tab)), None);
        assert_eq!(
            dialog.key(key(KeyCode::Enter)),
            Some(CertificateChoice::Once)
        );
        assert_eq!(dialog.key(key(KeyCode::Right)), None);
        assert_eq!(
            dialog.key(key(KeyCode::Enter)),
            Some(CertificateChoice::Trust)
        );
    }

    #[test]
    fn resumed_connection_keeps_form_values_without_manual_pin_ui() {
        let args = [
            "connect",
            "host.example",
            "3390",
            "--user",
            "LAB\\tester",
            "--size",
            "1280x800",
            "--dynamic-resolution",
            "off",
            "--clipboard",
            "off",
            "--ca",
            "/tmp/Lab CA.pem",
        ]
        .map(str::to_owned);
        let app = App::new(
            Vec::new(),
            Some("Certificate cancelled.".into()),
            Some(&args),
        );
        assert_eq!(app.form.computer, "host.example");
        assert_eq!(app.form.user, "LAB\\tester");
        assert_eq!(app.form.port, "3390");
        assert_eq!(app.form.size, "1280x800");
        assert!(!app.form.dynamic);
        assert!(!app.form.clipboard);
        assert_eq!(app.form.trust, Trust::Ca);
        assert_eq!(app.form.trust_value, "/tmp/Lab CA.pem");
    }

    #[test]
    fn reconnect_reuses_last_attempt_without_overwriting_edited_form() {
        let args = ["connect", "old.example", "--user", "tester"].map(str::to_owned);
        let mut app = App::new(Vec::new(), Some("Connection ended".into()), Some(&args));
        app.form.computer = "unsaved.example".into();
        assert_eq!(
            app.key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL)),
            Command::Connect(args.to_vec())
        );
        app.focus = Focus::Reconnect;
        assert_eq!(
            app.key(key(KeyCode::Enter)),
            Command::Connect(args.to_vec())
        );
        assert_eq!(app.form.computer, "unsaved.example");
        let mut fresh = App::new(Vec::new(), None, None);
        fresh.focus = Focus::Reconnect;
        assert_eq!(fresh.key(key(KeyCode::Enter)), Command::None);
        assert!(fresh.status.contains("No previous connection"));
    }
}
