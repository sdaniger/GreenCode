use gpui_kit::{rgb, Hsla};

use gpui_kit::component::input::InputState;
use gpui_kit::{Entity, Keystroke, ScrollHandle, SharedString, Subscription};
#[cfg(unix)]
use std::{os::fd::{AsRawFd, OwnedFd}, time::Duration};
#[cfg(windows)]
use std::io;
use std::{
    collections::HashMap,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    sync::Arc,
};

#[cfg(unix)]
pub(super) type Master = Arc<OwnedFd>;
#[cfg(windows)]
pub(super) type Master = Box<dyn portable_pty::MasterPty + Send>;
#[cfg(unix)]
pub(super) type Writer = tokio::fs::File;
#[cfg(windows)]
pub(super) type Writer = tokio::io::WriteHalf<tokio::io::SimplexStream>;
#[cfg(unix)]
pub(super) type Process = Arc<tokio::sync::Mutex<tokio::process::Child>>;

#[cfg(windows)]
pub(super) struct Process(Box<dyn portable_pty::Child + Send + Sync>);

#[cfg(windows)]
impl Drop for Process {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            let _ = self.0.kill();
        }
    }
}

#[cfg(windows)]
pub(super) fn windows_pty(
    rt: &tokio::runtime::Runtime,
    cwd: &Path,
    (rows, cols): (u16, u16),
) -> io::Result<(Process, Master, Box<dyn io::Read + Send>, Writer)> {
    let size = portable_pty::PtySize { rows, cols, pixel_width: 0, pixel_height: 0 };
    let pty = portable_pty::native_pty_system().openpty(size).map_err(io::Error::other)?;
    let stdout = pty.master.try_clone_reader().map_err(io::Error::other)?;
    let mut stdin = pty.master.take_writer().map_err(io::Error::other)?;
    let mut cmd = portable_pty::CommandBuilder::new(std::env::var_os("COMSPEC").unwrap_or_else(|| "cmd.exe".into()));
    cmd.cwd(cwd);
    cmd.arg("/D");
    let child = pty.slave.spawn_command(cmd).map_err(io::Error::other)?;
    let (mut from_ui, to_pty) = tokio::io::simplex(8192);
    let handle = rt.handle().clone();
    rt.spawn_blocking(move || {
        use tokio::io::AsyncReadExt;
        let mut buf = [0; 8192];
        while let Ok(n @ 1..) = handle.block_on(from_ui.read(&mut buf)) {
            if stdin.write_all(&buf[..n]).is_err() {
                break;
            }
        }
    });
    Ok((Process(child), pty.master, stdout, to_pty))
}

pub(super) struct Term {
    pub number: usize,
    pub input: Entity<InputState>,
    pub _subscription: Subscription,
    pub process: Option<Process>,
    pub stdin: Option<Arc<tokio::sync::Mutex<Writer>>>,
    pub master: Option<Master>,
    pub lines: Vec<SharedString>,
    pub previous: Vec<SharedString>,
    pub open: bool,
    pub screen: vt100::Parser<Replies>,
    pub selection: Option<(usize, usize)>,
    pub scroll: ScrollHandle,
    pending_escape: String,
    /// The Pi/OMP terminal UI of this thread id. Its output may contain secrets: it is only ever a live screen, never
    /// kept as lines, never saved, never offered to the composer.
    pub private: Option<String>,
    /// Ends the private terminal's child (and its process group); dropping it does too.
    pub kill: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Term {
    pub fn new(
        number: usize,
        input: Entity<InputState>,
        subscription: Subscription,
        process: Option<Process>,
        stdin: Option<Arc<tokio::sync::Mutex<Writer>>>,
        master: Option<Master>,
        cwd: &Path,
    ) -> Self {
        let previous = std::fs::read_to_string(log_path(cwd, number))
            .unwrap_or_default()
            .lines()
            .map(|s| s.to_string().into())
            .collect();
        let scroll = ScrollHandle::default();
        scroll.scroll_to_bottom();
        let mut screen = super::new_screen();
        // ConPTY can request the cursor position during startup.
        screen.callbacks_mut().enabled = cfg!(windows);
        Self {
            number,
            input,
            _subscription: subscription,
            process,
            stdin,
            master,
            lines: vec![],
            previous,
            open: false,
            screen,
            selection: None,
            pending_escape: String::new(),
            scroll,
            private: None,
            kill: None,
        }
    }

    /// Turns this into the private terminal of thread `thread`; nothing it shows is persisted or reused.
    pub fn into_private(
        mut self,
        thread: String,
        kill: tokio::sync::oneshot::Sender<()>,
        fg: [u8; 3],
        bg: [u8; 3],
    ) -> Self {
        self.previous.clear();
        self.private = Some(thread);
        self.kill = Some(kill);
        let replies = self.screen.callbacks_mut();
        (replies.enabled, replies.fg, replies.bg) = (true, fg, bg);
        self
    }

    /// Whether the program owns the whole screen and every key. Pi and OMP draw on the main screen with cursor
    /// movement, so a private terminal is always raw, not only in the alternate screen.
    pub fn raw(&self) -> bool {
        self.private.is_some() || self.screen.screen().alternate_screen()
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        let (Some(rows), Some(cols)) = (
            std::num::NonZeroU16::new(rows),
            std::num::NonZeroU16::new(cols),
        ) else {
            return;
        };
        if self.screen.screen().size() == (rows, cols) {
            return;
        }
        self.screen.set_size(rows, cols);
        if let Some(master) = &self.master {
            #[cfg(unix)]
            {
                let size = libc::winsize {
                    ws_row: rows.get(),
                    ws_col: cols.get(),
                    ws_xpixel: 0,
                    ws_ypixel: 0,
                };
                unsafe {
                    libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ as _, &size);
                }
            }
            #[cfg(windows)]
            let _ = master.resize(portable_pty::PtySize {
                rows: rows.get(),
                cols: cols.get(),
                pixel_width: 0,
                pixel_height: 0,
            });
        }
    }

    /// Feeds program output and returns any terminal query replies.
    pub fn feed(&mut self, text: &str) -> Vec<u8> {
        let was_full = self.screen.screen().alternate_screen();
        self.screen.process(text.as_bytes());
        let reply = std::mem::take(&mut self.screen.callbacks_mut().out);
        if self.private.is_some() || self.screen.screen().alternate_screen() {
            return reply;
        }
        let text = if was_full {
            text.rsplit_once("\u{1b}[?1049l")
                .map_or("", |(_, rest)| rest)
        } else {
            text
        };
        let text = format!("{}{text}", std::mem::take(&mut self.pending_escape));
        let mut chars = text.char_indices().peekable();
        let mut incomplete = None;
        while let Some((start, c)) = chars.next() {
            if c != '\x1b' {
                continue;
            }
            match chars.next() {
                None => {
                    incomplete = Some(start);
                    break;
                }
                Some((_, '[')) => {
                    if !chars.by_ref().any(|(_, c)| ('@'..='~').contains(&c)) {
                        incomplete = Some(start);
                        break;
                    }
                }
                Some((_, ']')) => {
                    let mut done = false;
                    while let Some((_, c)) = chars.next() {
                        if c == '\x07' {
                            done = true;
                            break;
                        }
                        if c == '\x1b' {
                            done = chars.next().is_some();
                            break;
                        }
                    }
                    if !done {
                        incomplete = Some(start);
                        break;
                    }
                }
                Some((_, '(' | ')')) if chars.next().is_none() => {
                    incomplete = Some(start);
                    break;
                }
                _ => {}
            }
        }
        let end = incomplete.unwrap_or(text.len());
        self.pending_escape.push_str(&text[end..]);
        super::term_feed(&mut self.lines, &mut self.open, &text[..end]);
        self.scroll.scroll_to_bottom();
        reply
    }

    pub fn save(&self, cwd: &Path) -> std::io::Result<()> {
        if self.private.is_some() {
            return Ok(());
        }
        let path = log_path(cwd, self.number);
        std::fs::create_dir_all(path.parent().expect("log directory"))?;
        let text = self
            .previous
            .iter()
            .chain(&self.lines)
            .rev()
            .take(500)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .map(|s| s.as_ref())
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(path, text)
    }
}

fn log_path(cwd: &Path, number: usize) -> PathBuf {
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    cwd.hash(&mut hash);
    greencode_core::home()
        .join("terminals")
        .join(format!("{:016x}-{number}.log", hash.finish()))
}

#[derive(Default)]
pub(super) struct TerminalSet {
    pub projects: HashMap<PathBuf, (Vec<Term>, usize)>,
    next: HashMap<PathBuf, usize>,
}
impl TerminalSet {
    pub fn active(&self, cwd: &Path) -> Option<&Term> {
        let (terms, active) = self.projects.get(cwd)?;
        terms.get(*active)
    }
    pub fn active_mut(&mut self, cwd: &Path) -> Option<&mut Term> {
        let (terms, active) = self.projects.get_mut(cwd)?;
        terms.get_mut(*active)
    }
    pub fn by_number_mut(&mut self, cwd: &Path, number: usize) -> Option<&mut Term> {
        self.projects
            .get_mut(cwd)?
            .0
            .iter_mut()
            .find(|t| t.number == number)
    }
    pub fn next_number(&mut self, cwd: &Path) -> usize {
        let n = self.next.entry(cwd.to_path_buf()).or_insert(1);
        let result = *n;
        *n += 1;
        result
    }
    pub fn insert(&mut self, cwd: PathBuf, term: Term) {
        let (terms, active) = self.projects.entry(cwd).or_default();
        terms.push(term);
        *active = terms.len() - 1;
    }
    pub fn remove_active(&mut self, cwd: &Path) -> Option<Term> {
        let (terms, active) = self.projects.get_mut(cwd)?;
        if terms.is_empty() {
            return None;
        }
        let term = terms.remove(*active);
        *active = (*active).min(terms.len().saturating_sub(1));
        Some(term)
    }
    /// Removes terminal `number`, keeping the same terminal selected when it was another one.
    pub fn remove_number(&mut self, cwd: &Path, number: usize) -> Option<Term> {
        let (terms, active) = self.projects.get_mut(cwd)?;
        let at = terms.iter().position(|t| t.number == number)?;
        let term = terms.remove(at);
        if at < *active {
            *active -= 1;
        }
        *active = (*active).min(terms.len().saturating_sub(1));
        Some(term)
    }
}
impl Drop for TerminalSet {
    fn drop(&mut self) {
        for (cwd, (terms, _)) in &self.projects {
            for term in terms {
                if let Err(e) = term.save(cwd) {
                    eprintln!("ターミナルの出力を保存できません: {e}");
                }
            }
        }
    }
}

pub(super) fn plain(line: &str) -> String {
    parse_sgr_line(line)
        .into_iter()
        .map(|(text, _, _, _)| text)
        .collect()
}

/// Answers the queries a full-screen program sends its terminal at startup. vt100 parses them but never replies, and Pi
/// and OMP wait for the answers (cursor position, device attributes, colours) before settling. Only private terminals reply.
#[derive(Default)]
pub(super) struct Replies {
    pub enabled: bool,
    pub out: Vec<u8>,
    pub fg: [u8; 3],
    pub bg: [u8; 3],
}

impl vt100::Callbacks for Replies {
    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        i1: Option<u8>,
        i2: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) {
        if !self.enabled {
            return;
        }
        let first = params.first().and_then(|p| p.first()).copied().unwrap_or(0);
        let (row, col) = screen.cursor_position();
        let (rows, cols) = screen.size();
        let reply = match (i1, i2, c) {
            (None, None, 'n') if first == 5 => "\x1b[0n".to_string(),
            (None, None, 'n') if first == 6 => format!("\x1b[{};{}R", row + 1, col + 1),
            (Some(b'?'), None, 'n') if first == 6 => format!("\x1b[?{};{}R", row + 1, col + 1),
            (None, None, 'c') if first == 0 => "\x1b[?62;22c".to_string(),
            (Some(b'>'), None, 'c') => "\x1b[>1;10;0c".to_string(),
            // DECRQM: every mode is reported "not recognised" (0), so programs fall back to plain behaviour.
            (Some(b'?'), Some(b'$'), 'p') => format!("\x1b[?{first};0$y"),
            (None, None, 't') if first == 18 => format!("\x1b[8;{};{}t", rows.get(), cols.get()),
            _ => return,
        };
        self.out.extend_from_slice(reply.as_bytes());
    }

    fn unhandled_osc(&mut self, _: &mut vt100::Screen, params: &[&[u8]]) {
        if !self.enabled {
            return;
        }
        let (code, colour) = match params {
            [b"10", b"?"] => ("10", self.fg),
            [b"11", b"?"] => ("11", self.bg),
            _ => return,
        };
        let [r, g, b] = colour.map(|v| format!("{v:02x}{v:02x}"));
        self.out
            .extend_from_slice(format!("\x1b]{code};rgb:{r}/{g}/{b}\x1b\\").as_bytes());
    }
}

/// Bytes for a key press that is not text. Text keys return `None` so the focused input receives them through the
/// platform text system, which is what lets an IME compose Japanese; Cmd shortcuts stay with the app.
pub(super) fn tui_key(k: &Keystroke, application_cursor: bool) -> Option<Vec<u8>> {
    let m = &k.modifiers;
    if m.platform {
        return None;
    }
    // xterm's modifier parameter: 1 + shift + 2 * alt + 4 * control.
    let param = 1 + u8::from(m.shift) + 2 * u8::from(m.alt) + 4 * u8::from(m.control);
    let arrow = |end: char| {
        if param > 1 {
            format!("\x1b[1;{param}{end}")
        } else if application_cursor {
            format!("\x1bO{end}")
        } else {
            format!("\x1b[{end}")
        }
    };
    let tilde = |n: u8| {
        if param > 1 {
            format!("\x1b[{n};{param}~")
        } else {
            format!("\x1b[{n}~")
        }
    };
    let function = |end: char| {
        if param > 1 {
            format!("\x1b[1;{param}{end}")
        } else {
            format!("\x1bO{end}")
        }
    };
    let key = k.key.as_str();
    let sequence = match key {
        "up" => arrow('A'),
        "down" => arrow('B'),
        "right" => arrow('C'),
        "left" => arrow('D'),
        "home" => arrow('H'),
        "end" => arrow('F'),
        "pageup" => tilde(5),
        "pagedown" => tilde(6),
        "delete" => tilde(3),
        "insert" => tilde(2),
        "f1" => function('P'),
        "f2" => function('Q'),
        "f3" => function('R'),
        "f4" => function('S'),
        "f5" => tilde(15),
        "f6" => tilde(17),
        "f7" => tilde(18),
        "f8" => tilde(19),
        "f9" => tilde(20),
        "f10" => tilde(21),
        "f11" => tilde(23),
        "f12" => tilde(24),
        // Shift+Enter and Alt+Enter both insert a newline in Pi/OMP; a bare terminal cannot tell Shift+Enter from Enter.
        "enter" if m.control => "\n".into(),
        "enter" if m.alt || m.shift => "\x1b\r".into(),
        "enter" => "\r".into(),
        "escape" if m.alt => "\x1b\x1b".into(),
        "escape" => "\x1b".into(),
        "backspace" if m.alt => "\x1b\x7f".into(),
        "backspace" if m.control => "\x08".into(),
        "backspace" => "\x7f".into(),
        "tab" if m.shift => "\x1b[Z".into(),
        "tab" => "\t".into(),
        "space" if m.control => "\0".into(),
        _ if key.chars().count() == 1 && (m.control || m.alt) => {
            let c = key.chars().next().unwrap();
            let mut out = String::new();
            if m.alt {
                out.push('\x1b');
            }
            if m.control {
                let byte = match c {
                    'a'..='z' => c as u8 & 0x1f,
                    '@' => 0,
                    '[' => 0x1b,
                    '\\' => 0x1c,
                    ']' => 0x1d,
                    '^' => 0x1e,
                    '_' | '/' => 0x1f,
                    _ => return None,
                };
                out.push(byte as char);
            } else {
                out.push(if m.shift { c.to_ascii_uppercase() } else { c });
            }
            out
        }
        _ => return None,
    };
    Some(sequence.into_bytes())
}

/// One row of the live screen as styled runs; `fg` and `bg` are the theme's default colours, which inverse video (and the cursor) swap.
pub(super) fn screen_row(
    screen: &vt100::Screen,
    row: u16,
    cursor: Option<u16>,
    fg: Hsla,
    bg: Hsla,
) -> Vec<Run> {
    let convert = |c: vt100::Color| match c {
        vt100::Color::Default => None,
        vt100::Color::Idx(n) => Some(color(n)),
        vt100::Color::Rgb(r, g, b) => {
            Some(rgb((u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b)).into())
        }
    };
    let mut runs: Vec<Run> = Vec::new();
    for col in 0..screen.size().1.get() {
        let Some(cell) = screen.cell(row, col) else {
            continue;
        };
        if cell.is_wide_continuation() {
            continue;
        }
        let text = if cell.has_contents() {
            cell.contents()
        } else {
            " "
        };
        let (mut front, mut back) = (convert(cell.fgcolor()), convert(cell.bgcolor()));
        if cell.inverse() != (cursor == Some(col)) {
            (front, back) = (Some(back.unwrap_or(bg)), Some(front.unwrap_or(fg)));
        }
        let bold = cell.bold();
        match runs.last_mut() {
            Some((run, f, b, bd)) if *f == front && *b == back && *bd == bold => run.push_str(text),
            _ => runs.push((text.to_string(), front, back, bold)),
        }
    }
    runs
}

/// Owns a private terminal's child until its whole process group is gone: waits for the child, or ends it when `kill`
/// fires or is dropped (SIGTERM, then SIGKILL after 3 s). Only a group that is verifiably empty counts as exited, so no
/// descendant can still write the session when the caller reconciles it.
#[cfg(unix)]
pub(super) async fn supervise(
    mut child: tokio::process::Child,
    kill: tokio::sync::oneshot::Receiver<()>,
) -> Result<std::process::ExitStatus, String> {
    // The child is its session leader (setsid in the PTY setup), so its pid is the process group id.
    let group = child.id().map(|pid| pid as libc::pid_t);
    let signal = |sig: libc::c_int| {
        if let Some(g) = group {
            unsafe {
                libc::killpg(g, sig);
            }
        }
    };
    let gone = || {
        group.is_none_or(|g| unsafe { libc::killpg(g, 0) } == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH))
    };
    let waited = tokio::select! {
        status = child.wait() => status,
        _ = kill => {
            signal(libc::SIGTERM);
            match tokio::time::timeout(Duration::from_secs(3), child.wait()).await {
                Ok(status) => status,
                Err(_) => { signal(libc::SIGKILL); child.wait().await }
            }
        }
    };
    let status = waited.map_err(|e| format!("端末 UI のプロセスを待機できません: {e}"))?;
    if !gone() {
        signal(libc::SIGTERM);
        for round in 0..60 {
            if gone() {
                break;
            }
            if round == 20 {
                signal(libc::SIGKILL);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    if gone() {
        Ok(status)
    } else {
        Err("端末 UI の子プロセスが終了しません。完全に終了するまで、このスレッドの引き継ぎは再開できません".into())
    }
}

pub(super) type Run = (String, Option<Hsla>, Option<Hsla>, bool);

/// Splits styled runs into rows of at most `cols` cells (wide CJK characters count two), as the PTY would wrap them.
pub(super) fn wrap_runs(runs: Vec<Run>, cols: usize) -> Vec<Vec<Run>> {
    let cols = cols.max(1);
    let mut rows: Vec<Vec<Run>> = vec![Vec::new()];
    let mut used = 0;
    for (text, fg, bg, bold) in runs {
        let mut piece = String::new();
        for c in text.chars() {
            let width = if c as u32 >= 0x1100
                && (c as u32 <= 0x115F
                    || (0x2E80..=0xA4CF).contains(&(c as u32))
                    || (0xAC00..=0xD7A3).contains(&(c as u32))
                    || (0xFF00..=0xFF60).contains(&(c as u32)))
            {
                2
            } else {
                1
            };
            if used + width > cols {
                if !piece.is_empty() {
                    rows.last_mut()
                        .expect("row")
                        .push((std::mem::take(&mut piece), fg, bg, bold));
                }
                rows.push(Vec::new());
                used = 0;
            }
            piece.push(c);
            used += width;
        }
        if !piece.is_empty() {
            rows.last_mut().expect("row").push((piece, fg, bg, bold));
        }
    }
    rows
}

fn color(n: u8) -> Hsla {
    const ANSI: [u32; 16] = [
        0x000000, 0x800000, 0x008000, 0x808000, 0x000080, 0x800080, 0x008080, 0xc0c0c0, 0x808080,
        0xff0000, 0x00ff00, 0xffff00, 0x0000ff, 0xff00ff, 0x00ffff, 0xffffff,
    ];
    let value = if n < 16 {
        ANSI[n as usize]
    } else if n < 232 {
        let n = n - 16;
        let component = |v: u8| if v == 0 { 0 } else { 55 + v as u32 * 40 };
        (component(n / 36) << 16) | (component(n / 6 % 6) << 8) | component(n % 6)
    } else {
        let v = 8 + (n as u32 - 232) * 10;
        (v << 16) | (v << 8) | v
    };
    rgb(value).into()
}

pub(super) fn parse_sgr_line(line: &str) -> Vec<Run> {
    let mut runs = Vec::new();
    let (mut fg, mut bg, mut bold) = (None, None, false);
    let mut text = String::new();
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            text.push(c);
            continue;
        }
        match chars.next() {
            Some('[') => {
                let mut params = String::new();
                let mut end = None;
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        end = Some(c);
                        break;
                    }
                    params.push(c);
                }
                if end != Some('m') {
                    continue;
                }
                if !text.is_empty() {
                    runs.push((std::mem::take(&mut text), fg, bg, bold));
                }
                let values: Vec<u16> = params.split(';').map(|s| s.parse().unwrap_or(0)).collect();
                let mut i = 0;
                while i < values.len() {
                    match values[i] {
                        0 => {
                            fg = None;
                            bg = None;
                            bold = false;
                        }
                        1 => bold = true,
                        22 => bold = false,
                        30..=37 => fg = Some(color((values[i] - 30) as u8)),
                        90..=97 => fg = Some(color((values[i] - 90 + 8) as u8)),
                        40..=47 => bg = Some(color((values[i] - 40) as u8)),
                        100..=107 => bg = Some(color((values[i] - 100 + 8) as u8)),
                        39 => fg = None,
                        49 => bg = None,
                        38 | 48 => {
                            let foreground = values[i] == 38;
                            let parsed = if values.get(i + 1) == Some(&5)
                                && values.get(i + 2).is_some_and(|v| *v <= 255)
                            {
                                i += 2;
                                Some(color(values[i] as u8))
                            } else if values.get(i + 1) == Some(&2)
                                && values
                                    .get(i + 2..i + 5)
                                    .is_some_and(|v| v.iter().all(|x| *x <= 255))
                            {
                                let v = ((values[i + 2] as u32) << 16)
                                    | ((values[i + 3] as u32) << 8)
                                    | values[i + 4] as u32;
                                i += 4;
                                Some(rgb(v).into())
                            } else {
                                None
                            };
                            if let Some(c) = parsed {
                                if foreground {
                                    fg = Some(c);
                                } else {
                                    bg = Some(c);
                                }
                            }
                        }
                        _ => {}
                    }
                    i += 1;
                }
            }
            Some(']') => {
                while let Some(c) = chars.next() {
                    if c == '\x07' {
                        break;
                    }
                    if c == '\x1b' {
                        chars.next();
                        break;
                    }
                }
            }
            Some('(' | ')') => {
                chars.next();
            }
            _ => {}
        }
    }
    if !text.is_empty() {
        runs.push((text, fg, bg, bold));
    }
    runs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_sgr_colors_and_resets() {
        let runs = parse_sgr_line("plain\x1b[1;31;44mred\x1b[38;5;196mbright\x1b[38;2;1;2;3mtrue\x1b[22;39;49mnormal\x1b[0mend");
        assert_eq!(runs[0], ("plain".into(), None, None, false));
        assert_eq!(
            runs[1],
            ("red".into(), Some(color(1)), Some(color(4)), true)
        );
        assert_eq!(runs[2].1, Some(color(196)));
        assert_eq!(runs[3].1, Some(rgb(0x010203).into()));
        assert_eq!(runs[4], ("normal".into(), None, None, false));
        assert_eq!(runs[5], ("end".into(), None, None, false));
    }

    #[test]
    fn terminal_sgr_nested_bright_and_escape_stripping() {
        let runs = parse_sgr_line(
            "\x1b[91;104mone\x1b[1mtwo\x1b[39mthree\x1b[0mfour\x1b[2K\x1b]0;title\x07!",
        );
        assert_eq!(
            runs[0],
            ("one".into(), Some(color(9)), Some(color(12)), false)
        );
        assert_eq!(
            runs[1],
            ("two".into(), Some(color(9)), Some(color(12)), true)
        );
        assert_eq!(runs[2], ("three".into(), None, Some(color(12)), true));
        assert_eq!(runs[3], ("four!".into(), None, None, false));
    }

    #[test]
    fn long_lines_wrap_at_the_pty_width() {
        let rows = super::wrap_runs(
            super::parse_sgr_line(&format!("{}\x1b[1m{}\x1b[0m", "a".repeat(7), "b".repeat(6))),
            5,
        );
        let text: Vec<String> = rows
            .iter()
            .map(|r| r.iter().map(|(t, ..)| t.as_str()).collect())
            .collect();
        assert_eq!(text, ["aaaaa", "aabbb", "bbb"]);
        assert!(rows[1][1].3 && !rows[1][0].3);
        let wide = super::wrap_runs(vec![("日本語です".into(), None, None, false)], 6);
        assert_eq!(wide.len(), 2);
        assert_eq!(super::wrap_runs(vec![], 10).len(), 1);
    }
}
