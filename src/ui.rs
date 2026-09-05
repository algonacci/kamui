use crate::terminal::{Style as AnsiStyle, Ui};
use anyhow::{Context, Result};
use crossterm::{
    cursor::SetCursorStyle,
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, Padding, Paragraph},
};
use std::{
    collections::VecDeque,
    io::{self, Stdout, Write},
    sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError},
    time::Duration,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Bouncing wall for the input editor while the agent is thinking.
/// A 3-cell bright wall (███) glides on a muted track (─), pausing
/// briefly at each end for eased ping-pong. A one-cell fade tail (▓)
/// trails behind the direction of travel for a subtle motion-blur.
const WALL_TRACK_LEN: usize = 10;
const WALL_LEN: usize = 3;
const WALL_POS: [usize; 16] = [0, 0, 1, 2, 3, 4, 5, 6, 7, 7, 6, 5, 4, 3, 2, 1];

fn bouncing_wall_spans(frame_idx: usize) -> Vec<Span<'static>> {
    let pos = WALL_POS[frame_idx % WALL_POS.len()];
    // first half of the cycle moves right, second half moves left
    let moving_right = (frame_idx % WALL_POS.len()) < 8;
    let mut spans = Vec::with_capacity(WALL_TRACK_LEN + 2);
    spans.push(Span::styled("[", Style::default().fg(MUTED())));
    for i in 0..WALL_TRACK_LEN {
        let in_wall = i >= pos && i < pos + WALL_LEN;
        let is_tail = if moving_right {
            pos > 0 && i + 1 == pos
        } else {
            i == pos + WALL_LEN && i < WALL_TRACK_LEN
        };
        if in_wall {
            spans.push(Span::styled(
                "█",
                Style::default().fg(BLUE()).add_modifier(Modifier::BOLD),
            ));
        } else if is_tail {
            spans.push(Span::styled("▓", Style::default().fg(BLUE())));
        } else {
            spans.push(Span::styled("─", Style::default().fg(BORDER())));
        }
    }
    spans.push(Span::styled("]", Style::default().fg(MUTED())));
    spans
}

struct WrappedCache {
    width: u16,
    fp: u64,
    rows: Vec<(Line<'static>, Option<u64>)>,
}

static WRAPPED_CACHE: OnceLock<Mutex<Option<WrappedCache>>> = OnceLock::new();

fn wrapped_fingerprint(model: &Model) -> u64 {
    let mut fp: u64 = 146959;
    fp = fp.wrapping_mul(31).wrapping_add(model.cards.len() as u64);
    for card in &model.cards {
        fp = fp.wrapping_mul(31).wrapping_add(card.id);
        fp = fp.wrapping_mul(31).wrapping_add(card.body.len() as u64);
        fp = fp.wrapping_mul(31).wrapping_add(card.title.len() as u64);
        fp = fp.wrapping_mul(31).wrapping_add(card.collapsed as u64);
        fp = fp.wrapping_mul(31).wrapping_add(match card.kind {
            CardKind::User => 1,
            CardKind::Tool => 2,
            CardKind::Output => 3,
            CardKind::Error => 4,
            CardKind::Note => 5,
        });
        if let Some((status, ok)) = &card.status {
            fp = fp.wrapping_mul(31).wrapping_add(status.len() as u64);
            fp = fp.wrapping_add(*ok as u64 + 7);
        }
    }
    fp = fp
        .wrapping_mul(31)
        .wrapping_add(model.warnings.len() as u64);
    fp = fp
        .wrapping_mul(31)
        .wrapping_add(model.warnings_visible as u64);
    fp = fp
        .wrapping_mul(31)
        .wrapping_add(model.warning_details.len() as u64);
    fp = fp
        .wrapping_mul(31)
        .wrapping_add(model.warning_details_visible as u64);
    fp
}

/// Welcome logo, opencode-style: block-letter art split into a muted left half and a bright,
/// bold right half (KAM | UI) so the brand pops without shouting. Rendered centered while the
/// transcript has no messages yet; the chat view takes over on the first message.
const LOGO_LEFT: [&str; 6] = [
    "██╗  ██╗  █████╗  ███╗   ███╗",
    "██║ ██╔╝ ██╔══██╗ ████╗ ████║",
    "█████╔╝  ███████║ ██╔████╔██║",
    "██╔═██╗  ██╔══██║ ██║╚██╔╝██║",
    "██║  ██╗ ██║  ██║ ██║ ╚═╝ ██║",
    "╚═╝  ╚═╝ ╚═╝  ╚═╝ ╚═╝     ╚═╝",
];
const LOGO_RIGHT: [&str; 6] = [
    "██╗   ██╗██╗",
    "██║   ██║██║",
    "██║   ██║██║",
    "██║   ██║██║",
    "╚██████╔╝██║",
    " ╚═════╝ ╚═╝",
];

/// Smaller KAMUI for the exit screen — 4 rows, flat (no shadow)
/// so the sign-off stays crisp and clearly reads KAMUI.
pub(crate) const EXIT_LOGO_SMALL: [&str; 4] = [
    "██  ██   ███    █   █   █   █   ███ ",
    "██ ██   █   █   ██ ██   █   █    █  ",
    "████    █████   █ █ █   █   █    █  ",
    "██  ██  █   █   █   █    ███    ███ ",
];

fn lock_screen(screen: &Mutex<FullScreen>) -> MutexGuard<'_, FullScreen> {
    screen.lock().unwrap_or_else(PoisonError::into_inner)
}

fn palette() -> Option<crate::theme::Palette> {
    ACTIVE_THEME.with(|c| c.borrow().clone().and_then(|t| t.palette()))
}
fn themed(or: Color, f: impl FnOnce(&crate::theme::Palette) -> String) -> Color {
    if let Some(p) = palette() {
        crate::theme::ratatui_fg(&f(&p))
    } else {
        or
    }
}
#[allow(non_snake_case)]
fn TEXT() -> Color {
    themed(Color::Rgb(0xc0, 0xca, 0xf5), |p| p.fg.clone())
}
#[allow(non_snake_case)]
fn MUTED() -> Color {
    themed(Color::Rgb(0x56, 0x5f, 0x89), |p| p.muted.clone())
}
#[allow(non_snake_case)]
fn BORDER() -> Color {
    themed(Color::Rgb(0x41, 0x48, 0x68), |_| {
        palette().unwrap().muted.clone()
    })
}
#[allow(non_snake_case)]
fn BG_CHAT() -> Color {
    themed(Color::Rgb(0x1a, 0x1b, 0x26), |p| p.bg.clone())
}
#[allow(non_snake_case)]
fn BG_ELEMENT() -> Color {
    let (r, g, b) = crate::theme::hex_to_rgb(&palette().map(|p| p.bg).unwrap_or("#24283b".into()));
    Color::Rgb(
        r.saturating_sub(12),
        g.saturating_sub(12),
        b.saturating_sub(12),
    )
}
#[allow(non_snake_case)]
fn BG_PANEL() -> Color {
    let (r, g, b) = crate::theme::hex_to_rgb(&palette().map(|p| p.bg).unwrap_or("#1f2335".into()));
    Color::Rgb(
        r.saturating_sub(6),
        g.saturating_sub(6),
        b.saturating_sub(6),
    )
}
#[allow(non_snake_case)]
fn BLUE() -> Color {
    themed(Color::Rgb(0x7a, 0xa2, 0xf7), |p| p.blue.clone())
}
#[allow(non_snake_case)]
fn POPUP_BG() -> Color {
    BG_ELEMENT()
}
#[allow(non_snake_case)]
fn MATCH_BG() -> Color {
    themed(Color::Rgb(0x29, 0x2e, 0x42), |p| p.muted.clone())
}
#[allow(non_snake_case)]
fn MATCH_CURRENT_BG() -> Color {
    themed(Color::Rgb(0x3d, 0x59, 0xa1), |p| p.mauve.clone())
}
#[allow(non_snake_case)]
fn ACCENT() -> Color {
    themed(Color::Rgb(0xbb, 0x9a, 0xf7), |p| p.mauve.clone())
}
#[allow(non_snake_case)]
fn GREEN() -> Color {
    themed(Color::Rgb(0x9e, 0xce, 0x6a), |p| p.green.clone())
}
#[allow(non_snake_case)]
fn WARN() -> Color {
    themed(Color::Rgb(0xe0, 0xaf, 0x68), |p| p.amber.clone())
}
#[allow(non_snake_case)]
fn RED() -> Color {
    themed(Color::Rgb(0xf7, 0x76, 0x8e), |p| p.red.clone())
}
#[allow(non_snake_case)]
fn CYAN() -> Color {
    themed(Color::Rgb(0x7d, 0xcf, 0xff), |p| p.cyan.clone())
}
#[allow(non_snake_case)]
fn NOTICE_FG() -> Color {
    MUTED()
}
// Back-compat const aliases for code that still uses `TEXT()` without call — we replace via regex below to `TEXT()`
thread_local! { static ACTIVE_THEME: std::cell::RefCell<Option<crate::theme::Theme>> = const { std::cell::RefCell::new(None) }; }
const MAX_HISTORY_LINES: usize = 4_000;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CardKind {
    User,
    Tool,
    Output,
    Error,
    /// Command output and status lines. These used to live in a separate six-entry ring that
    /// rendered below every card, so they lost their place in the conversation and older ones
    /// silently fell off the end.
    Note,
}

#[derive(Debug, Clone)]
struct Card {
    /// Monotonic id. Clicks resolve to a card through this rather than a position, so a
    /// history trim between the draw and the click cannot toggle the wrong card.
    id: u64,
    kind: CardKind,
    title: String,
    body: String,
    /// Tool outcome ("completed - 1.2s - 142 chars") plus whether it succeeded. Rendered as
    /// its own row that stays visible when the card is folded, so a finished tool always
    /// reports how it ended without the output being unfolded.
    status: Option<(String, bool)>,
    collapsed: bool,
}

impl Card {
    /// What copying this cell yields. An answer copies as the raw Markdown that was streamed,
    /// with no rails or headers; anything else keeps its header and outcome, which are the
    /// parts that say what the body actually is.
    fn clipboard_text(&self) -> String {
        if self.title == "Assistant" {
            return self.body.clone();
        }
        let mut out = String::new();
        if !self.title.is_empty() {
            out.push_str(&self.title);
            out.push('\n');
        }
        if let Some((status, _)) = &self.status {
            out.push_str(status);
            out.push('\n');
        }
        out.push_str(&self.body);
        out
    }

    /// Body rows hidden behind the fold; 0 means there is nothing to expand.
    fn foldable_rows(&self) -> usize {
        if self.body.trim().is_empty() {
            0
        } else {
            self.body.lines().count()
        }
    }
}

#[derive(Debug, Clone)]
struct Model {
    header: String,
    cards: Vec<Card>,
    footer: String,
    /// Transcript viewport offset in wrapped rows counted from the bottom; 0 means "follow
    /// the tail". PageUp/PageDown/Home/End move it, typing snaps back.
    scroll_from_bottom: usize,
    prompt_visible: bool,
    thinking: Option<(usize, &'static str)>,
    /// True until the first message lands: the home screen shows the centered logo.
    intro: bool,
    /// OpenCode-style right rail: bold keys with muted values (session, model, context…).
    /// `None` hides it entirely (narrow terminals included).
    sidebar: Option<Vec<(String, String)>>,
    /// Live typed text rendered inside the editor box; driven by `ScreenHandle`.
    input: String,
    /// Caret position as a byte offset into `input`. Editing happens here, not only at the end.
    input_caret: usize,
    /// Autocomplete menu state mirrored from the input loop each keystroke.
    ac_items: Vec<(String, String)>,
    ac_selected: usize,
    /// Warning messages render separately so `/warnings` can hide or reveal them.
    warnings: Vec<String>,
    warnings_visible: bool,
    warning_details: Vec<String>,
    warning_details_visible: bool,
    /// Lines typed while the agent runs; shown in the footer until consumed.
    queued_count: usize,
    /// Open modal (model picker / session switcher), opencode-style.
    dialog: Option<DialogState>,
    /// `?` overlay with keybindings.
    help_visible: bool,
    /// First keybinding row shown in the `?` overlay; a short terminal scrolls the sheet
    /// rather than silently cutting the bindings that did not fit.
    help_scroll: usize,
    theme: crate::theme::Theme,
    /// Right-side status-bar badge ("5.9k tok 41%", amber past 80%).
    token_badge: Option<(String, u8)>,
    /// Open approval modal (opencode permission panel).
    permission: Option<PermissionState>,
    /// `ask_user` overlay — never `println` onto the alternate screen.
    ask: Option<AskState>,
    /// Hidden by the user with Ctrl+B, as opposed to dropped for want of room.
    sidebar_hidden: bool,
    /// Live transcript search (Ctrl+F). `/search` looks through saved sessions in SQLite; this
    /// looks through what is on screen right now, which is a different question.
    search: Option<SearchState>,
    plan: Option<crate::tools::PlanView>,
    /// Semantic action under the mouse. Coordinates never enter model state, so moving within
    /// one row does not trigger redundant redraws.
    hovered: Option<HitTarget>,
}

/// An in-progress transcript search: what was typed, and which match is being looked at.
#[derive(Debug, Clone, Default)]
pub struct SearchState {
    pub query: String,
    /// Index into the matching rows, wrapping at both ends.
    pub current: usize,
    /// Match count from the last draw, for the `3/17` readout.
    pub total: usize,
}

/// Approval modal options, opencode labels. First field is the typed hotkey (`y`/`a`/`n`).
pub const PERM_OPTIONS: [(&str, &str); 3] = [
    ("y", "Allow once"),
    ("a", "Always allow this session"),
    ("n", "Reject"),
];
pub const PLAN_OPTIONS: [(&str, &str); 2] = [("approve", "Approve and build"), ("n", "Reject")];

fn permission_hotkey(options: &[(&'static str, &'static str)], ch: char) -> Option<&'static str> {
    let ch = ch.to_ascii_lowercase();
    options
        .iter()
        .find(|(key, _)| key.starts_with(ch) || (ch == 'y' && *key == "approve"))
        .map(|(key, _)| *key)
}

#[derive(Debug, Clone)]
pub struct PermissionState {
    pub title: String,
    pub body: String,
    pub selected: usize,
    /// First body row shown. A patch diff is routinely longer than the modal, and the body was
    /// cut at ten rows with nothing saying so -- you were being asked to authorise a change you
    /// could not finish reading.
    pub scroll: usize,
    pub options: Vec<(&'static str, &'static str)>,
}

/// Clarifying question from the model (`ask_user`), rendered as a modal like permission.
#[derive(Debug, Clone)]
pub struct AskState {
    pub question: String,
    pub options: Vec<String>,
    pub selected: usize,
    pub typed: String,
}

/// A modal picker that submits an existing slash command on Enter — pure UI sugar over
/// `/model <name>` and `/resume <id>`, exactly like opencode's model/session dialogs.
#[derive(Debug, Clone)]
pub struct DialogState {
    pub title: String,
    pub prefix: String,
    pub items: Vec<(String, String)>,
    pub query: String,
    pub selected: usize,
    /// When true, the dialog is a checkbox list: Space toggles a mark, Enter applies all marks
    /// (submits one command per item), Esc cancels. `marks[i]` is the desired final state of
    /// `items[i]`. Text query filtering is disabled in this mode.
    pub multi_toggle: bool,
    pub marks: Vec<bool>,
}

impl DialogState {
    pub fn new(title: &str, prefix: &str, items: Vec<(String, String)>) -> Self {
        Self {
            title: title.to_string(),
            prefix: prefix.to_string(),
            items,
            query: String::new(),
            selected: 0,
            multi_toggle: false,
            marks: Vec::new(),
        }
    }

    pub fn new_toggle(title: &str, items: Vec<(String, String)>, marks: Vec<bool>) -> Self {
        Self {
            title: title.to_string(),
            prefix: "/mcp apply ".to_string(),
            items,
            query: String::new(),
            selected: 0,
            multi_toggle: true,
            marks,
        }
    }

    pub fn filtered(&self) -> Vec<&(String, String)> {
        let needle = self.query.to_ascii_lowercase();
        self.items
            .iter()
            .filter(|(value, label)| {
                value.to_ascii_lowercase().contains(&needle)
                    || label.to_ascii_lowercase().contains(&needle)
            })
            .collect()
    }
}

impl Default for Model {
    fn default() -> Self {
        Self {
            theme: crate::theme::Theme::default(),
            header: String::from("Kamui"),
            cards: Vec::new(),
            footer: String::from("? help"),
            scroll_from_bottom: 0,
            prompt_visible: true,
            thinking: None,
            intro: true,
            sidebar: None,
            input: String::new(),
            input_caret: 0,
            ac_items: Vec::new(),
            ac_selected: 0,
            warnings: Vec::new(),
            warnings_visible: true,
            warning_details: Vec::new(),
            warning_details_visible: false,
            queued_count: 0,
            dialog: None,
            help_visible: false,
            help_scroll: 0,
            token_badge: None,
            permission: None,
            ask: None,
            sidebar_hidden: false,
            search: None,
            plan: None,
            hovered: None,
        }
    }
}

struct FullScreen {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    model: Model,
    /// Transcript viewport height from the most recent draw; PageUp/PageDown use it as the
    /// scroll page size.
    last_viewport_rows: usize,
    /// Terminal row -> card id from the most recent draw, so a mouse click can find the card
    /// under the pointer.
    last_card_rows: Vec<(u16, u64)>,
    /// Bounded interactive regions from the most recent draw. Later entries sit above earlier
    /// ones, matching the order widgets are painted.
    last_hits: Vec<HitRegion>,
    /// Transcript width and height from the last draw. Search re-wraps the transcript exactly
    /// as the renderer did, so the row it scrolls to is the row the user will see.
    last_transcript_width: u16,
    next_card_id: u64,
    /// Set once the real terminal has been handed back. Further draws are dropped so a
    /// still-running input thread cannot repaint over restored scrollback.
    restored: bool,
}

impl FullScreen {
    #[allow(dead_code)]
    fn new(header: String) -> Result<Self> {
        Self::new_with_theme(header, crate::theme::Theme::default())
    }
    fn new_with_theme(header: String, theme: crate::theme::Theme) -> Result<Self> {
        // If anything panics mid-draw, still restore the terminal instead of leaving it raw.
        let previous_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = disable_raw_mode();
            let _ = execute!(
                io::stdout(),
                SetCursorStyle::DefaultUserShape,
                DisableBracketedPaste,
                LeaveAlternateScreen,
                DisableMouseCapture
            );
            previous_hook(info);
        }));
        let mut stdout = io::stdout();
        enable_raw_mode().context("could not enable raw mode")?;
        execute!(
            stdout,
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste,
            SetCursorStyle::BlinkingBar
        )
        .context("could not enter alternate screen")?;
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = match Terminal::new(backend) {
            Ok(terminal) => terminal,
            Err(error) => {
                let mut stdout = io::stdout();
                let _ = execute!(stdout, LeaveAlternateScreen, DisableMouseCapture);
                let _ = disable_raw_mode();
                return Err(error).context("could not create Ratatui terminal");
            }
        };
        if let Err(error) = terminal.clear() {
            let mut stdout = io::stdout();
            let _ = execute!(stdout, LeaveAlternateScreen, DisableMouseCapture);
            let _ = disable_raw_mode();
            return Err(error).context("could not clear terminal");
        }
        let mut model = Model {
            header,
            ..Model::default()
        };
        model.theme = theme;
        let mut screen = Self {
            terminal,
            model,
            last_viewport_rows: 0,
            last_card_rows: Vec::new(),
            last_hits: Vec::new(),
            last_transcript_width: 0,
            next_card_id: 0,
            restored: false,
        };
        screen.draw()?;
        Ok(screen)
    }

    fn draw(&mut self) -> Result<()> {
        if self.restored {
            return Ok(());
        }
        let model = self.model.clone();
        let mut info = RenderInfo::default();
        self.terminal.draw(|frame| {
            info = render(frame, &model);
        })?;
        self.last_viewport_rows = info.viewport_rows;
        self.last_card_rows = info.card_rows;
        self.last_hits = info.hits;
        self.last_transcript_width = info.transcript_width;
        Ok(())
    }

    fn take_card_id(&mut self) -> u64 {
        self.next_card_id += 1;
        self.next_card_id
    }

    fn set_header(&mut self, header: String) -> Result<()> {
        self.model.header = header;
        self.draw()
    }

    fn add_card(
        &mut self,
        kind: CardKind,
        title: impl Into<String>,
        body: impl Into<String>,
    ) -> Result<()> {
        if !matches!(kind, CardKind::Note) {
            self.model.intro = false;
        }
        let title = title.into();
        let body = body.into();
        // Agent noise starts folded: every tool call collapses to a two-line peek, and any
        // output longer than two lines joins it. Long error dumps fold to a headline so they
        // cannot collide with the editor. Ctrl+O, a click, or `/expand` / `/collapse` toggle
        // it; answers always show in full.
        let collapsed = match kind {
            CardKind::Tool => true,
            CardKind::Output => title != "Assistant" && body.lines().count() > 2,
            CardKind::Error => error_should_fold(&body),
            _ => false,
        };
        let id = self.take_card_id();
        self.model.cards.push(Card {
            id,
            kind,
            title,
            body,
            status: None,
            collapsed,
        });
        self.trim_history();
        self.draw()
    }

    fn update_assistant(&mut self, body: String) -> Result<()> {
        self.model.intro = false;
        match self.model.cards.last_mut() {
            Some(card) if matches!(card.kind, CardKind::Output) && card.title == "Assistant" => {
                card.body = body;
            }
            _ => {
                let id = self.take_card_id();
                self.model.cards.push(Card {
                    id,
                    kind: CardKind::Output,
                    title: "Assistant".to_string(),
                    body,
                    status: None,
                    collapsed: false,
                });
            }
        }
        self.trim_history();
        self.draw()
    }

    /// Records a tool's outcome. It lands on the pending `Tool` card when there is one, so a
    /// call and its result render as a single block; otherwise it becomes a card of its own.
    fn finish_tool(&mut self, outcome: String, ok: bool, body: String) -> Result<()> {
        self.model.intro = false;
        match self.model.cards.last_mut() {
            Some(card) if matches!(card.kind, CardKind::Tool) && card.status.is_none() => {
                // The call's arguments were the peek while it ran; real output replaces them,
                // and a silent tool keeps its arguments so the card is not left blank.
                if !body.is_empty() {
                    card.body = body;
                }
                card.status = Some((outcome, ok));
                card.collapsed = true;
            }
            _ => {
                let id = self.take_card_id();
                self.model.cards.push(Card {
                    id,
                    kind: if ok {
                        CardKind::Output
                    } else {
                        CardKind::Error
                    },
                    title: "Tool Output".to_string(),
                    body,
                    status: Some((outcome, ok)),
                    collapsed: true,
                });
            }
        }
        self.trim_history();
        self.draw()
    }

    /// Flips the fold on the newest card that has anything folded away.
    fn toggle_last_card(&mut self) -> Result<bool> {
        let Some(card) = self.last_foldable_mut() else {
            return Ok(false);
        };
        card.collapsed = !card.collapsed;
        self.draw()?;
        Ok(true)
    }

    /// Opens transcript search, closing the slash menu so the two never share the row.
    fn open_search(&mut self) -> Result<()> {
        self.model.search = Some(SearchState::default());
        self.model.ac_items.clear();
        self.draw()
    }

    fn close_search(&mut self) -> Result<()> {
        self.model.search = None;
        // Searching scrolls back through history; closing it returns to the live tail, which is
        // where the next answer will appear.
        self.model.scroll_from_bottom = 0;
        self.draw()
    }

    /// Applies an edit to the query and re-runs it from the top.
    fn edit_search(&mut self, edit: impl FnOnce(&mut String)) -> Result<()> {
        let Some(search) = self.model.search.as_mut() else {
            return Ok(());
        };
        edit(&mut search.query);
        search.current = 0;
        self.refresh_search()
    }

    /// Steps to the next or previous match, wrapping at both ends.
    fn step_search(&mut self, delta: isize) -> Result<()> {
        let Some(search) = self.model.search.as_mut() else {
            return Ok(());
        };
        if search.total > 0 {
            let total = search.total as isize;
            let current = search.current as isize;
            search.current = ((current + delta).rem_euclid(total)) as usize;
        }
        self.refresh_search()
    }

    /// Recounts the matches and scrolls the current one into view. The transcript is re-wrapped
    /// exactly as the renderer wraps it, so the row counted here is the row that gets drawn.
    fn refresh_search(&mut self) -> Result<()> {
        let width = self.last_transcript_width;
        let visible = self.last_viewport_rows.max(1);
        let Some(query) = self.model.search.as_ref().map(|s| s.query.clone()) else {
            return Ok(());
        };
        let rows = wrapped_transcript(&self.model, width);
        let hits = matching_rows(&rows, &query);
        if let Some(search) = self.model.search.as_mut() {
            search.total = hits.len();
            if hits.is_empty() {
                search.current = 0;
            } else {
                search.current %= hits.len();
            }
        }
        if let Some(row) = self
            .model
            .search
            .as_ref()
            .filter(|_| !hits.is_empty())
            .map(|search| hits[search.current])
        {
            self.model.scroll_from_bottom = scroll_to_row(rows.len(), visible, row);
        }
        self.draw()
    }

    /// Copies the cell drawn at `row` to the system clipboard, reporting what was taken.
    /// Mouse capture means the terminal's own drag-select is unavailable, so the transcript
    /// needs its own way to get text out.
    fn copy_card_at_row(&mut self, row: u16) -> Result<()> {
        let id = self
            .last_card_rows
            .iter()
            .find(|(y, _)| *y == row)
            .map(|(_, id)| *id);
        let text = id
            .and_then(|id| self.model.cards.iter().find(|card| card.id == id))
            .map(Card::clipboard_text);
        self.copy_reporting(text, "cell")
    }

    /// Copies the newest answer, or failing that the newest cell with any text in it.
    fn copy_latest(&mut self) -> Result<()> {
        let newest_answer = self
            .model
            .cards
            .iter()
            .rev()
            .find(|card| card.title == "Assistant" && !card.body.trim().is_empty());
        let (text, what) = match newest_answer {
            Some(card) => (Some(card.clipboard_text()), "answer"),
            None => (
                self.model
                    .cards
                    .iter()
                    .rev()
                    .find(|card| !card.clipboard_text().trim().is_empty())
                    .map(Card::clipboard_text),
                "cell",
            ),
        };
        self.copy_reporting(text, what)
    }

    fn copy_reporting(&mut self, text: Option<String>, what: &str) -> Result<()> {
        let Some(text) = text.filter(|text| !text.trim().is_empty()) else {
            return self.add_notice(format!("nothing to copy under the {what}"));
        };
        let characters = text.chars().count();
        match set_clipboard_text(&text) {
            Ok(()) => self.add_notice(format!("copied {characters} chars ({what}) to clipboard")),
            Err(error) => self.add_notice(format!("could not copy: {error:#}")),
        }
    }

    /// Flips the fold on whichever card was drawn at `row`, for click-to-expand.
    fn toggle_card_at_row(&mut self, row: u16) -> Result<bool> {
        let Some((_, id)) = self.last_card_rows.iter().find(|(y, _)| *y == row).copied() else {
            return Ok(false);
        };
        let Some(card) = self.model.cards.iter_mut().find(|card| card.id == id) else {
            return Ok(false);
        };
        if card.foldable_rows() == 0 {
            return Ok(false);
        }
        card.collapsed = !card.collapsed;
        self.draw()?;
        Ok(true)
    }

    /// Folds or unfolds the newest card that actually has hidden rows -- the same card Ctrl+O
    /// toggles. Aiming at the literal last card made `/expand` target the note cell holding the
    /// command's own output rather than the tool output the user meant.
    fn set_last_collapsed(&mut self, collapsed: bool) -> Result<bool> {
        let Some(card) = self.last_foldable_mut() else {
            return Ok(false);
        };
        card.collapsed = collapsed;
        self.draw()?;
        Ok(true)
    }

    fn last_foldable_mut(&mut self) -> Option<&mut Card> {
        let index = last_foldable_index(&self.model.cards)?;
        self.model.cards.get_mut(index)
    }

    /// Appends to the note cell that is already open, or starts one. Consecutive lines from a
    /// single command stay in one cell; anything else pushed in between ends it naturally.
    fn add_notice(&mut self, text: impl Into<String>) -> Result<()> {
        let text = text.into();
        match self.model.cards.last_mut() {
            Some(card) if matches!(card.kind, CardKind::Note) => {
                if !card.body.is_empty() {
                    card.body.push('\n');
                }
                card.body.push_str(&text);
            }
            _ => {
                let id = self.take_card_id();
                self.model.cards.push(Card {
                    id,
                    kind: CardKind::Note,
                    title: String::new(),
                    body: text,
                    status: None,
                    collapsed: false,
                });
            }
        }
        self.trim_history();
        self.draw()
    }

    /// Opens a fresh cell headed by the command the user ran, so its output is attributed
    /// instead of merging into whatever was printed before it.
    fn add_command(&mut self, command: String) -> Result<()> {
        let id = self.take_card_id();
        self.model.cards.push(Card {
            id,
            kind: CardKind::Note,
            title: command,
            body: String::new(),
            status: None,
            collapsed: false,
        });
        self.trim_history();
        self.draw()
    }

    fn add_warning(&mut self, text: String) -> Result<()> {
        self.model.warnings.push(text);
        if self.model.warnings.len() > 32 {
            self.model.warnings.remove(0);
        }
        self.draw()
    }

    fn prompt(&mut self) -> Result<()> {
        self.model.prompt_visible = true;
        self.draw()
    }

    fn trim_history(&mut self) {
        let mut line_count = 0usize;
        for card in self.model.cards.iter().rev() {
            line_count += card.body.lines().count() + 3;
            if line_count > MAX_HISTORY_LINES {
                break;
            }
        }
        if self.model.cards.len() > MAX_HISTORY_LINES {
            let keep_from = self.model.cards.len().saturating_sub(MAX_HISTORY_LINES);
            self.model.cards.drain(..keep_from);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum HitTarget {
    Card(u64),
    Autocomplete(usize),
    Dialog(usize),
    Permission(usize),
    Ask(usize),
    Sidebar(SidebarAction),
    Footer(FooterAction),
    Overlay,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SidebarAction {
    Session,
    Model,
    Mode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FooterAction {
    Help,
    Models,
    Sessions,
    Interrupt,
    Live,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HitRegion {
    area: Rect,
    target: HitTarget,
}

fn point_in(rect: Rect, column: u16, row: u16) -> bool {
    column >= rect.x && column < rect.right() && row >= rect.y && row < rect.bottom()
}

fn hit_at(hits: &[HitRegion], column: u16, row: u16) -> Option<HitTarget> {
    hits.iter()
        .rev()
        .find(|hit| point_in(hit.area, column, row))
        .map(|hit| hit.target.clone())
}

impl FullScreen {
    /// Hands the real terminal back: leaves the alternate screen, drops raw mode and mouse
    /// capture. Idempotent, because `Drop` also calls it for the paths that never shut down
    /// explicitly.
    fn restore(&mut self) {
        if self.restored {
            return;
        }
        self.restored = true;
        let _ = self.terminal.show_cursor();
        let _ = execute!(
            self.terminal.backend_mut(),
            SetCursorStyle::DefaultUserShape,
            DisableBracketedPaste,
            LeaveAlternateScreen,
            DisableMouseCapture
        );
        let _ = disable_raw_mode();
        let _ = self.terminal.backend_mut().flush();
    }
}

impl Drop for FullScreen {
    fn drop(&mut self) {
        self.restore();
    }
}

/// Unified output surface. Plain mode preserves the existing scrollback contract; interactive
/// mode uses a retained Ratatui transcript while the agent loop remains provider-agnostic.
/// The fullscreen terminal sits behind a mutex so the thinking-animation ticker can redraw it
/// from a background task while the chat loop is blocked awaiting the model stream.
pub struct ChatUi {
    plain: Ui,
    fullscreen: Option<Arc<Mutex<FullScreen>>>,
    thinking: Option<ThinkingHandle>,
}

struct ThinkingHandle {
    stop: Arc<tokio::sync::Notify>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for ThinkingHandle {
    fn drop(&mut self) {
        self.stop.notify_one();
        self.task.abort();
    }
}

impl ChatUi {
    #[allow(dead_code)]
    pub fn new(interactive: bool, header: String) -> Result<Self> {
        Self::new_with_theme(interactive, header, crate::theme::Theme::default())
    }
    pub fn new_with_theme(
        interactive: bool,
        header: String,
        theme: crate::theme::Theme,
    ) -> Result<Self> {
        let plain = Ui::stdio();
        let fullscreen = interactive
            .then(|| {
                FullScreen::new_with_theme(header, theme).map(|screen| Arc::new(Mutex::new(screen)))
            })
            .transpose()?;
        Ok(Self {
            plain,
            fullscreen,
            thinking: None,
        })
    }
    pub fn set_theme(&self, theme: crate::theme::Theme) {
        if let Some(fs) = &self.fullscreen {
            let mut s = lock_screen(fs);
            s.model.theme = theme;
            let _ = s.draw();
        }
    }

    pub fn is_fullscreen(&self) -> bool {
        self.fullscreen.is_some()
    }

    fn screen(&self) -> MutexGuard<'_, FullScreen> {
        lock_screen(
            self.fullscreen
                .as_ref()
                .expect("fullscreen surface must exist"),
        )
    }

    /// Start the bouncing-wall loading animation in the editor while the model thinks. No-op
    /// outside fullscreen mode; plain mode keeps its inline spinner.
    pub fn thinking_start(&mut self, label: &'static str) -> Result<()> {
        if !self.is_fullscreen() || self.thinking.is_some() {
            return Ok(());
        }
        {
            let mut screen = self.screen();
            screen.model.thinking = Some((0, label));
            screen.draw()?;
        }
        let shared = self.fullscreen.clone().expect("fullscreen surface");
        let stop = Arc::new(tokio::sync::Notify::new());
        let stop_task = stop.clone();
        let task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(100));
            loop {
                tokio::select! {
                    _ = stop_task.notified() => break,
                    _ = interval.tick() => {
                        let mut screen = lock_screen(&shared);
                        if let Some((frame, _)) = screen.model.thinking.as_mut() {
                            *frame = frame.wrapping_add(1);
                            let _ = screen.draw();
                        } else {
                            break;
                        }
                    }
                }
            }
            let mut screen = lock_screen(&shared);
            screen.model.thinking = None;
            let _ = screen.draw();
        });
        self.thinking = Some(ThinkingHandle { stop, task });
        Ok(())
    }

    /// Stop the loading animation and leave the footer clean for the next prompt.
    pub async fn thinking_stop(&mut self) {
        if let Some(mut handle) = self.thinking.take() {
            handle.stop.notify_one();
            let task = std::mem::replace(&mut handle.task, tokio::spawn(async {}));
            let _ = task.await;
        }
    }

    /// Replace the right-rail contents (opencode-style session info). Hidden on narrow
    /// terminals; no-op outside fullscreen mode.
    pub fn set_sidebar(&mut self, entries: Vec<(String, String)>) -> Result<()> {
        match self.fullscreen.as_ref() {
            Some(screen) => {
                let mut screen = lock_screen(screen);
                screen.model.sidebar = Some(entries);
                screen.draw()
            }
            None => Ok(()),
        }
    }

    pub fn set_plan(&mut self, plan: Option<crate::tools::PlanView>) -> Result<()> {
        match self.fullscreen.as_ref() {
            Some(screen) => {
                let mut screen = lock_screen(screen);
                screen.model.plan = plan;
                screen.draw()
            }
            None => Ok(()),
        }
    }

    pub fn set_header(&mut self, header: String) -> Result<()> {
        match self.fullscreen.as_ref() {
            Some(screen) => lock_screen(screen).set_header(header),
            None => {
                print!("{header}");
                io::stdout().flush()?;
                Ok(())
            }
        }
    }

    pub fn prompt(&mut self) -> Result<()> {
        match self.fullscreen.as_ref() {
            Some(screen) => lock_screen(screen).prompt(),
            None => {
                print!(
                    "{}",
                    self.plain
                        .style("\u{276f} ", &[AnsiStyle::Cyan, AnsiStyle::Bold])
                );
                io::stdout().flush()?;
                Ok(())
            }
        }
    }

    pub fn user(&mut self, text: &str) -> Result<()> {
        self.card(CardKind::User, "User", text)
    }

    /// A message folded into a turn that was already running. Rendered as an ordinary user
    /// message otherwise, and on re-reading a session there was no way to tell which of two
    /// prompts was the one that started the work.
    pub fn user_steering(&mut self, text: &str) -> Result<()> {
        self.card(
            CardKind::User,
            "steering \u{2192} added to the running turn",
            text,
        )
    }

    /// Echoes a slash command as its own transcript cell; its output lands in the same cell.
    pub fn command_echo(&mut self, command: &str) -> Result<()> {
        match self.fullscreen.as_ref() {
            Some(screen) => lock_screen(screen).add_command(command.to_string()),
            None => Ok(()),
        }
    }

    pub fn tool_call(&mut self, name: &str, args: &str) -> Result<()> {
        match self.fullscreen.as_ref() {
            Some(screen) => {
                lock_screen(screen).add_card(CardKind::Tool, tool_header(name, args), args)
            }
            None => {
                print!(
                    "{}",
                    crate::render::render_tool_call(name, args, self.plain)
                );
                Ok(())
            }
        }
    }

    /// Reports a finished tool: `outcome` is the one-line result summary and always stays
    /// visible, `text` is the output hidden behind the fold. In plain (non-TUI) mode both are
    /// printed, matching the previous line-oriented output.
    pub fn tool_finished(&mut self, outcome: &str, ok: bool, text: &str) -> Result<()> {
        match self.fullscreen.as_ref() {
            Some(screen) => {
                lock_screen(screen).finish_tool(outcome.to_string(), ok, text.to_string())
            }
            None => {
                if !text.is_empty() {
                    if ok {
                        print!("{}", crate::render::render_tool_output(text, self.plain));
                    } else {
                        print!("{}", crate::render::render_error(text, self.plain));
                    }
                }
                println!("{outcome}");
                io::stdout().flush().ok();
                Ok(())
            }
        }
    }

    pub fn expand_last(&mut self) -> Result<bool> {
        match self.fullscreen.as_ref() {
            Some(screen) => lock_screen(screen).set_last_collapsed(false),
            None => Ok(false),
        }
    }

    pub fn collapse_last(&mut self) -> Result<bool> {
        match self.fullscreen.as_ref() {
            Some(screen) => lock_screen(screen).set_last_collapsed(true),
            None => Ok(false),
        }
    }

    pub fn assistant_update(&mut self, raw_markdown: &str) -> Result<()> {
        match self.fullscreen.as_ref() {
            Some(screen) => lock_screen(screen).update_assistant(raw_markdown.to_string()),
            None => {
                let mut renderer = crate::markdown::Renderer::for_stdout();
                print!("{}", renderer.render_block(raw_markdown));
                io::stdout().flush()?;
                Ok(())
            }
        }
    }

    /// Appends a past answer as its own cell. Replay must not go through `assistant_update`:
    /// that one *replaces* the last assistant card, because it exists to grow a card as tokens
    /// stream in. Two stored answers in a row would overwrite each other.
    pub fn assistant_replay(&mut self, text: &str) -> Result<()> {
        self.card(CardKind::Output, "Assistant", text)
    }

    pub fn assistant_done(&mut self) -> Result<()> {
        match self.fullscreen.as_ref() {
            Some(screen) => lock_screen(screen).draw(),
            None => {
                println!();
                Ok(())
            }
        }
    }

    pub fn notice(&mut self, text: &str) -> Result<()> {
        match self.fullscreen.as_ref() {
            Some(screen) => lock_screen(screen).add_notice(text),
            None => {
                println!("{text}");
                Ok(())
            }
        }
    }

    pub fn warning(&mut self, text: &str) -> Result<()> {
        match self.fullscreen.as_ref() {
            Some(screen) => lock_screen(screen).add_warning(text.to_string()),
            None => {
                print!("{}", crate::render::render_warning(text, self.plain));
                io::stdout().flush()?;
                Ok(())
            }
        }
    }

    /// Show or hide warning messages in the transcript (`/warnings`).
    /// Toggles the keybinding sheet overlay (`?` and `/help` in TUI mode).
    pub fn toggle_help(&mut self) -> Result<()> {
        match self.fullscreen.as_ref() {
            Some(screen_arc) => {
                let mut s = lock_screen(screen_arc);
                s.model.help_visible = !s.model.help_visible;
                s.model.help_scroll = 0;
                s.draw()
            }
            None => Ok(()),
        }
    }

    /// Leaves the logo home screen (any command output means real UI begins).
    pub fn leave_intro(&mut self) -> Result<()> {
        match self.fullscreen.as_ref() {
            Some(screen_arc) => {
                let mut s = lock_screen(screen_arc);
                s.model.intro = false;
                s.draw()
            }
            None => Ok(()),
        }
    }

    /// Status-bar badge: text plus context percent (amber at/above 80%).
    pub fn set_token_badge(&mut self, badge: Option<(String, u8)>) -> Result<()> {
        match self.fullscreen.as_ref() {
            Some(screen) => {
                let mut screen = lock_screen(screen);
                screen.model.token_badge = badge;
                screen.draw()
            }
            None => Ok(()),
        }
    }

    /// Per-warning detail lines (paths + reasons).
    /// Replaces the warning banner wholesale. `/warnings fix` re-checks the skills after the
    /// repair turn, and a banner that still lists folders which now load is worse than none.
    pub fn set_warnings(&mut self, lines: Vec<String>) -> Result<()> {
        match self.fullscreen.as_ref() {
            Some(screen) => {
                let mut screen = lock_screen(screen);
                screen.model.warnings = lines;
                screen.draw()
            }
            None => Ok(()),
        }
    }

    pub fn set_warning_details(&mut self, details: Vec<String>) -> Result<()> {
        match self.fullscreen.as_ref() {
            Some(screen) => {
                let mut screen = lock_screen(screen);
                screen.model.warning_details = details;
                screen.draw()
            }
            None => Ok(()),
        }
    }

    pub fn set_warnings_expanded(&mut self, expanded: bool) -> Result<()> {
        match self.fullscreen.as_ref() {
            Some(screen) => {
                let mut screen = lock_screen(screen);
                screen.model.warning_details_visible = expanded;
                screen.draw()
            }
            None => Ok(()),
        }
    }

    pub fn set_warnings_visible(&mut self, visible: bool) -> Result<()> {
        match self.fullscreen.as_ref() {
            Some(screen) => {
                let mut screen = lock_screen(screen);
                screen.model.warnings_visible = visible;
                screen.draw()
            }
            None => Ok(()),
        }
    }

    pub fn error(&mut self, text: &str) -> Result<()> {
        match self.fullscreen.as_ref() {
            Some(screen) => lock_screen(screen).add_card(CardKind::Error, "Error", text),
            None => {
                eprintln!(
                    "{}",
                    self.plain
                        .tool_outcome(&format!("Error: {text}"), Duration::ZERO)
                );
                Ok(())
            }
        }
    }

    fn card(
        &mut self,
        kind: CardKind,
        title: impl Into<String>,
        body: impl Into<String>,
    ) -> Result<()> {
        match self.fullscreen.as_ref() {
            Some(screen) => lock_screen(screen).add_card(kind, title, body),
            None => {
                let title = title.into();
                let body = body.into();
                let rendered = match kind {
                    CardKind::User => crate::render::render_user_prompt(&body, self.plain),
                    CardKind::Tool => crate::render::render_tool_call(
                        title.strip_prefix("Tool: ").unwrap_or(&title),
                        &body,
                        self.plain,
                    ),
                    CardKind::Output => crate::render::render_tool_output(&body, self.plain),
                    CardKind::Error => crate::render::render_error(&body, self.plain),
                    // Plain mode has no cells; a note is just a line of output.
                    CardKind::Note => format!(
                        "{body}
"
                    ),
                };
                print!("{rendered}");
                Ok(())
            }
        }
    }

    /// Leaves fullscreen and restores the real terminal. Anything printed after this lands in
    /// the user's scrollback; anything printed *before* it goes to the alternate screen, which
    /// is discarded on the way out. The exit summary has to come after.
    ///
    /// The terminal is restored in place rather than by dropping the handle: the input thread
    /// holds its own clone, so `Drop` would not run until that thread also lets go.
    pub fn leave_fullscreen(&mut self) {
        if let Some(screen) = self.fullscreen.take() {
            lock_screen(&screen).restore();
        }
    }

    /// Shareable key into the fullscreen terminal so a blocking input thread can render the
    /// live editor while the async chat loop awaits.
    pub fn screen_handle(&self) -> Option<ScreenHandle> {
        self.fullscreen.clone().map(ScreenHandle)
    }
}

// ---------------------------------------------------------------- input hub -----------------

/// Events the chat loop consumes from the keyboard hub.
pub enum HubEvent {
    /// A submitted line at an idle prompt.
    Line(String),
    /// Ctrl+C at an idle prompt — shut down.
    Quit,
}

/// Owns the keyboard for the whole session, opencode-style. While the agent runs the editor
/// stays live: typed lines queue instead of racing the turn, Esc raises an interrupt, and
/// page keys keep scrolling. Approval / ask_user prompts register a one-shot requester whose
/// answer bypasses the queue.
pub struct InputHub {
    rx: tokio::sync::mpsc::UnboundedReceiver<HubEvent>,
    pub interrupt: Arc<tokio::sync::Notify>,
    busy: Arc<std::sync::atomic::AtomicBool>,
    queue: Arc<Mutex<VecDeque<String>>>,
    requester: Arc<Mutex<Option<tokio::sync::oneshot::Sender<String>>>>,
    candidates: Arc<std::sync::RwLock<Vec<crate::tui::Candidate>>>,
    screen: ScreenHandle,
    models_src: Arc<std::sync::RwLock<Vec<(String, String)>>>,
    sessions_src: Arc<std::sync::RwLock<Vec<(String, String)>>>,
    path_candidates: Arc<std::sync::RwLock<Vec<String>>>,
}

impl InputHub {
    /// Spawns the persistent keyboard thread. Call once at startup in TUI mode.
    pub fn spawn(screen: ScreenHandle) -> Self {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let hub_screen = screen.clone();
        let interrupt = Arc::new(tokio::sync::Notify::new());
        let busy = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let queue = Arc::new(Mutex::new(VecDeque::new()));
        let requester: Arc<Mutex<Option<tokio::sync::oneshot::Sender<String>>>> =
            Arc::new(Mutex::new(None));
        let candidates = Arc::new(std::sync::RwLock::new(Vec::new()));
        let models_src = Arc::new(std::sync::RwLock::new(Vec::new()));
        let sessions_src = Arc::new(std::sync::RwLock::new(Vec::new()));
        let path_candidates = Arc::new(std::sync::RwLock::new(Vec::new()));
        std::thread::spawn({
            let interrupt = interrupt.clone();
            let busy = busy.clone();
            let queue = queue.clone();
            let requester = requester.clone();
            let candidates = candidates.clone();
            let models_src = models_src.clone();
            let sessions_src = sessions_src.clone();
            let path_candidates = path_candidates.clone();
            move || {
                input_thread(
                    screen,
                    tx,
                    interrupt,
                    busy,
                    queue,
                    requester,
                    candidates,
                    models_src,
                    sessions_src,
                    path_candidates,
                )
            }
        });
        Self {
            rx,
            interrupt,
            busy,
            queue,
            requester,
            candidates,
            screen: hub_screen,
            models_src,
            sessions_src,
            path_candidates,
        }
    }

    /// Refreshes the slash-menu snapshot (commands/skills change rarely).
    pub fn set_candidates(&self, candidates: Vec<crate::tui::Candidate>) {
        *self
            .candidates
            .write()
            .unwrap_or_else(PoisonError::into_inner) = candidates;
    }

    pub fn set_path_candidates(&self, candidates: Vec<String>) {
        *self
            .path_candidates
            .write()
            .unwrap_or_else(PoisonError::into_inner) = candidates;
    }

    /// Sources for the Ctrl+K model picker: (submit value, display label).
    pub fn set_models(&self, items: Vec<(String, String)>) {
        *self
            .models_src
            .write()
            .unwrap_or_else(PoisonError::into_inner) = items;
    }

    /// Opens the session switcher (Ctrl+S path shares this).
    pub fn open_sessions_dialog(&self) -> bool {
        let items = self
            .sessions_src
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if items.is_empty() {
            return false;
        }
        {
            let mut s = lock_screen(&self.screen.0);
            s.model.dialog = Some(DialogState::new("Resume Session", "/resume ", items));
        }
        let _ = self.screen.draw_now();
        true
    }

    /// Generic modal picker: Enter submits prefix+value as a chat line.
    pub fn open_dialog(&self, title: &str, prefix: &str, items: Vec<(String, String)>) -> bool {
        if items.is_empty() {
            return false;
        }
        {
            let mut s = lock_screen(&self.screen.0);
            s.model.dialog = Some(DialogState::new(title, prefix, items));
        }
        let _ = self.screen.draw_now();
        true
    }

    /// Opens the skills picker. It goes through the same dialog machinery as the model and
    /// session pickers on purpose: the previous popup drove `console::Term` directly, reading
    /// keys from the same stdin this hub's thread is already blocked on and painting raw ANSI
    /// into the alternate screen ratatui owns and repaints.
    pub fn open_skills_dialog(&self, items: Vec<(String, String)>) -> bool {
        if items.is_empty() {
            return false;
        }
        {
            let mut s = lock_screen(&self.screen.0);
            s.model.dialog = Some(DialogState::new("Skills", "/skills toggle ", items));
        }
        let _ = self.screen.draw_now();
        true
    }

    pub fn open_models_dialog(&self) -> bool {
        let items = self
            .models_src
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if items.is_empty() {
            return false;
        }
        {
            let mut s = lock_screen(&self.screen.0);
            s.model.dialog = Some(DialogState::new("Select Model", "/model ", items));
        }
        let _ = self.screen.draw_now();
        true
    }

    pub fn open_themes_dialog(&self) -> bool {
        let items: Vec<(String, String)> = crate::theme::Theme::all()
            .into_iter()
            .map(|t| (t.to_string(), t.to_string()))
            .collect();
        {
            let mut s = lock_screen(&self.screen.0);
            s.model.dialog = Some(DialogState::new("Select Theme", "/theme ", items));
        }
        let _ = self.screen.draw_now();
        true
    }

    pub fn open_mcp_dialog(&self, items: Vec<(String, String)>, marks: Vec<bool>) -> bool {
        if items.is_empty() {
            return false;
        }
        {
            let mut s = lock_screen(&self.screen.0);
            s.model.dialog = Some(DialogState::new_toggle("MCP Servers", items, marks));
        }
        let _ = self.screen.draw_now();
        true
    }
    /// Sources for the Ctrl+S session switcher.
    pub fn set_sessions(&self, items: Vec<(String, String)>) {
        *self
            .sessions_src
            .write()
            .unwrap_or_else(PoisonError::into_inner) = items;
    }

    /// Marks the agent busy while the guard lives; Drop returns the prompt to idle.
    pub fn busy_guard(&self) -> BusyGuard {
        self.busy.store(true, std::sync::atomic::Ordering::SeqCst);
        BusyGuard {
            busy: self.busy.clone(),
        }
    }

    /// Next event from the keyboard.
    pub async fn next(&mut self) -> Option<HubEvent> {
        self.rx.recv().await
    }

    /// Enqueues a prompt programmatically (e.g. `/warnings fix`). Runs on the next idle pass.
    pub fn push_prompt(&self, line: String) {
        self.queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push_back(line);
    }

    /// Pops a queued line, if any; drained between turns. Updates the footer count.
    pub fn pop_queue(&self) -> Option<String> {
        let popped = self
            .queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front();
        if popped.is_some() {
            let mut s = lock_screen(&self.screen.0);
            s.model.queued_count = self
                .queue
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .len();
            drop(s);
            let _ = self.screen.draw_now();
        }
        popped
    }

    /// Waits for a one-line answer (approval prompts, ask_user). Esc answers `None`.
    pub async fn request_line(&mut self) -> Option<String> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        *self
            .requester
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(tx);
        rx.await.ok()
    }

    /// Opens/closes the approval modal from the keyboard-thread side.
    pub fn open_permission_modal(&self, title: &str, body: String) {
        self.open_permission_modal_with_options(title, body, PERM_OPTIONS.to_vec());
    }

    pub fn open_permission_modal_with_options(
        &self,
        title: &str,
        body: String,
        options: Vec<(&'static str, &'static str)>,
    ) {
        {
            let mut s = lock_screen(&self.screen.0);
            s.model.permission = Some(PermissionState {
                title: title.to_string(),
                body,
                selected: 0,
                scroll: 0,
                options,
            });
        }
        let _ = self.screen.draw_now();
    }

    pub fn close_permission_modal(&self) {
        {
            let mut s = lock_screen(&self.screen.0);
            s.model.permission = None;
        }
        let _ = self.screen.draw_now();
    }

    pub fn open_ask_modal(&self, question: &str, options: Vec<String>) {
        {
            let mut s = lock_screen(&self.screen.0);
            s.model.ask = Some(AskState {
                question: question.to_string(),
                options,
                selected: 0,
                typed: String::new(),
            });
        }
        let _ = self.screen.draw_now();
    }

    pub fn close_ask_modal(&self) {
        {
            let mut s = lock_screen(&self.screen.0);
            s.model.ask = None;
        }
        let _ = self.screen.draw_now();
    }
}

/// RAII marker telling the keyboard thread the agent is running.
pub struct BusyGuard {
    busy: Arc<std::sync::atomic::AtomicBool>,
}

impl Drop for BusyGuard {
    fn drop(&mut self) {
        self.busy.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Viewport height from the last draw; one page for PageUp/PageDown.
/// Suffix of `input` that fits `max` display columns - the editor scrolls horizontally,
/// keeping the caret (always at the end of the buffer) visible.
/// Byte index of the char boundary before `caret`, or 0.
fn prev_char_boundary(buf: &str, caret: usize) -> usize {
    buf[..caret.min(buf.len())]
        .char_indices()
        .next_back()
        .map(|(index, _)| index)
        .unwrap_or(0)
}

/// Byte index of the char boundary after `caret`, or the end.
fn next_char_boundary(buf: &str, caret: usize) -> usize {
    let caret = caret.min(buf.len());
    buf[caret..]
        .chars()
        .next()
        .map(|ch| caret + ch.len_utf8())
        .unwrap_or(buf.len())
}

/// Start of the word before `caret`: skip any run of spaces, then the run of non-spaces. This
/// is both where Alt+Left lands and what Ctrl+W deletes.
fn prev_word_boundary(buf: &str, caret: usize) -> usize {
    let mut index = caret.min(buf.len());
    while index > 0 {
        let previous = prev_char_boundary(buf, index);
        if buf[previous..index].chars().all(char::is_whitespace) {
            index = previous;
        } else {
            break;
        }
    }
    while index > 0 {
        let previous = prev_char_boundary(buf, index);
        if buf[previous..index].chars().any(char::is_whitespace) {
            break;
        }
        index = previous;
    }
    index
}

/// End of the word after `caret`, mirroring `prev_word_boundary`.
fn next_word_boundary(buf: &str, caret: usize) -> usize {
    let mut index = caret.min(buf.len());
    while index < buf.len() {
        let next = next_char_boundary(buf, index);
        if buf[index..next].chars().all(char::is_whitespace) {
            index = next;
        } else {
            break;
        }
    }
    while index < buf.len() {
        let next = next_char_boundary(buf, index);
        if buf[index..next].chars().any(char::is_whitespace) {
            break;
        }
        index = next;
    }
    index
}

/// Start of the buffer line holding `caret` (Home).
fn line_start(buf: &str, caret: usize) -> usize {
    buf[..caret.min(buf.len())]
        .rfind('\n')
        .map(|index| index + 1)
        .unwrap_or(0)
}

/// End of the buffer line holding `caret` (End).
fn line_end(buf: &str, caret: usize) -> usize {
    let caret = caret.min(buf.len());
    buf[caret..]
        .find('\n')
        .map(|offset| caret + offset)
        .unwrap_or(buf.len())
}

fn line_display_col(buf: &str, caret: usize) -> usize {
    let start = line_start(buf, caret);
    buf[start..caret.min(buf.len())].chars().count()
}

fn offset_at_display_col(line: &str, col: usize) -> usize {
    let mut remaining = col;
    for (index, _) in line.char_indices() {
        if remaining == 0 {
            return index;
        }
        remaining -= 1;
    }
    line.len()
}

/// Previous buffer line, same display column. `None` on the first line (history takes over).
fn line_up(buf: &str, caret: usize) -> Option<usize> {
    let start = line_start(buf, caret);
    if start == 0 {
        return None;
    }
    let col = line_display_col(buf, caret);
    let prev_end = start - 1;
    let prev_start = line_start(buf, prev_end);
    let prev_line = &buf[prev_start..prev_end];
    Some(prev_start + offset_at_display_col(prev_line, col))
}

/// Next buffer line, same display column. `None` on the last line (history takes over).
fn line_down(buf: &str, caret: usize) -> Option<usize> {
    let end = line_end(buf, caret);
    if end >= buf.len() {
        return None;
    }
    let col = line_display_col(buf, caret);
    let next_start = end + 1;
    let next_end = line_end(buf, next_start);
    let next_line = &buf[next_start..next_end];
    Some(next_start + offset_at_display_col(next_line, col))
}

fn page_rows(screen: &ScreenHandle) -> i64 {
    let s = lock_screen(&screen.0);
    s.last_viewport_rows.max(1) as i64
}

fn scroll_screen(screen: &ScreenHandle, rows: i64) {
    let mut s = lock_screen(&screen.0);
    let next = s.model.scroll_from_bottom as i64 + rows;
    s.model.scroll_from_bottom = next.clamp(0, 100_000) as usize;
    let _ = s.draw();
}

/// Centered modal with border, opencode PlaceOverlay style.
/// Rows the picker shows at once; arrows scroll within this window.
const DIALOG_VISIBLE: usize = 8;

fn render_dialog(frame: &mut Frame<'_>, dialog: &DialogState, area: Rect) {
    let filtered = dialog.filtered();
    let selected = dialog.selected.min(filtered.len().saturating_sub(1));
    let width = 56.min(area.width.max(1));
    // Height follows what the list can actually show, not how many entries exist: sizing it to
    // `filtered.len()` grew the box to the full screen while only a window of rows was ever
    // drawn, leaving a session picker that was mostly empty space.
    let wanted = filtered.len().clamp(1, DIALOG_VISIBLE);
    let height = ((wanted + 4) as u16).min(area.height.max(1));
    let capacity = (height as usize).saturating_sub(4).max(1);
    // Keep the selection centred in the window instead of pinned to its last row.
    let start = if filtered.len() <= capacity {
        0
    } else {
        selected
            .saturating_sub(capacity / 2)
            .min(filtered.len() - capacity)
    };
    let end = filtered.len().min(start + capacity);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let box_area = Rect {
        x,
        y,
        width,
        height,
    };

    frame.render_widget(Clear, box_area);
    let header = if dialog.multi_toggle {
        Line::from(vec![
            Span::styled(
                "\u{276f} ".to_string(),
                Style::default().fg(BLUE()).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "Space toggles · Enter applies · Esc closes".to_string(),
                Style::default().fg(MUTED()),
            ),
        ])
    } else {
        Line::from(vec![
            Span::styled(
                "\u{276f} ".to_string(),
                Style::default().fg(BLUE()).add_modifier(Modifier::BOLD),
            ),
            Span::styled(dialog.query.clone(), Style::default().fg(TEXT())),
            Span::styled(
                if filtered.is_empty() {
                    "  Esc closes".to_string()
                } else {
                    format!("  {}/{} \u{b7} Esc closes", selected + 1, filtered.len())
                },
                Style::default().fg(BORDER()),
            ),
        ])
    };
    let mut lines = vec![header, Line::from("")];
    if filtered.is_empty() {
        lines.push(Line::styled(
            "(no match)".to_string(),
            Style::default().fg(MUTED()),
        ));
    }
    for (idx, (value, label)) in filtered.iter().enumerate().take(end).skip(start) {
        let is_on = idx == selected;
        let prefix = if is_on { "\u{276f} " } else { "  " };
        // The mark column maps a filtered row back to its item index via the value/label
        // match; toggle dialogs never filter, so idx is the item index.
        let mark = if dialog.multi_toggle {
            let item_idx = dialog
                .items
                .iter()
                .position(|(v, l)| v == value && l == label)
                .unwrap_or(idx);
            let on = dialog.marks.get(item_idx).copied().unwrap_or(false);
            Span::styled(
                if on { "[x] " } else { "[ ] " }.to_string(),
                Style::default().fg(if on { GREEN() } else { BORDER() }),
            )
        } else {
            Span::raw("")
        };
        let mut row = vec![
            Span::styled(
                prefix.to_string(),
                Style::default().fg(if is_on { BLUE() } else { BORDER() }),
            ),
            mark,
            Span::styled(
                label.clone(),
                Style::default()
                    .fg(if is_on { TEXT() } else { MUTED() })
                    .add_modifier(if is_on {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            ),
        ];
        // Show the raw value only when the label doesn't already contain it.
        if !dialog.multi_toggle && !label.contains(value.as_str()) {
            row.push(Span::raw("  "));
            row.push(Span::styled(value.clone(), Style::default().fg(BORDER())));
        }
        lines.push(Line::from(row));
    }
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .style(Style::default().bg(POPUP_BG()))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(BLUE()))
                    .title(format!(" {} ", dialog.title))
                    .title_style(Style::default().fg(BLUE()).add_modifier(Modifier::BOLD)),
            ),
        box_area,
    );
}

/// Approval modal: preview body plus the three opencode options.
fn render_permission(frame: &mut Frame<'_>, perm: &PermissionState, area: Rect) {
    let width = 64.min(area.width.max(1));
    let all_rows: Vec<String> = wrap_display(&perm.body, width.saturating_sub(6) as usize);
    // Everything the box spends on chrome: blank row, options, the scroll note, the key hint.
    let chrome = PERM_OPTIONS.len() + 5;
    let ceiling = area.height.max(1) as usize;
    let capacity = ceiling.saturating_sub(chrome).max(1);
    let scroll = perm.scroll.min(all_rows.len().saturating_sub(capacity));
    let body_rows: Vec<String> = all_rows
        .iter()
        .skip(scroll)
        .take(capacity)
        .cloned()
        .collect();
    let height = ((body_rows.len() + chrome) as u16).min(area.height.max(1));
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let box_area = Rect {
        x,
        y,
        width,
        height,
    };

    frame.render_widget(Clear, box_area);
    let mut lines = Vec::new();
    for row in &body_rows {
        let trimmed = row.trim_start();
        let style = if trimmed.starts_with("- ") {
            Style::default().fg(RED())
        } else if trimmed.starts_with("+ ") {
            Style::default().fg(GREEN())
        } else if trimmed.starts_with("--- ") {
            Style::default().fg(BLUE()).add_modifier(Modifier::BOLD)
        } else if trimmed.starts_with("…") {
            Style::default().fg(MUTED()).add_modifier(Modifier::DIM)
        } else {
            Style::default().fg(TEXT())
        };
        lines.push(Line::styled(row.clone(), style));
    }
    if all_rows.len() > body_rows.len() {
        lines.push(Line::styled(
            format!(
                "\u{2026} showing {}-{} of {} lines \u{b7} PgUp/PgDn",
                scroll + 1,
                scroll + body_rows.len(),
                all_rows.len()
            ),
            Style::default().fg(WARN()),
        ));
    }
    lines.push(Line::from(""));
    for (idx, (hotkey, label)) in perm.options.iter().enumerate() {
        let is_on = idx == perm.selected;
        let prefix = if is_on { "\u{276f} " } else { "  " };
        lines.push(Line::from(vec![
            Span::styled(
                prefix.to_string(),
                Style::default().fg(if is_on { BLUE() } else { BORDER() }),
            ),
            Span::styled(
                format!("{hotkey}  "),
                Style::default()
                    .fg(if is_on { BLUE() } else { MUTED() })
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                (*label).to_string(),
                Style::default()
                    .fg(match (idx, is_on) {
                        (2, true) => RED(),
                        (2, false) => MUTED(),
                        (_, true) => TEXT(),
                        _ => MUTED(),
                    })
                    .add_modifier(if is_on {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            ),
        ]));
    }
    lines.push(Line::from(Span::styled(
        "y / a / n  \u{b7}  Enter confirm  \u{b7}  Esc rejects".to_string(),
        Style::default().fg(MUTED()),
    )));
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .style(Style::default().bg(POPUP_BG()))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(WARN()))
                    .title(format!(" {} ", perm.title))
                    .title_style(Style::default().fg(WARN()).add_modifier(Modifier::BOLD)),
            ),
        box_area,
    );
}

fn render_ask(frame: &mut Frame<'_>, ask: &AskState, area: Rect) {
    let width = 64.min(area.width.saturating_sub(4));
    let question_rows = wrap_display(&ask.question, width.saturating_sub(6) as usize);
    let option_rows = ask.options.len();
    let chrome = 5;
    let height = ((question_rows.len() + option_rows + chrome) as u16)
        .min(area.height.saturating_sub(2))
        .max(7);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let box_area = Rect {
        x,
        y,
        width,
        height,
    };
    frame.render_widget(Clear, box_area);
    let mut lines: Vec<Line<'static>> = question_rows
        .into_iter()
        .map(|row| Line::styled(row, Style::default().fg(TEXT())))
        .collect();
    lines.push(Line::from(""));
    for (idx, option) in ask.options.iter().enumerate() {
        let is_on = idx == ask.selected && ask.typed.is_empty();
        let prefix = if is_on { "\u{276f} " } else { "  " };
        lines.push(Line::from(vec![
            Span::styled(
                prefix.to_string(),
                Style::default().fg(if is_on { BLUE() } else { BORDER() }),
            ),
            Span::styled(
                format!("{}  ", idx + 1),
                Style::default()
                    .fg(if is_on { BLUE() } else { MUTED() })
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                option.clone(),
                Style::default()
                    .fg(if is_on { TEXT() } else { MUTED() })
                    .add_modifier(if is_on {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            ),
        ]));
    }
    let prompt = if ask.typed.is_empty() && !ask.options.is_empty() {
        "type to answer freely".to_string()
    } else if ask.typed.is_empty() {
        "type an answer".to_string()
    } else {
        ask.typed.clone()
    };
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled(
            "\u{276f} ".to_string(),
            Style::default().fg(BLUE()).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            prompt,
            if ask.typed.is_empty() {
                Style::default().fg(MUTED()).add_modifier(Modifier::DIM)
            } else {
                Style::default().fg(TEXT())
            },
        ),
    ]));
    let hint = if ask.options.is_empty() {
        "Enter send  \u{b7}  Esc skip"
    } else {
        "1-4 pick  \u{b7}  Enter  \u{b7}  Esc skip"
    };
    lines.push(Line::from(Span::styled(
        hint.to_string(),
        Style::default().fg(MUTED()),
    )));
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .style(Style::default().bg(POPUP_BG()))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(BLUE()))
                    .title(" Ask ")
                    .title_style(Style::default().fg(BLUE()).add_modifier(Modifier::BOLD)),
            ),
        box_area,
    );
}

/// `?` overlay: the keybinding sheet.
fn render_help(frame: &mut Frame<'_>, area: Rect, scroll: usize) {
    let width = 64.min(area.width.saturating_sub(4));
    let rows: [(&str, &str); 22] = [
        (
            "Enter",
            "send message (accept slash completion, when the menu is open)",
        ),
        ("Shift/Ctrl+Enter", "newline without sending"),
        ("\u{2190}/\u{2192}", "move the caret"),
        ("Alt+\u{2190}/\u{2192}", "move by word"),
        ("Home/End, Ctrl+A/E", "start / end of line"),
        ("Ctrl+K", "switch model"),
        ("Ctrl+S", "resume a session"),
        ("Ctrl+O / click", "expand or fold tool output"),
        ("Ctrl+F", "search the transcript"),
        ("Ctrl+B", "show or hide the sidebar"),
        ("Ctrl+Y", "copy the latest answer"),
        ("Right click", "copy the cell under the pointer"),
        ("?", "toggle this help"),
        ("Tab / Shift+Tab", "cycle mode (build / auto / plan)"),
        ("Tab", "accept slash completion without sending"),
        (
            "\u{2191}/\u{2193}",
            "line in the editor; history on first/last line",
        ),
        ("PgUp/PgDn", "scroll transcript"),
        ("Ctrl+Home/End", "jump to top/bottom"),
        ("!<command>", "run a shell command"),
        ("/warnings", "hide or show warnings"),
        ("Esc", "interrupt the agent"),
        ("Ctrl+C x 2", "quit"),
    ];
    // Chrome the sheet always pays for: two borders, the title, and the closing hint.
    const HELP_CHROME: usize = 4;
    let ceiling = (area.height.saturating_sub(2).max(8) as usize).saturating_sub(HELP_CHROME);
    let capacity = ceiling.clamp(1, rows.len());
    // A terminal too short for the whole sheet scrolls it; it used to clip the tail with
    // nothing on screen saying bindings were missing.
    let scroll = scroll.min(rows.len() - capacity);
    let height = (capacity + HELP_CHROME) as u16;
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let box_area = Rect {
        x,
        y,
        width,
        height,
    };
    frame.render_widget(Clear, box_area);
    let mut lines = vec![Line::from(Span::styled(
        "Keybindings",
        Style::default().fg(BLUE()).add_modifier(Modifier::BOLD),
    ))];
    // One column width for every key, so the descriptions line up instead of stepping around
    // the longest binding.
    let key_width = rows
        .iter()
        .map(|(key, _)| UnicodeWidthStr::width(*key))
        .max()
        .unwrap_or(0);
    for (key, desc) in rows.iter().skip(scroll).take(capacity) {
        let pad = " ".repeat(key_width - UnicodeWidthStr::width(*key));
        lines.push(Line::from(vec![
            Span::styled(
                format!("  {key}{pad}  "),
                Style::default().fg(TEXT()).add_modifier(Modifier::BOLD),
            ),
            Span::styled((*desc).to_string(), Style::default().fg(MUTED())),
        ]));
    }
    let hidden = rows.len() - capacity;
    lines.push(Line::from(Span::styled(
        if hidden == 0 {
            "  Esc closes".to_string()
        } else {
            format!(
                "  \u{2191}/\u{2193} scroll \u{b7} {}-{} of {} \u{b7} Esc closes",
                scroll + 1,
                scroll + capacity,
                rows.len()
            )
        },
        Style::default().fg(BORDER()),
    )));
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .style(Style::default().bg(POPUP_BG()))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(BORDER())),
            ),
        box_area,
    );
}

/// The single keyboard reader for TUI mode.
#[allow(clippy::too_many_arguments)]
fn input_thread(
    screen: ScreenHandle,
    tx: tokio::sync::mpsc::UnboundedSender<HubEvent>,
    interrupt: Arc<tokio::sync::Notify>,
    busy: Arc<std::sync::atomic::AtomicBool>,
    queue: Arc<Mutex<VecDeque<String>>>,
    requester: Arc<Mutex<Option<tokio::sync::oneshot::Sender<String>>>>,
    candidates: Arc<std::sync::RwLock<Vec<crate::tui::Candidate>>>,
    models_src: Arc<std::sync::RwLock<Vec<(String, String)>>>,
    sessions_src: Arc<std::sync::RwLock<Vec<(String, String)>>>,
    path_candidates: Arc<std::sync::RwLock<Vec<String>>>,
) {
    let mut buf = String::new();
    // Caret as a byte offset into `buf`. Editing used to be append-and-backspace only: a typo
    // near the start of a long prompt meant deleting everything back to it.
    let mut caret = 0usize;
    let mut selected = 0usize;
    let mut history: Vec<String> = Vec::new();
    let mut history_idx = 0usize;
    let mut saved_buf = String::new();
    let mut last_ctrl_c: Option<std::time::Instant> = None;

    let sync = |screen: &ScreenHandle,
                buf: &str,
                caret: usize,
                selected: usize,
                items: Vec<(String, String)>| {
        let mut s = lock_screen(&screen.0);
        s.model.input = buf.to_string();
        s.model.input_caret = caret.min(buf.len());
        s.model.ac_selected = selected;
        s.model.ac_items = items;
        let snapshot = path_candidates
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let labels = crate::context::attachment_indicators(buf, &snapshot);
        s.model.footer = if labels.is_empty() {
            String::new()
        } else {
            format!("attachments: {}", labels.join(" "))
        };
        let _ = s.draw();
    };
    let items_for = |needle: &str| -> Vec<(String, String)> {
        candidates
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|c| c.name.to_ascii_lowercase().starts_with(needle))
            .map(|c| (c.name.clone(), c.description.clone()))
            .collect()
    };
    let path_items_for = |needle: &str| -> Vec<(String, String)> {
        path_candidates
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|path| {
                path.trim_start_matches('@')
                    .to_ascii_lowercase()
                    .starts_with(needle)
            })
            .map(|path| {
                let description = match path.trim_start_matches('@') {
                    "clipboard" => "clipboard",
                    "diff" => "diff",
                    "staged" => "staged",
                    value if crate::context::is_image_reference(value) => "image",
                    _ if path.ends_with('/') => "directory",
                    _ => "file",
                };
                (
                    path.trim_start_matches('@').to_string(),
                    description.to_string(),
                )
            })
            .collect()
    };

    sync(&screen, "", 0, 0, Vec::new());
    // Feed loop: the wheel scrolls right here; only key presses fall through to the editor.
    // Read errors are tolerated briefly, then quit gracefully (never process::exit - that
    // would skip FullScreen's Drop and leave raw mode + mouse capture enabled).
    let mut feed_errors = 0u32;
    'keys: loop {
        let key = 'feed: {
            match event::read() {
                Ok(Event::Key(key)) if key.kind == KeyEventKind::Press => break 'feed Some(key),
                // A paste is one event carrying its whole payload, so newlines inside it stay
                // in the buffer instead of each submitting a separate message.
                Ok(Event::Paste(pasted)) => {
                    let cleaned = pasted.replace("\r\n", "\n").replace('\r', "\n");
                    if !cleaned.is_empty() {
                        buf.insert_str(caret, &cleaned);
                        caret += cleaned.len();
                        selected = 0;
                        if history_idx == history.len() {
                            saved_buf = buf.clone();
                        }
                        let is_slash =
                            buf.trim_start().starts_with('/') && !buf.trim_start().contains(' ');
                        let at = crate::context::active_at_reference(&buf, caret);
                        let items = if let Some(reference) = at {
                            path_items_for(&reference.query)
                        } else if is_slash {
                            items_for(&needle_of(&buf))
                        } else {
                            Vec::new()
                        };
                        sync(&screen, &buf, caret, selected, items);
                    }
                }
                Ok(Event::Resize(_, _)) => {
                    let _ = screen.draw_now();
                }
                Ok(Event::Mouse(mouse)) => match mouse.kind {
                    MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                        let delta = if matches!(mouse.kind, MouseEventKind::ScrollUp) {
                            -1
                        } else {
                            1
                        };
                        let mut s = lock_screen(&screen.0);
                        if let Some(perm) = s.model.permission.as_mut() {
                            if delta < 0 {
                                perm.scroll = perm.scroll.saturating_sub(3);
                            } else {
                                perm.scroll += 3;
                            }
                        } else if s.model.ask.is_some() {
                            // The ask panel has no scrollable body; consume the wheel so it cannot
                            // move the transcript behind the modal.
                        } else if s.model.help_visible {
                            if delta < 0 {
                                s.model.help_scroll = s.model.help_scroll.saturating_sub(1);
                            } else {
                                s.model.help_scroll += 1;
                            }
                        } else if let Some(dialog) = s.model.dialog.as_mut() {
                            let total = dialog.filtered().len();
                            if total > 0 {
                                dialog.selected = if delta < 0 {
                                    dialog.selected.saturating_sub(1)
                                } else {
                                    (dialog.selected + 1).min(total - 1)
                                };
                            }
                        } else if !s.model.ac_items.is_empty() {
                            let total = s.model.ac_items.len();
                            selected = if delta < 0 {
                                selected.saturating_sub(1)
                            } else {
                                (selected + 1).min(total - 1)
                            };
                            s.model.ac_selected = selected;
                        } else {
                            drop(s);
                            scroll_screen(&screen, if delta < 0 { 3 } else { -3 });
                            continue 'keys;
                        }
                        let _ = s.draw();
                    }
                    MouseEventKind::Down(MouseButton::Left) => {
                        let target = {
                            let s = lock_screen(&screen.0);
                            hit_at(&s.last_hits, mouse.column, mouse.row)
                        };
                        match target {
                            Some(HitTarget::Card(_)) => {
                                let _ = lock_screen(&screen.0).toggle_card_at_row(mouse.row);
                            }
                            Some(HitTarget::Autocomplete(index)) => {
                                let choice =
                                    lock_screen(&screen.0).model.ac_items.get(index).cloned();
                                if let Some((value, _)) = choice {
                                    if let Some(reference) =
                                        crate::context::active_at_reference(&buf, caret)
                                    {
                                        let replacement = format!("@{value}");
                                        buf.replace_range(
                                            reference.replacement.clone(),
                                            &replacement,
                                        );
                                        caret = reference.replacement.start + replacement.len();
                                    } else {
                                        buf = format!("/{value} ");
                                        caret = buf.len();
                                    }
                                    selected = 0;
                                    sync(&screen, &buf, caret, selected, Vec::new());
                                }
                            }
                            Some(HitTarget::Dialog(index)) => {
                                let multi = lock_screen(&screen.0)
                                    .model
                                    .dialog
                                    .as_ref()
                                    .is_some_and(|d| d.multi_toggle);
                                if multi {
                                    let mut s = lock_screen(&screen.0);
                                    if let Some(dialog) = s.model.dialog.as_mut()
                                        && let Some(mark) = dialog.marks.get_mut(index)
                                    {
                                        *mark = !*mark;
                                        drop(s);
                                        let _ = screen.draw_now();
                                    }
                                    continue 'keys;
                                }
                                let line = {
                                    let mut s = lock_screen(&screen.0);
                                    let picked = s.model.dialog.as_ref().and_then(|dialog| {
                                        dialog
                                            .filtered()
                                            .get(index)
                                            .map(|(value, _)| format!("{}{}", dialog.prefix, value))
                                    });
                                    if picked.is_some() {
                                        s.model.dialog = None;
                                    }
                                    picked
                                };
                                if let Some(line) = line {
                                    submit_line(
                                        &screen, &tx, &requester, &busy, &interrupt, &queue, line,
                                    );
                                }
                            }
                            Some(HitTarget::Permission(index)) => {
                                let answer = lock_screen(&screen.0)
                                    .model
                                    .permission
                                    .as_ref()
                                    .and_then(|p| p.options.get(index))
                                    .map(|(key, _)| (*key).to_string())
                                    .unwrap_or_else(|| "n".to_string());
                                lock_screen(&screen.0).model.permission = None;
                                if let Some(answer_tx) = requester
                                    .lock()
                                    .unwrap_or_else(PoisonError::into_inner)
                                    .take()
                                {
                                    let _ = answer_tx.send(answer);
                                }
                            }
                            Some(HitTarget::Ask(index)) => {
                                lock_screen(&screen.0).model.ask = None;
                                if let Some(answer_tx) = requester
                                    .lock()
                                    .unwrap_or_else(PoisonError::into_inner)
                                    .take()
                                {
                                    let _ = answer_tx.send((index + 1).to_string());
                                }
                            }
                            Some(HitTarget::Sidebar(SidebarAction::Model)) => {
                                let items = models_src
                                    .read()
                                    .unwrap_or_else(PoisonError::into_inner)
                                    .clone();
                                if !items.is_empty() {
                                    let mut s = lock_screen(&screen.0);
                                    s.model.dialog =
                                        Some(DialogState::new("Select Model", "/model ", items));
                                    drop(s);
                                    let _ = screen.draw_now();
                                }
                            }
                            Some(HitTarget::Sidebar(SidebarAction::Session)) => {
                                let items = sessions_src
                                    .read()
                                    .unwrap_or_else(PoisonError::into_inner)
                                    .clone();
                                if !items.is_empty() {
                                    let mut s = lock_screen(&screen.0);
                                    s.model.dialog =
                                        Some(DialogState::new("Resume Session", "/resume ", items));
                                    drop(s);
                                    let _ = screen.draw_now();
                                }
                            }
                            Some(HitTarget::Sidebar(SidebarAction::Mode)) => submit_line(
                                &screen,
                                &tx,
                                &requester,
                                &busy,
                                &interrupt,
                                &queue,
                                "/mode next".into(),
                            ),
                            Some(HitTarget::Footer(FooterAction::Help)) => {
                                let mut s = lock_screen(&screen.0);
                                s.model.help_visible = true;
                                s.model.help_scroll = 0;
                                drop(s);
                                let _ = screen.draw_now();
                            }
                            Some(HitTarget::Footer(FooterAction::Models)) => {
                                let items = models_src
                                    .read()
                                    .unwrap_or_else(PoisonError::into_inner)
                                    .clone();
                                if !items.is_empty() {
                                    let mut s = lock_screen(&screen.0);
                                    s.model.dialog =
                                        Some(DialogState::new("Select Model", "/model ", items));
                                    drop(s);
                                    let _ = screen.draw_now();
                                }
                            }
                            Some(HitTarget::Footer(FooterAction::Sessions)) => {
                                let items = sessions_src
                                    .read()
                                    .unwrap_or_else(PoisonError::into_inner)
                                    .clone();
                                if !items.is_empty() {
                                    let mut s = lock_screen(&screen.0);
                                    s.model.dialog =
                                        Some(DialogState::new("Resume Session", "/resume ", items));
                                    drop(s);
                                    let _ = screen.draw_now();
                                }
                            }
                            Some(HitTarget::Footer(FooterAction::Interrupt)) => {
                                if busy.load(std::sync::atomic::Ordering::SeqCst) {
                                    interrupt.notify_one();
                                }
                            }
                            Some(HitTarget::Footer(FooterAction::Live)) => {
                                let mut s = lock_screen(&screen.0);
                                s.model.scroll_from_bottom = 0;
                                drop(s);
                                let _ = screen.draw_now();
                            }
                            Some(HitTarget::Overlay) | None => {}
                        }
                    }
                    MouseEventKind::Down(MouseButton::Right) => {
                        let is_card = {
                            let s = lock_screen(&screen.0);
                            matches!(
                                hit_at(&s.last_hits, mouse.column, mouse.row),
                                Some(HitTarget::Card(_))
                            )
                        };
                        if is_card {
                            let _ = lock_screen(&screen.0).copy_card_at_row(mouse.row);
                        }
                    }
                    MouseEventKind::Moved => {
                        let mut s = lock_screen(&screen.0);
                        let next = hit_at(&s.last_hits, mouse.column, mouse.row);
                        if s.model.hovered != next {
                            s.model.hovered = next;
                            let _ = s.draw();
                        }
                    }
                    _ => {}
                },
                Ok(_) => {}
                Err(_) => {
                    feed_errors += 1;
                    if feed_errors >= 5 {
                        let _ = tx.send(HubEvent::Quit);
                        break 'feed None;
                    }
                }
            }
            continue 'keys;
        };
        let Some(key) = key else { break };
        feed_errors = 0;

        // --- Approval modal owns the keys while open ---
        {
            let mut s = lock_screen(&screen.0);
            if let Some(perm) = s.model.permission.as_mut() {
                match key.code {
                    KeyCode::Up => {
                        perm.selected = perm
                            .selected
                            .checked_sub(1)
                            .unwrap_or(perm.options.len().saturating_sub(1));
                    }
                    KeyCode::Down => {
                        perm.selected = (perm.selected + 1) % perm.options.len().max(1);
                    }
                    // Up/Down belong to the options, so the body pages instead.
                    KeyCode::PageUp => {
                        perm.scroll = perm.scroll.saturating_sub(5);
                    }
                    KeyCode::PageDown => {
                        perm.scroll += 5;
                    }
                    KeyCode::Enter => {
                        let answer = perm.options[perm.selected.min(perm.options.len() - 1)]
                            .0
                            .to_string();
                        s.model.permission = None;
                        drop(s);
                        if let Some(tx) = requester
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .take()
                        {
                            let _ = tx.send(answer);
                        }
                        continue;
                    }
                    KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                        if let Some(answer) = permission_hotkey(&perm.options, c) {
                            s.model.permission = None;
                            drop(s);
                            if let Some(tx) = requester
                                .lock()
                                .unwrap_or_else(PoisonError::into_inner)
                                .take()
                            {
                                let _ = tx.send(answer.to_string());
                            }
                            continue;
                        }
                    }
                    KeyCode::Esc => {
                        s.model.permission = None;
                        drop(s);
                        if let Some(tx) = requester
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .take()
                        {
                            let _ = tx.send("n".to_string());
                        }
                        continue;
                    }
                    _ => {}
                }
                drop(s);
                let _ = screen.draw_now();
                continue;
            }
            drop(s);
        }

        // --- ask_user modal owns the keys while open ---
        {
            let mut s = lock_screen(&screen.0);
            if let Some(ask) = s.model.ask.as_mut() {
                match key.code {
                    KeyCode::Up if ask.options.len() > 1 && ask.typed.is_empty() => {
                        ask.selected = ask.selected.checked_sub(1).unwrap_or(ask.options.len() - 1);
                    }
                    KeyCode::Down if ask.options.len() > 1 && ask.typed.is_empty() => {
                        ask.selected = (ask.selected + 1) % ask.options.len();
                    }
                    KeyCode::Enter => {
                        let answer = if !ask.typed.is_empty() {
                            ask.typed.clone()
                        } else if !ask.options.is_empty() {
                            (ask.selected + 1).to_string()
                        } else {
                            String::new()
                        };
                        s.model.ask = None;
                        drop(s);
                        if let Some(tx) = requester
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .take()
                        {
                            let _ = tx.send(answer);
                        }
                        continue;
                    }
                    KeyCode::Esc => {
                        s.model.ask = None;
                        drop(s);
                        if let Some(tx) = requester
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .take()
                        {
                            let _ = tx.send(String::new());
                        }
                        continue;
                    }
                    KeyCode::Backspace => {
                        ask.typed.pop();
                    }
                    KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                        if ask.typed.is_empty()
                            && let Some(digit) = c.to_digit(10)
                            && digit >= 1
                        {
                            let index = (digit as usize).saturating_sub(1);
                            if index < ask.options.len() {
                                s.model.ask = None;
                                drop(s);
                                if let Some(tx) = requester
                                    .lock()
                                    .unwrap_or_else(PoisonError::into_inner)
                                    .take()
                                {
                                    let _ = tx.send(digit.to_string());
                                }
                                continue;
                            }
                        }
                        ask.typed.push(c);
                    }
                    _ => {}
                }
                drop(s);
                let _ = screen.draw_now();
                continue;
            }
            drop(s);
        }

        // --- Modal overlays own the keys while open (opencode dialogs) ---
        {
            let mut s = lock_screen(&screen.0);
            if s.model.help_visible {
                match key.code {
                    KeyCode::Char('?') | KeyCode::Esc => {
                        s.model.help_visible = false;
                        s.model.help_scroll = 0;
                    }
                    KeyCode::Down | KeyCode::PageDown => {
                        s.model.help_scroll = s.model.help_scroll.saturating_add(1);
                    }
                    KeyCode::Up | KeyCode::PageUp => {
                        s.model.help_scroll = s.model.help_scroll.saturating_sub(1);
                    }
                    KeyCode::Home => s.model.help_scroll = 0,
                    _ => {}
                }
                drop(s);
                let _ = screen.draw_now();
                continue;
            }
            if let Some(dialog) = s.model.dialog.as_mut() {
                match key.code {
                    KeyCode::Up => {
                        let total = dialog.filtered().len();
                        if total > 0 {
                            dialog.selected = dialog.selected.checked_sub(1).unwrap_or(total - 1);
                        }
                    }
                    KeyCode::Down => {
                        let total = dialog.filtered().len();
                        if total > 0 {
                            dialog.selected = (dialog.selected + 1) % total;
                        }
                    }
                    KeyCode::Char(' ') if dialog.multi_toggle => {
                        if let Some(mark) = dialog.marks.get_mut(dialog.selected) {
                            *mark = !*mark;
                        }
                    }
                    KeyCode::Enter => {
                        if dialog.multi_toggle {
                            // Apply every mark as one queued command so the response is a single
                            // "config updated" line, not one notice per server.
                            let pairs: Vec<String> = dialog
                                .items
                                .iter()
                                .zip(dialog.marks.iter())
                                .map(|((value, _), &on)| {
                                    format!("{}:{}", value, if on { "on" } else { "off" })
                                })
                                .collect();
                            let line = format!("/mcp apply {}", pairs.join(" "));
                            s.model.dialog = None;
                            drop(s);
                            submit_line(&screen, &tx, &requester, &busy, &interrupt, &queue, line);
                            continue;
                        }
                        let picked = dialog
                            .filtered()
                            .get(dialog.selected)
                            .map(|(value, _)| (*value).clone());
                        if let Some(value) = picked {
                            let line = format!("{}{}", dialog.prefix, value);
                            s.model.dialog = None;
                            drop(s);
                            submit_line(&screen, &tx, &requester, &busy, &interrupt, &queue, line);
                            continue;
                        }
                    }
                    KeyCode::Esc => s.model.dialog = None,
                    KeyCode::Backspace => {
                        if !dialog.multi_toggle {
                            dialog.query.pop();
                            dialog.selected = 0;
                        }
                    }
                    KeyCode::Char(c)
                        if !dialog.multi_toggle
                            && !key.modifiers.contains(KeyModifiers::CONTROL) =>
                    {
                        dialog.query.push(c);
                        dialog.selected = 0;
                    }
                    _ => {}
                }
                drop(s);
                let _ = screen.draw_now();
                continue;
            }
            drop(s);
        }

        // Transcript search owns the keyboard while it is open, so its query cannot leak into
        // the editor buffer.
        let searching = lock_screen(&screen.0).model.search.is_some();
        if searching {
            let mut sc = lock_screen(&screen.0);
            match key.code {
                KeyCode::Esc => {
                    let _ = sc.close_search();
                }
                KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    let _ = sc.close_search();
                }
                KeyCode::Enter | KeyCode::Down => {
                    let _ = sc.step_search(1);
                }
                KeyCode::Up => {
                    let _ = sc.step_search(-1);
                }
                KeyCode::Backspace => {
                    let _ = sc.edit_search(|query| {
                        query.pop();
                    });
                }
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    let _ = sc.edit_search(|query| query.push(c));
                }
                _ => {}
            }
            continue;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('f') {
            let _ = lock_screen(&screen.0).open_search();
            continue;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('b') {
            let mut sc = lock_screen(&screen.0);
            sc.model.sidebar_hidden = !sc.model.sidebar_hidden;
            let _ = sc.draw();
            continue;
        }

        // Openers.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('k') {
            let items = models_src
                .read()
                .unwrap_or_else(PoisonError::into_inner)
                .clone();
            if !items.is_empty() {
                let mut sc = lock_screen(&screen.0);
                sc.model.dialog = Some(DialogState::new("Select Model", "/model ", items));
                drop(sc);
                let _ = screen.draw_now();
            }
            continue;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('s') {
            let items = sessions_src
                .read()
                .unwrap_or_else(PoisonError::into_inner)
                .clone();
            if !items.is_empty() {
                let mut sc = lock_screen(&screen.0);
                sc.model.dialog = Some(DialogState::new("Resume Session", "/resume ", items));
                drop(sc);
                let _ = screen.draw_now();
            }
            continue;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('o') {
            let _ = lock_screen(&screen.0).toggle_last_card();
            continue;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('y') {
            let _ = lock_screen(&screen.0).copy_latest();
            continue;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && key.modifiers.contains(KeyModifiers::SHIFT)
            && key.code == KeyCode::Char('v')
        {
            caret = insert_clipboard_reference(&mut buf, caret);
            selected = 0;
            if history_idx == history.len() {
                saved_buf = buf.clone();
            }
            sync(&screen, &buf, caret, selected, Vec::new());
            continue;
        }
        if key.code == KeyCode::Char('?') && buf.is_empty() {
            let mut sc = lock_screen(&screen.0);
            sc.model.help_visible = !sc.model.help_visible;
            sc.model.help_scroll = 0;
            drop(sc);
            let _ = screen.draw_now();
            continue;
        }

        let is_slash = buf.trim_start().starts_with('/') && !buf.trim_start().contains(' ');
        let active_at = crate::context::active_at_reference(&buf, caret);
        let needle = buf
            .trim_start()
            .trim_start_matches('/')
            .to_ascii_lowercase();
        let is_busy = busy.load(std::sync::atomic::Ordering::SeqCst);
        if !matches!(key.code, KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL))
        {
            last_ctrl_c = None;
        }
        match key.code {
            KeyCode::Up => {
                if let Some(reference) = active_at.as_ref() {
                    let total = path_items_for(&reference.query).len();
                    if total > 0 {
                        selected = selected.checked_sub(1).unwrap_or(total - 1);
                    }
                } else if is_slash {
                    let total = filtered_len(&candidates, &needle);
                    if total > 0 {
                        selected = selected.checked_sub(1).unwrap_or(total - 1);
                    }
                } else if let Some(next) = line_up(&buf, caret) {
                    caret = next;
                } else if history_idx > 0 {
                    if history_idx == history.len() {
                        saved_buf = buf.clone();
                    }
                    history_idx -= 1;
                    buf = history[history_idx].clone();
                    caret = buf.len();
                    selected = 0;
                }
            }
            KeyCode::Down => {
                if let Some(reference) = active_at.as_ref() {
                    let total = path_items_for(&reference.query).len();
                    if total > 0 {
                        selected = (selected + 1) % total;
                    }
                } else if is_slash {
                    let total = filtered_len(&candidates, &needle);
                    if total > 0 {
                        selected = (selected + 1) % total;
                    }
                } else if let Some(next) = line_down(&buf, caret) {
                    caret = next;
                } else if history_idx < history.len() {
                    history_idx += 1;
                    buf = if history_idx == history.len() {
                        saved_buf.clone()
                    } else {
                        history[history_idx].clone()
                    };
                    caret = buf.len();
                    selected = 0;
                }
            }
            KeyCode::Tab => {
                if let Some(reference) = active_at.as_ref()
                    && let Some(choice) = path_items_for(&reference.query).get(selected)
                {
                    let replacement = format!("@{}", choice.0);
                    buf.replace_range(reference.replacement.clone(), &replacement);
                    caret = reference.replacement.start + replacement.len();
                    selected = 0;
                } else if is_slash {
                    let all = items_for(&needle);
                    if let Some(choice) = all.get(selected) {
                        buf = format!("/{} ", choice.0);
                        caret = buf.len();
                        selected = 0;
                    }
                } else {
                    // opencode cycles agents with Tab. Completion keeps first claim on the key
                    // while the slash menu is open, so the two never compete for it.
                    submit_line(
                        &screen,
                        &tx,
                        &requester,
                        &busy,
                        &interrupt,
                        &queue,
                        "/mode next".to_string(),
                    );
                }
            }
            KeyCode::BackTab => {
                submit_line(
                    &screen,
                    &tx,
                    &requester,
                    &busy,
                    &interrupt,
                    &queue,
                    "/mode prev".to_string(),
                );
            }
            // Caret motion. Alt jumps by word, plain arrows by character.
            KeyCode::Left if key.modifiers.contains(KeyModifiers::ALT) => {
                caret = prev_word_boundary(&buf, caret);
            }
            KeyCode::Right if key.modifiers.contains(KeyModifiers::ALT) => {
                caret = next_word_boundary(&buf, caret);
            }
            KeyCode::Left => caret = prev_char_boundary(&buf, caret),
            KeyCode::Right => caret = next_char_boundary(&buf, caret),
            KeyCode::Delete => {
                let end = next_char_boundary(&buf, caret);
                if end > caret {
                    buf.replace_range(caret..end, "");
                    selected = 0;
                    if history_idx == history.len() {
                        saved_buf = buf.clone();
                    }
                }
            }
            // Ctrl+W deletes the word before the caret, as in a shell.
            KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                let start = prev_word_boundary(&buf, caret);
                if start < caret {
                    buf.replace_range(start..caret, "");
                    caret = start;
                    selected = 0;
                    if history_idx == history.len() {
                        saved_buf = buf.clone();
                    }
                }
            }
            KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                caret = line_start(&buf, caret);
            }
            KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                caret = line_end(&buf, caret);
            }
            // Newline without submitting. opencode binds shift+return / ctrl+return /
            // alt+return / ctrl+j for this; terminals disagree about which of those they
            // report, so accept all of them. The trailing-\\ form still works.
            KeyCode::Enter
                if key.modifiers.intersects(
                    KeyModifiers::SHIFT | KeyModifiers::ALT | KeyModifiers::CONTROL,
                ) =>
            {
                buf.insert(caret, '\n');
                caret += 1;
                selected = 0;
                if history_idx == history.len() {
                    saved_buf = buf.clone();
                }
            }
            KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                buf.insert(caret, '\n');
                caret += 1;
                selected = 0;
                if history_idx == history.len() {
                    saved_buf = buf.clone();
                }
            }
            KeyCode::Enter => {
                // opencode editor behavior: a backslash before the caret escapes the newline
                // and continues the message on the next line. Read at the caret rather than at
                // the end of the buffer, so it still works mid-line. Checked first so a
                // trailing backslash never triggers completion instead.
                let before = prev_char_boundary(&buf, caret);
                if before < caret && &buf[before..caret] == "\\" {
                    buf.replace_range(before..caret, "\n");
                    caret = before + 1;
                    selected = 0;
                    // Sync before looping: returning early used to leave the new line undrawn
                    // until the next keystroke.
                    sync(&screen, &buf, caret, selected, Vec::new());
                    continue 'keys;
                }
                if let Some(reference) = active_at.as_ref()
                    && let Some(choice) = path_items_for(&reference.query).get(selected)
                {
                    let replacement = format!("@{}", choice.0);
                    buf.replace_range(reference.replacement.clone(), &replacement);
                    caret = reference.replacement.start + replacement.len();
                    selected = 0;
                    sync(&screen, &buf, caret, selected, Vec::new());
                    continue 'keys;
                }
                // Slash menu open with a match: Enter accepts the highlighted
                // candidate and submits it, so `/mo` + Enter runs `/model`
                // without a Tab stop first. Exact buffer wins: typing the full
                // command still submits what was typed.
                let mut line = buf.trim().to_string();
                if is_slash {
                    let all = items_for(&needle);
                    if !all.is_empty()
                        && !all.iter().any(|(name, _)| line == format!("/{name}"))
                        && let Some(choice) = all.get(selected)
                    {
                        line = format!("/{} ", choice.0);
                    }
                }
                buf.clear();
                caret = 0;
                selected = 0;
                if !line.is_empty() {
                    history.push(line.clone());
                    if history.len() > 500 {
                        history.remove(0);
                    }
                    history_idx = history.len();
                }
                submit_line(&screen, &tx, &requester, &busy, &interrupt, &queue, line);
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                scroll_screen(&screen, -(page_rows(&screen) / 2));
            }
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                scroll_screen(&screen, page_rows(&screen) / 2);
            }
            KeyCode::Esc => {
                if is_busy {
                    interrupt.notify_one();
                    buf.clear();
                    caret = 0;
                    selected = 0;
                    let _ = lock_screen(&screen.0).add_notice("interrupt requested");
                } else {
                    buf.clear();
                    caret = 0;
                    selected = 0;
                }
            }
            KeyCode::PageUp => scroll_screen(&screen, page_rows(&screen)),
            KeyCode::PageDown => scroll_screen(&screen, -page_rows(&screen)),
            // Home/End move the caret, which is what they do in every other text field.
            // Jumping the transcript to top/bottom moved to Ctrl+Home / Ctrl+End.
            KeyCode::Home if key.modifiers.contains(KeyModifiers::CONTROL) => {
                scroll_screen(&screen, 100_000);
            }
            KeyCode::End if key.modifiers.contains(KeyModifiers::CONTROL) => {
                scroll_screen(&screen, -100_000);
            }
            KeyCode::Home => caret = line_start(&buf, caret),
            KeyCode::End => caret = line_end(&buf, caret),
            KeyCode::Backspace => {
                let start = prev_char_boundary(&buf, caret);
                if start < caret {
                    buf.replace_range(start..caret, "");
                    caret = start;
                    selected = 0;
                    if history_idx == history.len() {
                        saved_buf = buf.clone();
                    }
                }
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if is_busy {
                    interrupt.notify_one();
                } else if last_ctrl_c
                    .map(|t| t.elapsed() < std::time::Duration::from_secs(3))
                    .unwrap_or(false)
                {
                    let _ = tx.send(HubEvent::Quit);
                    return;
                } else {
                    last_ctrl_c = Some(std::time::Instant::now());
                    let _ =
                        lock_screen(&screen.0).add_notice("Press Ctrl+C again within 3s to quit.");
                }
            }
            KeyCode::Char(c) => {
                buf.insert(caret, c);
                caret += c.len_utf8();
                selected = 0;
                if history_idx == history.len() {
                    saved_buf = buf.clone();
                }
            }
            _ => {}
        }
        // Menu visibility follows the post-keystroke buffer: an empty or non-slash line
        // closes it (filtering with an empty needle would otherwise match every candidate
        // and leave the helper stuck open).
        let is_slash_now = buf.trim_start().starts_with('/') && !buf.trim_start().contains(' ');
        let (items, selected) =
            if let Some(reference) = crate::context::active_at_reference(&buf, caret) {
                (path_items_for(&reference.query), selected)
            } else if is_slash_now {
                (items_for(&needle_of(&buf)), selected)
            } else {
                (Vec::new(), 0)
            };
        sync(&screen, &buf, caret, selected, items);
    }
}

fn insert_clipboard_reference(buf: &mut String, caret: usize) -> usize {
    let prefix = if caret > 0
        && !buf[..caret]
            .chars()
            .next_back()
            .is_some_and(char::is_whitespace)
    {
        " "
    } else {
        ""
    };
    let suffix =
        if caret < buf.len() && !buf[caret..].chars().next().is_some_and(char::is_whitespace) {
            " "
        } else {
            ""
        };
    let insertion = format!("{prefix}@clipboard{suffix}");
    buf.insert_str(caret, &insertion);
    caret + prefix.len() + "@clipboard".len()
}

/// Shared submit path for the editor and modal dialogs: a waiting approval/ask_user takes
/// the answer, busy queues it, idle sends it straight to the chat loop.
fn submit_line(
    screen: &ScreenHandle,
    tx: &tokio::sync::mpsc::UnboundedSender<HubEvent>,
    requester: &Arc<Mutex<Option<tokio::sync::oneshot::Sender<String>>>>,
    busy: &Arc<std::sync::atomic::AtomicBool>,
    interrupt: &Arc<tokio::sync::Notify>,
    queue: &Arc<Mutex<VecDeque<String>>>,
    line: String,
) {
    if line.is_empty() {
        return;
    }
    let control = line.trim().to_ascii_lowercase();
    if busy.load(std::sync::atomic::Ordering::SeqCst) {
        if control == "stop" || control == "cancel" {
            // Busy controls are consumed here rather than becoming queued prompts.
            // The keyboard loop owns the interrupt notifier through `tx`'s companion path.
            interrupt.notify_one();
            return;
        }
        if control == "mode" || control == "mode?" || control == "status mode" {
            let mut s = lock_screen(&screen.0);
            let _ = s.add_notice("mode is controlled by the active chat turn".to_string());
            let _ = s.draw();
            return;
        }
    }
    let answer_tx = requester
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take();
    if let Some(tx) = answer_tx {
        let _ = tx.send(line);
    } else if busy.load(std::sync::atomic::Ordering::SeqCst) {
        queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push_back(line.clone());
        let mut s = lock_screen(&screen.0);
        s.model.queued_count = queue.lock().unwrap_or_else(PoisonError::into_inner).len();
        let _ = s.add_notice(format!("queued: {line}"));
        drop(s);
    } else {
        let _ = tx.send(HubEvent::Line(line));
    }
}

fn needle_of(buf: &str) -> String {
    buf.trim_start()
        .trim_start_matches('/')
        .to_ascii_lowercase()
}

fn filtered_len(candidates: &std::sync::RwLock<Vec<crate::tui::Candidate>>, needle: &str) -> usize {
    crate::tui::filter_candidates(
        &candidates.read().unwrap_or_else(PoisonError::into_inner),
        needle,
    )
    .len()
}

/// Blocking editor loop, opencode-style: keystrokes update `Model::input` and redraw through
/// ratatui, so typed text lives inside the bordered editor instead of a raw ANSI popup below
/// the frame. Runs on a dedicated thread (see `spawn_blocking` callers).
#[derive(Clone)]
pub struct ScreenHandle(Arc<Mutex<FullScreen>>);

impl ScreenHandle {
    /// Redraws the frame from current model state.
    pub fn draw_now(&self) -> Result<()> {
        lock_screen(&self.0).draw()
    }
}

/// What one draw learned about the frame: the transcript viewport height (PageUp/PageDown
/// page by it) and which card owns each drawn row (a click resolves through it).
#[derive(Default)]
struct RenderInfo {
    viewport_rows: usize,
    card_rows: Vec<(u16, u64)>,
    transcript_width: u16,
    hits: Vec<HitRegion>,
}

fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    }
}

fn dialog_geometry(dialog: &DialogState, area: Rect) -> (Rect, usize, usize) {
    let filtered = dialog.filtered();
    let selected = dialog.selected.min(filtered.len().saturating_sub(1));
    let width = 56.min(area.width.max(1));
    let wanted = filtered.len().clamp(1, DIALOG_VISIBLE);
    let height = ((wanted + 4) as u16).min(area.height.max(1));
    let capacity = (height as usize).saturating_sub(4).max(1);
    let start = if filtered.len() <= capacity {
        0
    } else {
        selected
            .saturating_sub(capacity / 2)
            .min(filtered.len() - capacity)
    };
    (centered_rect(area, width, height), start, capacity)
}

fn render(frame: &mut Frame<'_>, model: &Model) -> RenderInfo {
    ACTIVE_THEME.with(|c| *c.borrow_mut() = Some(model.theme.clone()));
    // Paint every terminal cell before laying out widgets. Windows Terminal/ConPTY can expose
    // newly reported rows or columns between draws; leaving any cell untouched reveals the
    // terminal's default black instead of Tokyo Night and looks like letterboxing.
    let whole = frame.area();
    frame
        .buffer_mut()
        .set_style(whole, Style::default().bg(BG_CHAT()));
    // OpenCode layout: transcript on top, autocomplete menu above the bordered editor, a
    // one-line footer, and the sidebar rail splitting the body horizontally.
    // The search bar and the slash menu never coexist: opening search closes the editor's menu.
    let popup_height = if model.search.is_some() {
        1
    } else {
        menu_height(model.ac_items.len())
    };
    // Multiline editor: grows with the buffer's newlines (backslash-newline continuation).
    // Split the same way `editor_widget` does: `lines()` drops a trailing empty segment, which
    // would leave the caret a row below the text after the buffer ends with a newline.
    // While thinking with an empty buffer the placeholder row is omitted, so count zero content
    // rows and let the wall occupy the only text line.
    let input_lines = if model.input.is_empty() && model.thinking.is_some() {
        0
    } else {
        model.input.split('\n').count().max(1)
    };
    let editor_rows =
        (input_lines.min(EDITOR_VISIBLE_LINES) as u16) + 2 + u16::from(model.thinking.is_some());
    let screen_rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(frame.area());
    let main_area = screen_rows[0];
    let footer_area = screen_rows[1];

    // Sidebar rail takes the right side once the terminal is wide enough — including the home
    // screen, so a new chat is not a logo floating in a void.
    let (left_area, sidebar_area) = match (&model.sidebar, model.sidebar_hidden) {
        (Some(entries), false) if !entries.is_empty() && main_area.width >= 68 => {
            let rail = if main_area.width >= 84 { 30 } else { 24 };
            let cols = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Min(0), Constraint::Length(rail)])
                .split(main_area);
            (cols[0], Some(cols[1]))
        }
        _ => (main_area, None),
    };

    let left_rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(popup_height),
            Constraint::Length(editor_rows),
        ])
        .split(left_area);
    // spacer row between body and input/popup
    frame
        .buffer_mut()
        .set_style(left_rows[1], Style::default().bg(BG_CHAT()));
    let transcript_area = left_rows[0];
    let popup_area = left_rows[2];
    let editor_area = left_rows[3];

    let mut card_rows: Vec<(u16, u64)> = Vec::new();
    let mut hit_regions = Vec::new();
    let home = model.intro
        && model
            .cards
            .iter()
            .all(|card| matches!(card.kind, CardKind::Note));
    if home {
        frame.render_widget(intro_paragraph(model, transcript_area), transcript_area);
    } else {
        // Wrap every row ourselves so viewport math is exact (Paragraph's own wrap happens
        // after scroll offsets, which makes bottom-follow drift on long wrapped lines).
        // Each source line carries its owning card, so wrapping cannot lose the attribution
        // a click needs.
        let rows = wrapped_transcript(model, transcript_area.width);
        let visible = transcript_area.height as usize;
        let start = rows
            .len()
            .saturating_sub(visible.saturating_add(model.scroll_from_bottom));
        let hits = model
            .search
            .as_ref()
            .map(|search| matching_rows(&rows, &search.query))
            .unwrap_or_default();
        let current_hit = model
            .search
            .as_ref()
            .filter(|_| !hits.is_empty())
            .map(|search| hits[search.current % hits.len()]);
        let shown: Vec<(Line<'static>, Option<u64>)> =
            rows.into_iter().skip(start).take(visible).collect();
        // Short transcripts pad at the top so messages sit just above the editor, matching
        // opencode. Pad rows are display-only: they are not click targets and they are not
        // injected into the wrap cache, so search indices stay aligned with wrapped rows.
        let pad = visible.saturating_sub(shown.len());
        let mut window: Vec<Line<'static>> = Vec::with_capacity(visible);
        for _ in 0..pad {
            window.push(Line::from(""));
        }
        for (offset, (line, owner)) in shown.into_iter().enumerate() {
            if let Some(id) = owner {
                let row = transcript_area.y + (pad + offset) as u16;
                card_rows.push((row, id));
                hit_regions.push(HitRegion {
                    area: Rect::new(transcript_area.x, row, transcript_area.width, 1),
                    target: HitTarget::Card(id),
                });
            }
            let absolute = start + offset;
            let line = if Some(absolute) == current_hit {
                highlight_row(line, MATCH_CURRENT_BG())
            } else if hits.binary_search(&absolute).is_ok() {
                highlight_row(line, MATCH_BG())
            } else {
                line
            };
            window.push(line);
        }
        frame.render_widget(
            Paragraph::new(Text::from(window)).style(Style::default().bg(BG_CHAT())),
            transcript_area,
        );
    }
    if let Some(area) = sidebar_area {
        frame.render_widget(sidebar_paragraph(model, area), area);
        // Sidebar actions use whole semantic rows, not individual glyph coordinates. The compact
        // rail keeps these rows stable enough to remain useful at narrow supported widths.
        if let Some(entries) = &model.sidebar {
            for (row, (key, _)) in entries.iter().enumerate() {
                let target = match key.as_str() {
                    "Session" => Some(SidebarAction::Session),
                    "Model" => Some(SidebarAction::Model),
                    "mode" | "Mode" => Some(SidebarAction::Mode),
                    _ => None,
                };
                if let Some(target) = target {
                    hit_regions.push(HitRegion {
                        area: Rect::new(area.x, area.y + row as u16, area.width, 1),
                        target: HitTarget::Sidebar(target),
                    });
                }
            }
        }
    }
    if let Some(search) = &model.search {
        frame.render_widget(search_widget(search), popup_area);
    } else if popup_height > 0 {
        frame.render_widget(popup_widget(model, popup_area), popup_area);
        let total = model.ac_items.len();
        let selected = model.ac_selected.min(total.saturating_sub(1));
        let start = if total <= MENU_VISIBLE {
            0
        } else {
            selected
                .saturating_sub(MENU_VISIBLE / 2)
                .min(total - MENU_VISIBLE)
        };
        for (row, index) in (start..total.min(start + MENU_VISIBLE)).enumerate() {
            hit_regions.push(HitRegion {
                area: Rect::new(
                    popup_area.x + 1,
                    popup_area.y + 2 + row as u16,
                    popup_area.width.saturating_sub(2),
                    1,
                ),
                target: HitTarget::Autocomplete(index),
            });
        }
    }
    frame.render_widget(editor_widget(model, editor_area), editor_area);

    // Terminal cursor sits at the end of the typed text whenever the editor owns input. This
    // includes the home screen: ratatui hides the cursor on any frame that sets no position,
    // so skipping it there left the first thing a user types with no visible caret.
    {
        let inner = editor_area.width.saturating_sub(4).max(1) as usize;
        let view = editor_view(&model.input, model.input_caret, inner);
        // The editor block draws a LEFT border: one column, no rows. Text then starts after the
        // two-cell row prefix, so the caret belongs at x + 3 on the block's own first row --
        // an earlier `y + 1` aimed at a top border that this block never draws.
        let row = editor_area.y + view.caret_row as u16;
        let col = editor_area.x + 3 + view.caret_col.min(inner) as u16;
        frame.set_cursor_position((
            col.min(editor_area.right().saturating_sub(1)),
            row.min(editor_area.bottom().saturating_sub(1)),
        ));
    }

    frame.render_widget(footer_widget(model, footer_area), footer_area);
    let footer_targets = footer_hit_regions(model, footer_area);
    hit_regions.extend(footer_targets);

    // Modal elevation: dim whatever is behind an overlay so the dialog reads as the front
    // surface rather than one more box on the same plane. Only the DIM modifier is patched in,
    // so every cell underneath keeps its own colours.
    if model.permission.is_some()
        || model.ask.is_some()
        || model.help_visible
        || model.dialog.is_some()
    {
        hit_regions.push(HitRegion {
            area: frame.area(),
            target: HitTarget::Overlay,
        });
        let whole = frame.area();
        frame
            .buffer_mut()
            .set_style(whole, Style::default().add_modifier(Modifier::DIM));
    }
    if let Some(perm) = &model.permission {
        render_permission(frame, perm, frame.area());
        let width = 64.min(frame.area().width.saturating_sub(4));
        let all_rows = wrap_display(&perm.body, width.saturating_sub(6) as usize);
        let chrome = perm.options.len() + 5;
        let capacity = (frame.area().height.saturating_sub(2).max(7) as usize)
            .saturating_sub(chrome)
            .max(1);
        let body_rows = all_rows.len().saturating_sub(perm.scroll).min(capacity);
        let height = ((body_rows + chrome) as u16)
            .min(frame.area().height.saturating_sub(2))
            .max(7);
        let area = centered_rect(frame.area(), width, height);
        let option_y = area.y + 1 + body_rows as u16 + u16::from(all_rows.len() > body_rows) + 1;
        for index in 0..perm.options.len() {
            hit_regions.push(HitRegion {
                area: Rect::new(
                    area.x + 1,
                    option_y + index as u16,
                    area.width.saturating_sub(2),
                    1,
                ),
                target: HitTarget::Permission(index),
            });
        }
    }
    if let Some(ask) = &model.ask {
        render_ask(frame, ask, frame.area());
        let width = 64.min(frame.area().width.saturating_sub(4));
        let question_rows = wrap_display(&ask.question, width.saturating_sub(6) as usize).len();
        let height = ((question_rows + ask.options.len() + 5) as u16)
            .min(frame.area().height.saturating_sub(2))
            .max(7);
        let area = centered_rect(frame.area(), width, height);
        let option_y = area.y + 1 + question_rows as u16 + 1;
        for index in 0..ask.options.len() {
            hit_regions.push(HitRegion {
                area: Rect::new(
                    area.x + 1,
                    option_y + index as u16,
                    area.width.saturating_sub(2),
                    1,
                ),
                target: HitTarget::Ask(index),
            });
        }
    }
    if model.help_visible {
        render_help(frame, frame.area(), model.help_scroll);
    }
    if let Some(dialog) = &model.dialog {
        render_dialog(frame, dialog, frame.area());
        let (area, start, capacity) = dialog_geometry(dialog, frame.area());
        let end = dialog.filtered().len().min(start + capacity);
        for (row, index) in (start..end).enumerate() {
            hit_regions.push(HitRegion {
                area: Rect::new(
                    area.x + 1,
                    area.y + 3 + row as u16,
                    area.width.saturating_sub(2),
                    1,
                ),
                target: HitTarget::Dialog(index),
            });
        }
    }

    // Hover is a quiet background wash. Apply it after normal widgets, but before modal-specific
    // rows are added above the catch-all overlay, preserving the same precedence as clicks.
    if let Some(hovered) = &model.hovered
        && let Some(hit) = hit_regions.iter().rev().find(|hit| &hit.target == hovered)
        && !matches!(hovered, HitTarget::Overlay)
    {
        frame
            .buffer_mut()
            .set_style(hit.area, Style::default().bg(MATCH_BG()));
    }

    RenderInfo {
        viewport_rows: if home {
            0
        } else {
            transcript_area.height as usize
        },
        card_rows,
        transcript_width: transcript_area.width,
        hits: hit_regions,
    }
}

fn footer_hit_regions(model: &Model, area: Rect) -> Vec<HitRegion> {
    let mut out = Vec::new();
    let mut x = area.x;
    let mut add = |label: &str, target: FooterAction| {
        let width = label.chars().count() as u16;
        if width > 0 && x < area.right() {
            let actual = width.min(area.right() - x);
            out.push(HitRegion {
                area: Rect::new(x, area.y, actual, 1),
                target: HitTarget::Footer(target),
            });
            x = x.saturating_add(width);
        }
    };
    add("? help", FooterAction::Help);
    if model.thinking.is_some() {
        add("  ·  Esc interrupts", FooterAction::Interrupt);
    }
    if model.scroll_from_bottom > 0 {
        add("  ·  Ctrl+End live", FooterAction::Live);
    }
    add("  ·  Ctrl+K", FooterAction::Models);
    add("  ·  Ctrl+S", FooterAction::Sessions);
    out
}

/// The newest card with rows hidden behind a fold -- the one Ctrl+O, `/expand`, and
/// `/collapse` all act on. Command output is a cell too now, so "the last card" is usually the
/// note holding that command's own output rather than the tool output worth unfolding.
fn last_foldable_index(cards: &[Card]) -> Option<usize> {
    cards.iter().rposition(|card| card.foldable_rows() > 0)
}

/// Puts text on the system clipboard. Kept behind one function so the failure mode -- a
/// headless session with no clipboard at all -- is reported once, as a notice, instead of
/// taking the UI down.
fn set_clipboard_text(text: &str) -> Result<()> {
    arboard::Clipboard::new()
        .context("could not access the system clipboard")?
        .set_text(text.to_string())
        .context("could not write to the system clipboard")
}

/// The editor's visible rows together with where the caret sits among them. Rows and caret come
/// from one function on purpose: derived separately they drift apart, which is how the caret
/// ended up pointing at a row the text was never drawn on.
struct EditorView {
    rows: Vec<String>,
    caret_row: usize,
    /// Display columns from the start of the row's text.
    caret_col: usize,
}

/// Lays out `input` for an editor `width` columns wide, scrolled so the caret is always on
/// screen both vertically (long multi-line buffers) and horizontally (long single lines).
fn editor_view(input: &str, caret: usize, width: usize) -> EditorView {
    let width = width.max(1);
    let caret = caret.min(input.len());
    let segments: Vec<&str> = input.split('\n').collect();

    // Which buffer line the caret is on, and how many chars into it.
    let start_of_line = line_start(input, caret);
    let caret_row_full = input[..start_of_line].matches('\n').count();
    let caret_chars = input[start_of_line..caret].chars().count();

    // Vertical viewport: the newest lines, extended back if the caret sits above them.
    let mut first = segments.len().saturating_sub(EDITOR_VISIBLE_LINES);
    first = first.min(caret_row_full);

    let mut rows = Vec::with_capacity(segments.len() - first);
    let mut caret_col = 0usize;
    for (offset, segment) in segments[first..].iter().enumerate() {
        if first + offset == caret_row_full {
            let (visible, column) = visible_around_caret(segment, caret_chars, width);
            caret_col = column;
            rows.push(visible);
        } else {
            rows.push(segment.chars().take(width).collect());
        }
    }
    EditorView {
        rows,
        caret_row: caret_row_full - first,
        caret_col,
    }
}

/// The slice of one buffer line that fits in `width` columns while keeping the caret visible,
/// plus the caret's column inside that slice.
fn visible_around_caret(segment: &str, caret_chars: usize, width: usize) -> (String, usize) {
    let chars: Vec<char> = segment.chars().collect();
    // One column is reserved so a caret at the very end of the line still has somewhere to sit.
    let span = width.saturating_sub(1).max(1);
    let start = caret_chars.saturating_sub(span);
    let end = chars.len().min(start + width);
    let visible: String = chars[start.min(chars.len())..end].iter().collect();
    let column = UnicodeWidthStr::width(
        chars[start.min(chars.len())..caret_chars.min(chars.len())]
            .iter()
            .collect::<String>()
            .as_str(),
    );
    (visible, column)
}

/// How many buffer rows the editor shows at once; longer buffers scroll to the newest.
const EDITOR_VISIBLE_LINES: usize = 5;

/// The opencode-style prompt: left accent border, element background, `❯` glyph with the live
/// buffer, and the caret sitting at the end of the buffer.
fn editor_widget(model: &Model, area: Rect) -> Paragraph<'static> {
    // Horizontal viewport: keep the caret (always at the end of the buffer) on screen.
    // Empty + thinking: skip the placeholder — the bouncing wall already says a turn is live.
    let mut rows: Vec<Line<'static>> = match () {
        _ if model.input.is_empty() && model.thinking.is_some() => Vec::new(),
        _ if model.input.is_empty() => vec![Line::from(vec![
            Span::styled(
                "\u{276f} ".to_string(),
                Style::default().fg(BLUE()).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "Ask Kamui, or / for commands".to_string(),
                Style::default().add_modifier(Modifier::DIM),
            ),
        ])],
        _ => {
            // One row per newline segment, scrolled to keep the caret in view. Joining the
            // segments into a single Line (as this did) collapsed a multi-line buffer onto one
            // row while the caret was placed per segment, so the two disagreed about the text.
            let inner = area.width.saturating_sub(4).max(1) as usize;
            let view = editor_view(&model.input, model.input_caret, inner);
            view.rows
                .into_iter()
                .enumerate()
                .map(|(offset, row)| {
                    Line::from(vec![
                        // Continuation rows repeat the prompt glyph's width so the text column
                        // -- and therefore the caret column -- is the same on every row.
                        if offset == 0 && model.input_caret <= model.input.len() {
                            Span::styled(
                                "\u{276f} ".to_string(),
                                Style::default().fg(BLUE()).add_modifier(Modifier::BOLD),
                            )
                        } else {
                            Span::raw("  ".to_string())
                        },
                        Span::styled(row, Style::default().fg(TEXT())),
                    ])
                })
                .collect()
        }
    };
    // Loading state: the editor keeps accepting input, so the run needs its own row here to
    // say that something is in flight and how to stop it. The bouncing wall is the sole
    // in-flight indicator; the transcript does not duplicate it.
    if let Some((frame_idx, label)) = model.thinking {
        let mut wall_line: Vec<Span<'static>> = vec![Span::raw("  ".to_string())];
        wall_line.extend(bouncing_wall_spans(frame_idx));
        wall_line.push(Span::raw(" ".to_string()));
        // pulse the label so the bar never looks frozen — wall moves, dots breathe
        let dots = ".".repeat(frame_idx % 4);
        wall_line.push(Span::styled(
            format!("{label}{dots}"),
            Style::default().fg(MUTED()).add_modifier(Modifier::DIM),
        ));
        rows.push(Line::from(wall_line));
    }
    Paragraph::new(Text::from(rows))
        .style(Style::default().bg(BG_ELEMENT()))
        .block(
            Block::default()
                .borders(Borders::LEFT)
                .border_style(Style::default().fg(BLUE())),
        )
}

/// Slash-command menu rendered above the editor while the buffer looks like a command.
/// How many menu rows fit above the editor at once; arrows scroll within this window.
const MENU_VISIBLE: usize = 8;

pub(crate) fn menu_height(item_count: usize) -> u16 {
    if item_count == 0 {
        0
    } else {
        (item_count.min(MENU_VISIBLE) as u16 + 4).min(12)
    }
}

/// The search bar: what was typed, and which match of how many is in view.
fn search_widget(search: &SearchState) -> Paragraph<'static> {
    let readout = if search.query.is_empty() {
        "type to search the transcript".to_string()
    } else if search.total == 0 {
        "no matches".to_string()
    } else {
        format!("{}/{}", search.current % search.total + 1, search.total)
    };
    Paragraph::new(Text::from(Line::from(vec![
        Span::styled(
            "search ".to_string(),
            Style::default().fg(BLUE()).add_modifier(Modifier::BOLD),
        ),
        Span::styled(search.query.clone(), Style::default().fg(TEXT())),
        Span::styled(
            format!("  {readout}"),
            Style::default().fg(MUTED()).add_modifier(Modifier::DIM),
        ),
        Span::styled(
            "  \u{b7} Enter next \u{b7} Up prev \u{b7} Esc close".to_string(),
            Style::default().fg(BORDER()).add_modifier(Modifier::DIM),
        ),
    ])))
    .style(Style::default().bg(POPUP_BG()))
}

fn popup_widget(model: &Model, area: Rect) -> Paragraph<'static> {
    let total = model.ac_items.len();
    let selected = model.ac_selected.min(total.saturating_sub(1));
    // Sliding window keeps the highlighted row in view no matter how many candidates match.
    let start = if total <= MENU_VISIBLE {
        0
    } else {
        selected
            .saturating_sub(MENU_VISIBLE / 2)
            .min(total - MENU_VISIBLE)
    };
    let end = total.min(start + MENU_VISIBLE);
    let content_width = area.width.saturating_sub(2) as usize;
    let name_width = 24.min(content_width.saturating_sub(4));
    let description_width = content_width.saturating_sub(name_width + 4);

    let counter = if total > MENU_VISIBLE {
        format!(" {} / {}", selected + 1, total)
    } else {
        String::new()
    };
    let rule = (area.width as usize)
        .saturating_sub(counter.chars().count() + 2)
        .max(1);
    let mut lines: Vec<Line<'static>> = vec![Line::from(vec![
        Span::styled("\u{2500}".repeat(rule), Style::default().fg(BORDER())),
        Span::styled(counter, Style::default().fg(BORDER())),
    ])];
    for idx in start..end {
        let (name, description) = &model.ac_items[idx];
        let is_on = idx == selected;
        let prefix = if is_on { "\u{276f} " } else { "  " };
        lines.push(Line::from(vec![
            Span::styled(
                prefix.to_string(),
                Style::default().fg(if is_on { BLUE() } else { BORDER() }),
            ),
            Span::styled(
                crate::tui::truncate_chars(name, name_width),
                Style::default()
                    .fg(if is_on { TEXT() } else { MUTED() })
                    .add_modifier(if is_on {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            ),
            Span::raw("  "),
            Span::styled(
                crate::tui::truncate_chars(description, description_width),
                Style::default().fg(MUTED()),
            ),
        ]));
    }
    Paragraph::new(Text::from(lines))
        .style(Style::default().bg(POPUP_BG()))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(BORDER()))
                .title(" commands ")
                .title_style(Style::default().fg(MUTED())),
        )
}

fn sidebar_paragraph(model: &Model, area: Rect) -> Paragraph<'static> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    // Left border eats one column; keep a little padding so values never kiss the rail.
    let max = area.width.saturating_sub(3).max(1) as usize;
    if let Some(plan) = &model.plan {
        lines.push(Line::from(Span::styled(
            "Plan",
            Style::default().fg(MUTED()).add_modifier(Modifier::BOLD),
        )));
        let compact_plan = area.width < 28 || area.height < 12;
        let steps = if compact_plan {
            plan.steps
                .iter()
                .filter(|(_, status)| *status == crate::tools::PlanStepStatus::InProgress)
                .take(1)
                .collect::<Vec<_>>()
        } else {
            plan.steps.iter().collect::<Vec<_>>()
        };
        let completed = plan
            .steps
            .iter()
            .filter(|(_, status)| *status == crate::tools::PlanStepStatus::Completed)
            .count();
        if compact_plan {
            lines.push(Line::styled(
                format!("Progress {completed}/{}", plan.steps.len()),
                Style::default().fg(MUTED()),
            ));
        }
        for (step, status) in steps {
            let mark = match status {
                crate::tools::PlanStepStatus::Completed => "x",
                crate::tools::PlanStepStatus::InProgress => "~",
                crate::tools::PlanStepStatus::Pending => " ",
            };
            lines.push(Line::from(vec![
                Span::styled(format!("[{mark}] "), Style::default().fg(BLUE())),
                Span::styled(
                    crate::tui::truncate_chars(step, max.saturating_sub(4)),
                    Style::default().fg(if *status == crate::tools::PlanStepStatus::InProgress {
                        TEXT()
                    } else {
                        MUTED()
                    }),
                ),
            ]));
        }
    }
    if let Some(entries) = &model.sidebar {
        let compact = area.height < (entries.len() as u16).saturating_mul(4);
        for (i, (key, value)) in entries.iter().enumerate() {
            // Section headers (Session/Runtime/Context/Activity/Last turn) render as a
            // small muted rule so groups read apart without a blank line each.
            if is_sidebar_section(key) {
                if i > 0 {
                    lines.push(Line::from(Span::styled(
                        "─".repeat(max.min(key.len() + 4)),
                        Style::default().fg(BORDER()),
                    )));
                }
                lines.push(Line::from(Span::styled(
                    key.to_string(),
                    Style::default().fg(MUTED()).add_modifier(Modifier::BOLD),
                )));
                for value_line in value.split('\n') {
                    push_sidebar_value(&mut lines, key, value_line, max);
                }
                continue;
            }
            lines.push(Line::from(Span::styled(
                format!("{key} "),
                Style::default().fg(TEXT()).add_modifier(Modifier::BOLD),
            )));
            // Values may carry newlines (Last turn metrics); ratatui strips them inside
            // spans, so split before styling. Tab-separated metric rows keep label/value
            // contrast; project paths truncate from the left so the leaf stays readable.
            for value_line in value.split('\n') {
                push_sidebar_value(&mut lines, key, value_line, max);
            }
            if !compact && i + 1 < entries.len() {
                lines.push(Line::from(""));
            }
        }
    }
    Paragraph::new(Text::from(lines))
        .style(Style::default().bg(BG_PANEL()))
        .block(
            Block::default()
                .padding(Padding::new(1, 0, 1, 0))
                .style(Style::default().bg(BG_PANEL())),
        )
}

/// Keys that open a sidebar group rather than a plain label row.
fn is_sidebar_section(key: &str) -> bool {
    matches!(
        key,
        "Session" | "Runtime" | "Context" | "Activity" | "Last turn"
    )
}

/// Semantic value color: only the load-bearing value gets ink, labels stay quiet.
fn sidebar_value_style(key: &str, label: &str, value: &str) -> Style {
    match (key, label) {
        (_, "model") => Style::default().fg(BLUE()),
        (_, "mode") => {
            if value.contains("plan") {
                Style::default().fg(WARN())
            } else {
                Style::default().fg(GREEN())
            }
        }
        (_, "git") => {
            if value.contains("changed") || value.contains('±') {
                Style::default().fg(WARN())
            } else {
                Style::default().fg(NOTICE_FG())
            }
        }
        (_, "cache") => Style::default().fg(CYAN()),
        _ => Style::default().fg(NOTICE_FG()),
    }
}
/// One sidebar value row: metric tab-rows keep label/value contrast, `bar\tNN`
/// rows render a variant-C usage bar, everything else truncates plainly.
fn push_sidebar_value(lines: &mut Vec<Line<'static>>, key: &str, value_line: &str, max: usize) {
    if let Some(pct_str) = value_line.strip_prefix("bar\t")
        && let Ok(pct) = pct_str.trim().parse::<u8>()
    {
        let pct = pct.min(100);
        let width = max.clamp(4, 12) as u64;
        let filled = (pct as u64 * width / 100) as usize;
        let color = if pct >= 80 {
            RED()
        } else if pct >= 50 {
            WARN()
        } else {
            GREEN()
        };
        let mut spans = vec![Span::styled(
            "▓".repeat(filled) + &"░".repeat(width as usize - filled),
            Style::default().fg(color),
        )];
        spans.push(Span::styled(
            format!(" {pct}%"),
            Style::default().fg(MUTED()),
        ));
        lines.push(Line::from(spans));
        return;
    }
    if let Some((label, rest)) = value_line.split_once('\t') {
        let style = sidebar_value_style(key, label, rest);
        // label on its own line, value indented on next line (user request: not "model model name")
        lines.push(Line::styled(
            format!("{label} :"),
            Style::default().fg(TEXT()).add_modifier(Modifier::BOLD),
        ));
        let indented = format!(
            " {}",
            crate::tui::truncate_chars(rest, max.saturating_sub(1))
        );
        // mcp value already contains newlines/bullets — keep as-is but indented
        for part in indented.split('\n') {
            lines.push(Line::styled(part.to_string(), style));
        }
        return;
    }
    let truncated = if key == "Project" {
        crate::tui::truncate_left_chars(value_line, max)
    } else {
        crate::tui::truncate_chars(value_line, max)
    };
    lines.push(Line::styled(truncated, Style::default().fg(NOTICE_FG())));
}

/// The home screen: two-tone block-letter logo centered above the version/model line and the
/// getting-started hint. Mirrors opencode's muted-left / bright-right treatment.
fn intro_paragraph(model: &Model, area: Rect) -> Paragraph<'static> {
    let width = area.width.max(20) as usize;
    let mut lines: Vec<Line<'static>> = Vec::new();
    let total_height = LOGO_LEFT.len() + 6;
    let top_pad = area.height.saturating_sub(total_height as u16 + 2) as usize / 6;
    for _ in 0..top_pad {
        lines.push(Line::from(""));
    }
    for (left, right) in LOGO_LEFT.iter().zip(LOGO_RIGHT.iter()) {
        let combined = format!("{left}{right}");
        let pad = width.saturating_sub(combined.chars().count()) / 2;
        lines.push(Line::from(vec![
            Span::raw(" ".repeat(pad)),
            Span::styled((*left).to_string(), Style::default().fg(NOTICE_FG())),
            Span::styled(
                (*right).to_string(),
                Style::default().fg(BLUE()).add_modifier(Modifier::BOLD),
            ),
        ]));
    }
    lines.push(Line::from(""));
    let info = model.header.clone();
    let pad = width.saturating_sub(UnicodeWidthStr::width(info.as_str())) / 2;
    lines.push(Line::from(vec![
        Span::raw(" ".repeat(pad)),
        Span::styled(info, Style::default().fg(NOTICE_FG())),
    ]));
    let hint = "Ask Kamui, or / for commands";
    let pad = width.saturating_sub(hint.len()) / 2;
    lines.push(Line::from(vec![
        Span::raw(" ".repeat(pad)),
        Span::styled(
            hint.to_string(),
            Style::default().add_modifier(Modifier::DIM),
        ),
    ]));
    lines.push(Line::from(""));
    let keys = "Ctrl+K  models    Ctrl+S  sessions    ?  keys";
    let pad = width.saturating_sub(keys.len()) / 2;
    lines.push(Line::from(vec![
        Span::raw(" ".repeat(pad)),
        Span::styled(keys.to_string(), Style::default().fg(MUTED())),
    ]));
    // Startup notices still belong on the home screen — the logo must never hide them.
    for card in &model.cards {
        let mut rows = card.title.lines().chain(card.body.lines()).peekable();
        if rows.peek().is_some() {
            lines.push(Line::from(""));
        }
        for row in rows {
            lines.push(Line::styled(row.to_string(), Style::default().fg(MUTED())));
        }
    }
    if model.warnings_visible {
        for warning in &model.warnings {
            lines.push(Line::from(""));
            lines.push(Line::styled(
                format!("\u{26a0} {warning}"),
                Style::default().fg(WARN()),
            ));
        }
        if model.warning_details_visible {
            for detail in &model.warning_details {
                lines.push(Line::from(""));
                lines.push(Line::styled(
                    format!("  ↳ {detail}"),
                    Style::default().fg(MUTED()),
                ));
            }
        }
    }
    Paragraph::new(Text::from(lines)).style(Style::default().bg(BG_CHAT()))
}

/// Status line: a few discoverable keys, live run hints, token badge.
fn footer_widget(model: &Model, area: Rect) -> Paragraph<'static> {
    let mut left = if model.footer.is_empty() {
        "? help".to_string()
    } else {
        model.footer.clone()
    };
    // Keep action/status hints ahead of discoverability hints: truncation should not hide control
    // of an in-flight turn or the fact that input was queued.
    if model.thinking.is_some() {
        left.push_str("  \u{b7}  Esc interrupts  \u{b7}  Enter steers");
    }
    if model.scroll_from_bottom > 0 {
        left.push_str(&format!(
            "  \u{b7}  \u{2191} {} row(s) back  \u{b7}  Ctrl+End live",
            model.scroll_from_bottom
        ));
    }
    if model.queued_count > 0 {
        let plural = if model.queued_count == 1 { "" } else { "s" };
        left.push_str(&format!(
            "  \u{b7}  {} message{} queued",
            model.queued_count, plural
        ));
    }
    if !left.contains("Ctrl+K") {
        left.push_str("  \u{b7}  Ctrl+K  \u{b7}  Ctrl+S");
    }
    if let Some(plan) = &model.plan
        && let Some(active) = &plan.active
    {
        left.push_str(&format!(
            "  ·  plan: {}",
            crate::tui::truncate_chars(active, 36)
        ));
    }
    // Model and token badge are pinned to the right edge: they are the two facts worth a glance
    // at any moment, and letting the hint text push them off the row (a plain Paragraph clips at
    // the right) hid them exactly when the hints grew -- scrolled back, queued, mid-turn.
    let mut right: Vec<Span<'static>> = Vec::new();
    if let Some(entries) = &model.sidebar
        && let Some((_, model_name)) = entries.iter().find(|(key, _)| key == "Model")
    {
        right.push(Span::styled(
            crate::tui::truncate_chars(model_name, 24),
            Style::default().fg(BORDER()),
        ));
    }
    if let Some((badge_text, pct)) = &model.token_badge {
        if !right.is_empty() {
            right.push(Span::raw("  "));
        }
        right.push(Span::styled(
            format!(" {badge_text} "),
            Style::default()
                .bg(if *pct >= 80 { WARN() } else { TEXT() })
                .fg(BG_PANEL())
                .add_modifier(Modifier::BOLD),
        ));
    }
    let total = area.width as usize;
    let right_width: usize = right
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum();
    // The hints yield to the right cluster rather than the other way round.
    let left_budget = total.saturating_sub(right_width + 2);
    let left = crate::tui::truncate_chars(&left, left_budget);
    let gap = total.saturating_sub(UnicodeWidthStr::width(left.as_str()) + right_width);
    let mut spans = vec![Span::styled(left, Style::default().fg(MUTED()))];
    if !right.is_empty() {
        spans.push(Span::raw(" ".repeat(gap)));
        spans.extend(right);
    }
    Paragraph::new(Line::from(spans)).style(Style::default().bg(BG_PANEL()))
}

/// The transcript as it is actually drawn: every source line wrapped to `width`, each wrapped
/// row still carrying its owning card. Search re-uses this so the row it counts is the row that
/// gets rendered.
fn wrapped_transcript(model: &Model, width: u16) -> Vec<(Line<'static>, Option<u64>)> {
    let fp = wrapped_fingerprint(model);
    let cache = WRAPPED_CACHE.get_or_init(|| Mutex::new(None));
    if let Ok(guard) = cache.lock()
        && let Some(cached) = guard.as_ref()
        && cached.width == width
        && cached.fp == fp
    {
        return cached.rows.clone();
    }
    let mut rows = Vec::new();
    for (line, owner) in transcript_rows(model, width) {
        for row in wrap_card_line(&line, width.max(1) as usize) {
            rows.push((Line::from(row), owner));
        }
    }
    if let Ok(mut guard) = cache.lock() {
        *guard = Some(WrappedCache {
            width,
            fp,
            rows: rows.clone(),
        });
    }
    rows
}

/// Wraps one transcript line, keeping a card's rail on every row it produces. `card_lines`
/// emits `\u{258c} ` as the first span of every row it owns; wrapping the line as a flat span
/// list dropped that prefix from the continuation rows, so a long assistant paragraph looked
/// like its rail stopped one line in and the text jumped back to column zero.
fn wrap_card_line(line: &Line<'_>, width: usize) -> Vec<Vec<Span<'static>>> {
    let width = width.max(1);
    let Some(rail) = line
        .spans
        .first()
        .filter(|span| span.content == THICK_BORDER)
    else {
        return wrap_spans(&line.spans, width);
    };
    let rail_style = rail.style;
    let indent = UnicodeWidthStr::width(THICK_BORDER);
    wrap_spans(&line.spans[1..], width.saturating_sub(indent).max(1))
        .into_iter()
        .map(|row| {
            let mut out = vec![Span::styled(THICK_BORDER.to_string(), rail_style)];
            out.extend(row);
            out
        })
        .collect()
}

/// Repaints a whole drawn row onto `background`. Colouring only the matched substring would
/// mean re-deriving character offsets through styling and wrapping that have already been
/// applied; marking the row says the same thing and cannot drift out of step with it.
fn highlight_row(line: Line<'static>, background: Color) -> Line<'static> {
    Line::from(
        line.spans
            .into_iter()
            .map(|span| {
                let style = span.style.bg(background);
                Span::styled(span.content.to_string(), style)
            })
            .collect::<Vec<_>>(),
    )
}

/// Plain text of one drawn row, for matching a search against.
fn row_text(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

/// Indices of the drawn rows containing `needle`, case-insensitively.
fn matching_rows(rows: &[(Line<'static>, Option<u64>)], needle: &str) -> Vec<usize> {
    if needle.is_empty() {
        return Vec::new();
    }
    let needle = needle.to_lowercase();
    rows.iter()
        .enumerate()
        .filter(|(_, (line, _))| row_text(line).to_lowercase().contains(&needle))
        .map(|(index, _)| index)
        .collect()
}

/// The `scroll_from_bottom` that puts `row` in the middle of a `visible`-row viewport.
fn scroll_to_row(total_rows: usize, visible: usize, row: usize) -> usize {
    let half = visible / 2;
    let start = row.saturating_sub(half);
    let max_start = total_rows.saturating_sub(visible);
    total_rows
        .saturating_sub(visible)
        .saturating_sub(start.min(max_start))
}

/// Every transcript line paired with the card it belongs to (`None` for notices and warnings,
/// which fold nothing and so are not click targets).
fn transcript_rows(model: &Model, width: u16) -> Vec<(Line<'static>, Option<u64>)> {
    let inner_width = width.max(20) as usize;
    let mut lines: Vec<(Line<'static>, Option<u64>)> = Vec::new();
    for card in &model.cards {
        if matches!(card.kind, CardKind::User) {
            lines.push((
                Line::styled(
                    "─".repeat(inner_width.min(24)),
                    Style::default().fg(BORDER()),
                ),
                None,
            ));
        }
        let owner = (card.foldable_rows() > 0).then_some(card.id);
        let mut card_out = card_lines(card, inner_width);
        // Drop trailing blank content rows so the single swimlane separator stays single.
        while card_out
            .last()
            .is_some_and(|line| line.spans.iter().all(|s| s.content.trim().is_empty()))
        {
            card_out.pop();
        }
        for line in card_out {
            lines.push((line, owner));
        }
        lines.push((Line::from(""), None));
    }
    // No blank padding under the last card — bottom-align already sits the stack on the editor.
    if !model.cards.is_empty() {
        lines.pop();
    }
    // The separator belongs to the warning block, not to the flag: with warnings visible but
    // none pending -- the default -- this pushed a stray blank row under every transcript.
    if model.warnings_visible && !model.warnings.is_empty() {
        if !lines.is_empty() {
            lines.push((Line::from(""), None));
        }
        for warning in &model.warnings {
            lines.push((
                Line::styled(format!("\u{26a0} {warning}"), Style::default().fg(WARN())),
                None,
            ));
        }
        if model.warning_details_visible {
            for detail in &model.warning_details {
                lines.push((Line::from(""), None));
                lines.push((
                    Line::styled(format!("  \u{21b3} {detail}"), Style::default().fg(MUTED())),
                    None,
                ));
            }
        }
    }
    lines
}

/// OpenCode's chat grammar (internal/tui/components/chat/message.go): every message is a thick
/// left border with no background fill — user borders secondary-blue, assistant the brand
/// accent, tool calls muted — and raw output truncates to a head window with an expand hint.
const THICK_BORDER: &str = "\u{258c} ";
/// Rows shown for a folded card before the expand hint.
const COLLAPSED_PEEK: usize = 2;

/// One-line call header: the tool name plus a trimmed peek at its arguments, so a folded card
/// still says what ran and against what.
fn tool_header(name: &str, args: &str) -> String {
    let compact = args.split_whitespace().collect::<Vec<_>>().join(" ");
    let compact = crate::tui::truncate_chars(&compact, 60);
    if compact.is_empty() {
        name.to_string()
    } else {
        format!("{name}  {compact}")
    }
}

/// Fold a dumped error so a URL/serde wall cannot collide with the editor. Short messages stay
/// open. The wrap width is not known at insert time, so a typical inner width is the cutoff.
fn error_should_fold(body: &str) -> bool {
    let mut n = 0usize;
    for line in body.lines() {
        n += 1;
        if n > 2 || UnicodeWidthStr::width(line) > 80 {
            return true;
        }
    }
    let lower = body.to_ascii_lowercase();
    lower.contains("http") && lower.contains("error decoding")
}

/// One-line card title for a folded error: human summary, not the raw reqwest sentence. The
/// full body (URL included) stays available on expand.
fn error_headline(body: &str) -> String {
    let first = body.lines().next().unwrap_or("").trim();
    if first.is_empty() {
        return String::new();
    }
    let lower = first.to_ascii_lowercase();
    if lower.contains("error decoding response body")
        || lower.contains("invalid type:")
        || lower.contains("expected a sequence")
    {
        return "provider returned an invalid response".to_string();
    }
    if lower.contains("error sending request") || lower.contains("connection refused") {
        return "provider request failed".to_string();
    }
    if let Some(idx) = first.find(" for url ") {
        let clause = first[..idx].trim().trim_end_matches(':').trim();
        if !clause.is_empty() {
            return clause.to_string();
        }
    }
    if let Some(idx) = lower.find(" (http") {
        let clause = first[..idx].trim().trim_end_matches(':').trim();
        if !clause.is_empty() {
            return clause.to_string();
        }
    }
    first.to_string()
}

fn card_lines(card: &Card, width: usize) -> Vec<Line<'static>> {
    let (border, body_style) = if card.title == "Assistant" {
        (BLUE(), Style::default().fg(TEXT()))
    } else {
        match card.kind {
            CardKind::User => (ACCENT(), Style::default().fg(TEXT())),
            CardKind::Tool => match &card.status {
                None => (WARN(), Style::default().fg(MUTED())),
                Some((_, true)) => (GREEN(), Style::default().fg(MUTED())),
                Some((_, false)) => (RED(), Style::default().fg(RED())),
            },
            CardKind::Output => match &card.status {
                Some((_, false)) => (RED(), Style::default().fg(RED())),
                Some((_, true)) => (GREEN(), Style::default().fg(MUTED())),
                None => (GREEN(), Style::default().fg(MUTED())),
            },
            CardKind::Error => (RED(), Style::default().fg(TEXT())),
            CardKind::Note => (MUTED(), Style::default().fg(NOTICE_FG())),
        }
    };

    let mut out = Vec::new();
    let push_bordered = |out: &mut Vec<Line<'static>>, spans: Vec<Span<'static>>| {
        let mut line = vec![Span::styled(
            THICK_BORDER.to_string(),
            Style::default().fg(border),
        )];
        line.extend(spans);
        out.push(Line::from(line));
    };

    if card.title == "Assistant" && !card.collapsed {
        push_bordered(
            &mut out,
            vec![Span::styled(
                "Kamui".to_string(),
                Style::default().fg(BLUE()).add_modifier(Modifier::BOLD),
            )],
        );
        let text = crate::markdown::render_ratatui(&card.body);
        for line in text.lines {
            let spans: Vec<Span<'static>> = line
                .spans
                .into_iter()
                .map(|span| Span::styled(span.content.to_string(), span.style.patch(body_style)))
                .collect();
            push_bordered(&mut out, spans);
        }
        return out;
    }

    if matches!(card.kind, CardKind::Error) {
        let title = if card.title.is_empty() {
            "Error".to_string()
        } else {
            card.title.clone()
        };
        push_bordered(
            &mut out,
            vec![Span::styled(
                title,
                Style::default().fg(TEXT()).add_modifier(Modifier::BOLD),
            )],
        );
        if let Some((status, ok)) = &card.status {
            push_bordered(
                &mut out,
                vec![Span::styled(
                    format!("{} {status}", if *ok { "\u{2713}" } else { "\u{2717}" }),
                    Style::default().fg(if *ok { GREEN() } else { RED() }),
                )],
            );
        }
        let wrap_width = width.saturating_sub(4).max(1);
        if card.collapsed {
            let headline = crate::tui::truncate_chars(&error_headline(&card.body), wrap_width);
            if !headline.is_empty() {
                push_bordered(&mut out, vec![Span::styled(headline, body_style)]);
            }
            if !card.body.trim().is_empty() {
                push_bordered(
                    &mut out,
                    vec![Span::styled(
                        "\u{2026} ctrl+o or click".to_string(),
                        Style::default().fg(MUTED()),
                    )],
                );
            }
            return out;
        }
        for line in wrap_display(&card.body, wrap_width) {
            push_bordered(&mut out, vec![Span::styled(line, body_style)]);
        }
        return out;
    }

    // Tool cards lead with their own header row. Without it a finished call reduced to a
    // bare outcome ("completed - 0ms - 332 chars") that never said which tool produced it.
    if matches!(card.kind, CardKind::Tool | CardKind::Note) && !card.title.is_empty() {
        push_bordered(
            &mut out,
            vec![Span::styled(
                card.title.clone(),
                Style::default().fg(TEXT()).add_modifier(Modifier::BOLD),
            )],
        );
    }
    // The outcome stays visible whether or not the output is folded: how a tool ended is the
    // part worth reading at a glance, and it used to be a detached notice further down.
    if let Some((status, ok)) = &card.status {
        push_bordered(
            &mut out,
            vec![Span::styled(
                format!("{} {status}", if *ok { "\u{2713}" } else { "\u{2717}" }),
                Style::default().fg(if *ok { GREEN() } else { RED() }),
            )],
        );
    }

    let total_lines = card.body.lines().count();
    if card.collapsed {
        // A card that already shows an outcome needs no peek: it folds to two tidy rows and
        // opens on demand. Cards without one keep the old head window.
        let peek = if card.status.is_some() {
            0
        } else {
            COLLAPSED_PEEK
        };
        for source in card.body.lines().take(peek) {
            for row in wrap_display(source, width.saturating_sub(4)) {
                push_bordered(&mut out, vec![Span::styled(row, body_style)]);
            }
        }
        let hidden = total_lines.saturating_sub(peek);
        if hidden > 0 && !card.body.trim().is_empty() {
            push_bordered(
                &mut out,
                vec![Span::styled(
                    format!("\u{2026} {hidden} more line(s) \u{b7} ctrl+o or click"),
                    Style::default().fg(MUTED()),
                )],
            );
        }
        return out;
    }

    match card.kind {
        CardKind::User => {
            push_bordered(
                &mut out,
                vec![Span::styled(
                    "You".to_string(),
                    Style::default().fg(ACCENT()).add_modifier(Modifier::BOLD),
                )],
            );
            if card.title != "User" && !card.title.is_empty() {
                push_bordered(
                    &mut out,
                    vec![Span::styled(
                        card.title.clone(),
                        Style::default().fg(MUTED()).add_modifier(Modifier::DIM),
                    )],
                );
            }
            for line in wrap_display(&card.body, width.saturating_sub(4)) {
                push_bordered(&mut out, vec![Span::styled(line, body_style)]);
            }
        }
        _ => {
            // Tool call: "Name: args", then its output beneath, all under one muted rail.
            for (i, source) in card.body.lines().enumerate() {
                let styled = if i == 0 {
                    Span::styled(source.to_string(), Style::default().fg(TEXT()))
                } else {
                    Span::styled(source.to_string(), body_style)
                };
                for row in wrap_display(styled.content.as_ref(), width.saturating_sub(4)) {
                    if i == 0 {
                        push_bordered(
                            &mut out,
                            vec![Span::styled(row, Style::default().fg(TEXT()))],
                        );
                    } else {
                        push_bordered(&mut out, vec![Span::styled(row, body_style)]);
                    }
                }
            }
        }
    }
    out
}

/// Char-greedy wrap of styled spans to `width` columns. Carries each source span's style into
/// the produced rows; soft-wrap only (newlines already split upstream).
/// Word-aware greedy wrap of styled spans to `width` columns. Breaks at spaces when a word
/// fits on the next row; hard-splits only words longer than the whole width. Styles carry
/// through every produced row.
fn wrap_spans(spans: &[Span<'_>], width: usize) -> Vec<Vec<Span<'static>>> {
    let width = width.max(1);
    #[derive(Clone)]
    enum Tok {
        Word(String, Style),
        Space(String, Style),
        Break,
    }
    let mut toks: Vec<Tok> = Vec::new();
    for span in spans {
        let style = span.style;
        let mut cur = String::new();
        let mut cur_is_space = false;
        for ch in span.content.chars() {
            if ch == '\n' {
                if !cur.is_empty() {
                    let done = std::mem::take(&mut cur);
                    toks.push(if cur_is_space {
                        Tok::Space(done, style)
                    } else {
                        Tok::Word(done, style)
                    });
                }
                toks.push(Tok::Break);
                cur_is_space = false;
                continue;
            } else {
                let is_sp = ch == ' ';
                if !cur.is_empty() && is_sp != cur_is_space {
                    let done = std::mem::take(&mut cur);
                    toks.push(if cur_is_space {
                        Tok::Space(done, style)
                    } else {
                        Tok::Word(done, style)
                    });
                }
                cur_is_space = is_sp;
            }
            cur.push(ch);
        }
        if !cur.is_empty() {
            toks.push(if cur_is_space {
                Tok::Space(cur, style)
            } else {
                Tok::Word(cur, style)
            });
        }
    }

    fn tok_width(t: &str) -> usize {
        t.chars()
            .map(|c| UnicodeWidthChar::width(c).unwrap_or(0))
            .sum()
    }

    let mut rows: Vec<Vec<Span<'static>>> = vec![Vec::new()];
    let push_row_text = |rows: &mut Vec<Vec<Span<'static>>>, text: String, style: Style| {
        rows.last_mut().unwrap().push(Span::styled(text, style));
    };

    let mut used = 0usize;
    for tok in &toks {
        match tok {
            Tok::Break => {
                rows.push(Vec::new());
                used = 0;
            }
            Tok::Space(text, style) => {
                if used == 0 {
                    continue; // no leading spaces after a wrap
                }
                let w = tok_width(text);
                if used + w > width {
                    continue; // spaces vanish at end of row
                }
                used += w;
                push_row_text(&mut rows, text.clone(), *style);
            }
            Tok::Word(text, style) => {
                let w = tok_width(text);
                if w > width {
                    // Hard-split oversized words by display width.
                    let mut rest = text.as_str();
                    loop {
                        if used >= width {
                            rows.push(Vec::new());
                            used = 0;
                        }
                        let avail = width - used;
                        let mut take_w = 0usize;
                        let mut take_end = 0usize;
                        for (i, ch) in rest.char_indices() {
                            let cw = UnicodeWidthChar::width(ch).unwrap_or(0).max(1);
                            if take_w + cw > avail {
                                take_end = i;
                                break;
                            }
                            take_w += cw;
                            take_end = i + ch.len_utf8();
                        }
                        if take_end == 0 {
                            // A char wider than the remaining columns: take it whole anyway so the
                            // loop always advances. (str::ceil_char_boundary is still unstable.)
                            if let Some(ch) = rest.chars().next() {
                                take_end = ch.len_utf8();
                                take_w = UnicodeWidthChar::width(ch).unwrap_or(0).max(1);
                            }
                        }
                        let piece = &rest[..take_end];
                        push_row_text(&mut rows, piece.to_string(), *style);
                        used += take_w;
                        rest = &rest[take_end..];
                        if rest.is_empty() {
                            break;
                        }
                    }
                } else {
                    if used + w > width && used > 0 {
                        rows.push(Vec::new());
                        used = 0;
                    }
                    used += w;
                    push_row_text(&mut rows, text.clone(), *style);
                }
            }
        }
    }
    rows
}

fn wrap_display(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut result = Vec::new();
    for source in text.lines() {
        if source.is_empty() {
            result.push(String::new());
            continue;
        }
        let mut current = String::new();
        let mut current_width = 0usize;
        for ch in source.chars() {
            let char_width = UnicodeWidthChar::width(ch).unwrap_or(0);
            if current_width > 0 && current_width + char_width > width {
                result.push(std::mem::take(&mut current));
                current_width = 0;
            }
            current.push(ch);
            current_width += char_width;
        }
        result.push(current);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_paints_the_full_terminal_background() {
        let backend = ratatui::backend::TestBackend::new(83, 25);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| {
                render(frame, &Model::default());
            })
            .expect("draw");

        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(82, 24)].bg, BG_PANEL());
        assert_ne!(buffer[(82, 24)].bg, Color::Black);
    }

    /// Draws one frame into a test backend and reports where the terminal caret ended up
    /// together with the row it landed on, so caret and text can be compared directly.
    fn caret_and_row(model: &Model) -> ((u16, u16), String) {
        let backend = ratatui::backend::TestBackend::new(60, 12);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| {
                render(frame, model);
            })
            .expect("draw");
        let pos = terminal.get_cursor_position().expect("caret");
        let buffer = terminal.backend().buffer().clone();
        let row: String = (0..60).map(|x| buffer[(x, pos.y)].symbol()).collect();
        ((pos.x, pos.y), row)
    }

    /// Collects a card's rendered rows as plain strings.
    fn rendered(card: &Card, width: usize) -> Vec<String> {
        card_lines(card, width)
            .iter()
            .map(|line| line.spans.iter().map(|s| s.content.to_string()).collect())
            .collect()
    }

    fn note(id: u64, title: &str, body: &str) -> Card {
        Card {
            id,
            kind: CardKind::Note,
            title: title.to_string(),
            body: body.to_string(),
            status: None,
            collapsed: false,
        }
    }

    #[test]
    fn wrapped_assistant_text_keeps_its_rail_on_every_row() {
        let model = Model {
            intro: false,
            cards: vec![Card {
                id: 1,
                kind: CardKind::Output,
                title: "Assistant".into(),
                body: "satu paragraf panjang tanpa newline yang harus dibungkus beberapa kali"
                    .into(),
                status: None,
                collapsed: false,
            }],
            ..Default::default()
        };
        let rows = wrapped_transcript(&model, 30);
        assert!(rows.len() > 2, "the paragraph wraps: {}", rows.len());
        for (line, _) in &rows {
            let text = row_text(line);
            assert!(
                text.starts_with(THICK_BORDER),
                "every wrapped row keeps the rail: {text:?}"
            );
            assert!(
                UnicodeWidthStr::width(text.as_str()) <= 30,
                "no row overruns the transcript: {text:?}"
            );
        }
    }

    #[test]
    fn the_token_badge_survives_a_crowded_footer() {
        let model = Model {
            intro: false,
            footer: "? help".into(),
            queued_count: 3,
            scroll_from_bottom: 42,
            sidebar: Some(vec![("Model".into(), "some-very-long-model-name".into())]),
            token_badge: Some(("9.9k tok 41%".into(), 41)),
            ..Default::default()
        };
        let area = Rect {
            x: 0,
            y: 0,
            width: 60,
            height: 1,
        };
        let backend = ratatui::backend::TestBackend::new(60, 1);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| {
                frame.render_widget(footer_widget(&model, area), area);
            })
            .expect("draw");
        let buffer = terminal.backend().buffer().clone();
        let row: String = (0..60).map(|x| buffer[(x, 0)].symbol()).collect();
        assert!(
            row.contains("9.9k tok 41%"),
            "the badge is pinned right, not clipped by the hints: {row:?}"
        );
        assert!(
            row.trim_end().ends_with("9.9k tok 41%"),
            "the badge sits at the right edge: {row:?}"
        );
    }

    #[test]
    fn compact_sidebar_plan_shows_progress_and_active_step_only() {
        let model = Model {
            plan: Some(crate::tools::PlanView {
                steps: vec![
                    ("done".into(), crate::tools::PlanStepStatus::Completed),
                    (
                        "current active step".into(),
                        crate::tools::PlanStepStatus::InProgress,
                    ),
                    (
                        "later pending step".into(),
                        crate::tools::PlanStepStatus::Pending,
                    ),
                ],
                active: Some("current active step".into()),
            }),
            ..Model::default()
        };
        let backend = ratatui::backend::TestBackend::new(24, 8);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| {
                frame.render_widget(sidebar_paragraph(&model, frame.area()), frame.area())
            })
            .expect("draw");
        let buffer = terminal.backend().buffer().clone();
        let text: String = (0..8)
            .flat_map(|y| {
                (0..24).map({
                    let buffer = buffer.clone();
                    move |x| buffer[(x, y)].symbol().to_string()
                })
            })
            .collect();
        assert!(text.contains("Progress 1/3"));
        assert!(text.contains("current"));
        assert!(!text.contains("later pending step"));
    }

    #[test]
    fn dialog_geometry_stays_inside_tiny_area() {
        let dialog = DialogState::new("title", "/", vec![("value".into(), "label".into())]);
        let (box_area, _, _) = dialog_geometry(&dialog, Rect::new(0, 0, 3, 2));
        assert!(box_area.width <= 3);
        assert!(box_area.height <= 2);
    }

    #[test]
    fn folding_targets_the_newest_card_that_has_something_folded() {
        // A command leaves a note cell behind as the literal last card. `/expand` used to aim
        // at that instead of the tool output the user had just seen.
        let cards = vec![
            note(1, "", "no body to speak of"),
            Card {
                id: 2,
                kind: CardKind::Tool,
                title: "read_file".into(),
                body: "a\nb\nc".into(),
                status: Some(("completed".into(), true)),
                collapsed: true,
            },
            note(3, "/sessions", ""),
        ];
        assert_eq!(
            last_foldable_index(&cards),
            Some(1),
            "the tool card, not the trailing note"
        );
        assert_eq!(last_foldable_index(&[]), None);
        assert_eq!(
            last_foldable_index(&[note(1, "/help", "")]),
            None,
            "a cell with no body folds nothing"
        );
    }

    #[test]
    fn copying_an_answer_yields_the_raw_markdown() {
        let card = Card {
            id: 1,
            kind: CardKind::Output,
            title: "Assistant".into(),
            body: "# Heading\n\nsome **text**".into(),
            status: None,
            collapsed: false,
        };
        // No rails, no header: what is copied is what the model wrote.
        assert_eq!(card.clipboard_text(), "# Heading\n\nsome **text**");
    }

    #[test]
    fn copying_a_tool_cell_keeps_what_says_the_body_is() {
        let card = Card {
            id: 1,
            kind: CardKind::Tool,
            title: tool_header("read_file", r#"{"path": "src/main.rs"}"#),
            body: "fn main() {}".into(),
            status: Some(("completed \u{b7} 3ms \u{b7} 12 chars".into(), true)),
            collapsed: true,
        };
        let text = card.clipboard_text();
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[0].contains("read_file"), "{text:?}");
        assert!(lines[1].contains("completed"), "{text:?}");
        assert_eq!(lines[2], "fn main() {}");
    }

    #[test]
    fn a_command_and_its_output_render_as_one_cell() {
        let card = note(1, "/sessions", "row one\nrow two");
        let rows = rendered(&card, 60);
        assert_eq!(rows.len(), 3, "header plus both rows: {rows:?}");
        assert!(
            rows[0].contains("/sessions"),
            "the command heads the cell: {rows:?}"
        );
        assert!(
            rows[1].contains("row one") && rows[2].contains("row two"),
            "{rows:?}"
        );
        for row in &rows {
            assert!(
                row.starts_with('\u{258c}'),
                "every row carries the cell rail: {row:?}"
            );
        }
    }

    #[test]
    fn command_cells_keep_their_place_in_the_transcript() {
        // Notices used to render after every card regardless of when they happened, so command
        // output always sank to the bottom instead of sitting where it was produced.
        let model = Model {
            intro: false,
            cards: vec![
                note(1, "/model ornith", "Now using ornith:latest (ornith)."),
                Card {
                    id: 2,
                    kind: CardKind::User,
                    title: "User".into(),
                    body: "halo".into(),
                    status: None,
                    collapsed: false,
                },
                note(3, "/compact", "Not enough history to compact yet."),
            ],
            ..Default::default()
        };
        let rows: Vec<String> = transcript_rows(&model, 60)
            .iter()
            .map(|(line, _)| line.spans.iter().map(|s| s.content.to_string()).collect())
            .collect();
        let position = |needle: &str| {
            rows.iter()
                .position(|row| row.contains(needle))
                .unwrap_or_else(|| panic!("{needle} missing from {rows:?}"))
        };
        assert!(position("/model ornith") < position("halo"));
        assert!(position("halo") < position("/compact"));
    }

    #[test]
    fn a_running_turn_reports_itself_in_the_editor_not_the_transcript() {
        // The bouncing wall in the editor is the sole in-flight indicator. Duplicating a
        // spinner under the last message made the wait look like two agents.
        let model = Model {
            intro: false,
            cards: vec![note(1, "", "earlier output")],
            thinking: Some((0, "Thinking...")),
            ..Default::default()
        };
        let rows: Vec<String> = transcript_rows(&model, 60)
            .iter()
            .map(|(line, _)| line.spans.iter().map(|s| s.content.to_string()).collect())
            .collect();
        assert!(
            rows.iter().any(|row| row.contains("earlier output")),
            "earlier output present"
        );
        assert!(
            rows.iter().all(|row| !row.contains("Thinking")),
            "the transcript does not repeat the thinking label: {rows:?}"
        );

        let backend = ratatui::backend::TestBackend::new(80, 14);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| {
                render(frame, &model);
            })
            .expect("draw");
        let buffer = terminal.backend().buffer().clone();
        let all: Vec<String> = (0..14)
            .map(|y| (0..80).map(|x| buffer[(x, y)].symbol()).collect())
            .collect();
        assert!(
            all.iter().any(|row| row.contains("Thinking")),
            "the editor wall names the run: {all:?}"
        );
        assert!(
            all.iter().any(|row| row.contains("Esc interrupts")),
            "the footer says how to stop: {all:?}"
        );
    }

    #[test]
    fn the_editor_stays_usable_while_a_turn_runs() {
        let model = Model {
            intro: false,
            input: "steer me".into(),
            input_caret: 8,
            thinking: Some((3, "Thinking...")),
            ..Default::default()
        };
        let backend = ratatui::backend::TestBackend::new(80, 14);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| {
                render(frame, &model);
            })
            .expect("draw");
        let caret = terminal.get_cursor_position().expect("caret");
        let buffer = terminal.backend().buffer().clone();
        let row: String = (0..80).map(|x| buffer[(x, caret.y)].symbol()).collect();
        // The buffer is still shown and the caret still sits at its end, rather than the editor
        // being replaced by a status label for the duration of the turn.
        assert!(row.contains("steer me"), "buffer stays visible: {row:?}");
        assert_eq!(caret.x, "\u{2502}\u{276f} steer me".chars().count() as u16);
        let all: Vec<String> = (0..14)
            .map(|y| (0..80).map(|x| buffer[(x, y)].symbol()).collect())
            .collect();
        assert!(
            all.iter().any(|row| row.contains("Esc interrupts")),
            "the footer says how to stop: {all:?}"
        );
        assert!(
            all.iter().all(|row| !row.contains("type to steer")),
            "steering copy is not a second row when the buffer already shows the steer: {all:?}"
        );
    }

    #[test]
    fn empty_thinking_editor_drops_the_placeholder() {
        let model = Model {
            intro: false,
            thinking: Some((1, "Thinking...")),
            ..Default::default()
        };
        let backend = ratatui::backend::TestBackend::new(80, 14);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| {
                render(frame, &model);
            })
            .expect("draw");
        let buffer = terminal.backend().buffer().clone();
        let all: Vec<String> = (0..14)
            .map(|y| (0..80).map(|x| buffer[(x, y)].symbol()).collect())
            .collect();
        assert!(
            all.iter().all(|row| !row.contains("type to steer")),
            "wall + hint are enough; no steer placeholder: {all:?}"
        );
        assert!(
            all.iter().all(|row| !row.contains("type a message")),
            "idle placeholder stays off while thinking: {all:?}"
        );
        assert!(
            all.iter().any(|row| row.contains("Thinking")),
            "the wall still names the run: {all:?}"
        );
        assert!(
            all.iter().any(|row| row.contains("Esc interrupts")),
            "the footer owns the control hint: {all:?}"
        );
    }

    fn rows_of(texts: &[&str]) -> Vec<(Line<'static>, Option<u64>)> {
        texts
            .iter()
            .map(|text| (Line::from(Span::raw(text.to_string())), None))
            .collect()
    }

    fn with_sidebar(width: u16, hidden: bool) -> RenderInfo {
        let model = Model {
            intro: false,
            sidebar_hidden: hidden,
            sidebar: Some(vec![("Model".into(), "orvix/auto".into())]),
            cards: vec![Card {
                id: 1,
                kind: CardKind::User,
                title: "User".into(),
                body: "hi".into(),
                status: None,
                collapsed: false,
            }],
            ..Default::default()
        };
        let backend = ratatui::backend::TestBackend::new(width, 14);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let mut info = RenderInfo::default();
        terminal
            .draw(|frame| {
                info = render(frame, &model);
            })
            .expect("draw");
        info
    }

    #[test]
    fn the_sidebar_narrows_before_it_disappears() {
        // Dropping the rail outright at 84 columns took the session, model, and mode readout
        // with it and said nothing about why.
        assert_eq!(
            with_sidebar(100, false).transcript_width,
            70,
            "wide: full 30-column rail"
        );
        assert_eq!(
            with_sidebar(76, false).transcript_width,
            52,
            "tight: narrowed to 24"
        );
        assert_eq!(
            with_sidebar(60, false).transcript_width,
            60,
            "too narrow for any rail: the transcript takes the whole width"
        );
    }

    #[test]
    fn hiding_the_sidebar_gives_its_columns_to_the_transcript() {
        assert_eq!(with_sidebar(100, true).transcript_width, 100);
    }
    #[test]
    fn sidebar_groups_render_section_rules_and_a_usage_bar() {
        let model = Model {
            intro: false,
            sidebar: Some(vec![
                ("Session".into(), "hello\nid\ted1699fb".into()),
                (
                    "Runtime".into(),
                    "model\torvix/grok-4.6\nmode\tbuild\ngit\tmain · 13 changed".into(),
                ),
                (
                    "Context".into(),
                    "7550 tokens (5.9% of 128000)\nbar\t6".into(),
                ),
                ("Activity".into(), "tools\t3\nlat\t1.9s".into()),
            ]),
            ..Default::default()
        };
        let backend = ratatui::backend::TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| {
                render(frame, &model);
            })
            .expect("draw");
        let mut text = String::new();
        for row in 0..30 {
            for col in 70..100 {
                text.push_str(terminal.backend().buffer()[(col, row)].symbol());
            }
            text.push('\n');
        }
        for want in ["Session", "Runtime", "Context", "Activity", "░", "6%"] {
            assert!(text.contains(want), "rail shows {want:?}:\n{text}");
        }
    }

    #[test]
    fn a_steering_message_is_labelled_as_one() {
        let steer = Card {
            id: 1,
            kind: CardKind::User,
            title: "steering \u{2192} added to the running turn".into(),
            body: "actually use the other file".into(),
            status: None,
            collapsed: false,
        };
        let rows = rendered(&steer, 60);
        assert!(rows[0].contains("You"), "speaker first: {rows:?}");
        assert!(
            rows[1].contains("steering"),
            "the label heads the cell: {rows:?}"
        );
        assert!(rows[2].contains("actually use"), "{rows:?}");

        // An ordinary prompt still names the speaker.
        let plain = Card {
            id: 2,
            kind: CardKind::User,
            title: "User".into(),
            body: "hello".into(),
            status: None,
            collapsed: false,
        };
        let plain_rows = rendered(&plain, 60);
        assert_eq!(plain_rows.len(), 2, "You + body");
        assert!(plain_rows[0].contains("You"));
        assert!(plain_rows[1].contains("hello"));
    }

    #[test]
    fn search_matches_rows_case_insensitively() {
        let rows = rows_of(&["the Cargo build", "nothing here", "cargo test", "CARGO"]);
        assert_eq!(matching_rows(&rows, "cargo"), vec![0, 2, 3]);
        assert_eq!(
            matching_rows(&rows, "CARGO"),
            vec![0, 2, 3],
            "either case finds either"
        );
        assert!(matching_rows(&rows, "absent").is_empty());
        assert!(
            matching_rows(&rows, "").is_empty(),
            "an empty query matches nothing, not everything"
        );
    }

    #[test]
    fn search_matches_text_split_across_styled_spans() {
        // Rows are built from many spans (rail, header, body). Matching per span would miss a
        // word that straddles two of them.
        let line = Line::from(vec![
            Span::raw("\u{258c} ".to_string()),
            Span::raw("car".to_string()),
            Span::raw("go build".to_string()),
        ]);
        let rows = vec![(line, None)];
        assert_eq!(matching_rows(&rows, "cargo build"), vec![0]);
    }

    #[test]
    fn scrolling_to_a_match_centres_it_and_stays_in_range() {
        // 100 rows, a 20-row viewport. scroll_from_bottom counts up from the live tail.
        assert_eq!(
            scroll_to_row(100, 20, 90),
            0,
            "a match in the tail needs no scrolling"
        );
        assert_eq!(
            scroll_to_row(100, 20, 50),
            40,
            "40 rows back puts row 50 mid-viewport"
        );
        assert_eq!(
            scroll_to_row(100, 20, 0),
            80,
            "the first row scrolls all the way back"
        );
        // Fewer rows than fit: nothing to scroll, and no underflow.
        assert_eq!(scroll_to_row(5, 20, 3), 0);
        assert_eq!(scroll_to_row(0, 20, 0), 0);
    }

    #[test]
    fn highlighting_a_row_repaints_it_without_losing_its_text() {
        let line = Line::from(vec![
            Span::styled("\u{258c} ".to_string(), Style::default().fg(BLUE())),
            Span::styled("hit".to_string(), Style::default().fg(TEXT())),
        ]);
        let painted = highlight_row(line, MATCH_BG());
        assert_eq!(row_text(&painted), "\u{258c} hit", "text survives");
        for span in &painted.spans {
            assert_eq!(
                span.style.bg,
                Some(MATCH_BG()),
                "every span carries the wash"
            );
        }
        // Foreground styling is left alone, so a highlighted answer still reads as an answer.
        assert_eq!(painted.spans[0].style.fg, Some(BLUE()));
        assert_eq!(painted.spans[1].style.fg, Some(TEXT()));
    }

    #[test]
    fn caret_motion_walks_characters_words_and_lines() {
        let buf = "hello brave world";
        assert_eq!(prev_char_boundary(buf, 5), 4);
        assert_eq!(prev_char_boundary(buf, 0), 0, "start of buffer is a floor");
        assert_eq!(next_char_boundary(buf, 16), 17);
        assert_eq!(
            next_char_boundary(buf, 17),
            17,
            "end of buffer is a ceiling"
        );

        // Alt+Left from the end lands at the start of the last word, and again at the one
        // before it -- the run of spaces is skipped, not counted as a word.
        assert_eq!(prev_word_boundary(buf, buf.len()), 12);
        assert_eq!(prev_word_boundary(buf, 12), 6);
        assert_eq!(prev_word_boundary(buf, 0), 0);
        assert_eq!(next_word_boundary(buf, 0), 5);
        assert_eq!(next_word_boundary(buf, buf.len()), buf.len());
    }

    #[test]
    fn caret_motion_is_utf8_safe() {
        // Byte stepping would slice a multi-byte char in half and panic.
        let buf = "haló界";
        let mut caret = buf.len();
        let mut steps = 0;
        while caret > 0 {
            caret = prev_char_boundary(buf, caret);
            steps += 1;
            assert!(
                buf.is_char_boundary(caret),
                "landed mid-character at {caret}"
            );
        }
        assert_eq!(steps, 5, "five characters, not eight bytes");
    }

    #[test]
    fn home_and_end_act_on_the_buffer_line_under_the_caret() {
        let buf = "first\nsecond\nthird";
        let caret = buf.find("second").unwrap() + 2;
        assert_eq!(line_start(buf, caret), buf.find("second").unwrap());
        assert_eq!(line_end(buf, caret), buf.find("second").unwrap() + 6);
        assert_eq!(
            line_start(buf, 2),
            0,
            "first line starts at the buffer start"
        );
        assert_eq!(line_end(buf, buf.len()), buf.len());
    }

    #[test]
    fn up_and_down_move_between_buffer_lines_then_stop() {
        let buf = "first\nsecond\nthird";
        let on_second = buf.find("second").unwrap() + 3; // 'o' of second, col 3
        let up = line_up(buf, on_second).expect("not on first line");
        assert_eq!(&buf[line_start(buf, up)..line_end(buf, up)], "first");
        assert_eq!(line_display_col(buf, up), 3);
        assert!(line_up(buf, 2).is_none(), "first line yields to history");
        let down = line_down(buf, on_second).expect("not on last line");
        assert_eq!(&buf[line_start(buf, down)..line_end(buf, down)], "third");
        assert!(line_down(buf, buf.len()).is_none());
    }

    #[test]
    fn a_long_line_scrolls_to_keep_the_caret_visible() {
        // Editing in the middle of a line longer than the box must not push the caret off it.
        let buf = "x".repeat(200);
        let view = editor_view(&buf, 120, 40);
        assert_eq!(view.rows.len(), 1);
        assert!(
            view.caret_col < 40,
            "caret stays inside the box: {}",
            view.caret_col
        );
        assert!(view.rows[0].chars().count() <= 40, "row fits the box");

        // At the very start the window shows the head, with the caret at column 0.
        let head = editor_view(&buf, 0, 40);
        assert_eq!(head.caret_col, 0);
        assert!(head.rows[0].starts_with('x'));
    }

    #[test]
    fn the_view_scrolls_up_to_reach_a_caret_on_an_earlier_line() {
        // Eight lines with the caret on the first: the newest-lines window alone would leave
        // the caret off screen, so the window has to extend back to it.
        let buf = (1..=8)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let view = editor_view(&buf, 2, 40);
        assert_eq!(view.caret_row, 0, "the caret's line is the first shown");
        assert!(view.rows[0].contains("line 1"), "{:?}", view.rows);
        assert_eq!(view.caret_col, 2);
    }

    #[test]
    fn folded_tool_card_still_names_the_tool_and_its_outcome() {
        // The reported problem: a finished call collapsed to a bare "completed - 0ms - 332
        // chars" with no way to tell which tool it belonged to.
        let card = Card {
            id: 7,
            kind: CardKind::Tool,
            title: tool_header("read_file", r#"{"path": "src/main.rs"}"#),
            body: (1..=20)
                .map(|i| format!("line {i}"))
                .collect::<Vec<_>>()
                .join("\n"),
            status: Some(("completed \u{b7} 28.0s \u{b7} 142 chars".into(), true)),
            collapsed: true,
        };
        let rows = rendered(&card, 70);
        assert_eq!(rows.len(), 3, "header + outcome + fold hint: {rows:?}");
        assert!(
            rows[0].contains("read_file"),
            "header names the tool: {rows:?}"
        );
        assert!(
            rows[0].contains("src/main.rs"),
            "header peeks at args: {rows:?}"
        );
        assert!(
            rows[1].contains("completed"),
            "outcome stays visible: {rows:?}"
        );
        assert!(
            rows[1].contains('\u{2713}'),
            "outcome is marked as success: {rows:?}"
        );
        assert!(
            rows[2].contains("20 more line(s)"),
            "everything else folds: {rows:?}"
        );
    }

    #[test]
    fn failed_tool_card_marks_the_outcome_and_keeps_the_detail_foldable() {
        let card = Card {
            id: 8,
            kind: CardKind::Tool,
            title: tool_header("run_command", r#"{"command": "cargo test"}"#),
            body: "thread 'main' panicked\nstack backtrace follows".into(),
            status: Some(("failed \u{b7} 1.2s".into(), false)),
            collapsed: true,
        };
        let rows = rendered(&card, 70);
        assert!(rows[1].contains('\u{2717}'), "failure is marked: {rows:?}");
        assert!(
            rows[1].contains("failed"),
            "outcome says it failed: {rows:?}"
        );
        assert!(
            rows.iter().all(|row| !row.contains("panicked")),
            "detail stays folded until asked for: {rows:?}"
        );
    }

    #[test]
    fn unfolding_a_card_reveals_the_body_under_the_outcome() {
        let mut card = Card {
            id: 9,
            kind: CardKind::Tool,
            title: tool_header("grep", "pattern"),
            body: "hit one\nhit two".into(),
            status: Some(("completed \u{b7} 3ms \u{b7} 15 chars".into(), true)),
            collapsed: false,
        };
        let rows = rendered(&card, 70);
        assert!(rows.iter().any(|row| row.contains("hit one")), "{rows:?}");
        assert!(rows.iter().any(|row| row.contains("hit two")), "{rows:?}");
        card.collapsed = true;
        assert!(
            rendered(&card, 70)
                .iter()
                .all(|row| !row.contains("hit one")),
            "folding hides the body again"
        );
    }

    #[test]
    fn drawn_rows_map_back_to_their_card_for_click_targeting() {
        let model = Model {
            intro: false,
            cards: vec![Card {
                id: 42,
                kind: CardKind::Tool,
                title: tool_header("glob", "**/*.rs"),
                body: (1..=6)
                    .map(|i| format!("file {i}.rs"))
                    .collect::<Vec<_>>()
                    .join("\n"),
                status: Some(("completed \u{b7} 9ms \u{b7} 60 chars".into(), true)),
                collapsed: true,
            }],
            ..Default::default()
        };
        let backend = ratatui::backend::TestBackend::new(60, 12);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let mut info = RenderInfo::default();
        terminal
            .draw(|frame| {
                info = render(frame, &model);
            })
            .expect("draw");
        assert!(!info.card_rows.is_empty(), "card rows are recorded");
        assert!(
            info.card_rows.iter().all(|(_, id)| *id == 42),
            "every recorded row belongs to the only card"
        );
        // A card with nothing folded away is not a click target.
        let plain = Model {
            intro: false,
            cards: vec![Card {
                id: 43,
                kind: CardKind::User,
                title: "User".into(),
                body: String::new(),
                status: None,
                collapsed: false,
            }],
            ..Default::default()
        };
        terminal
            .draw(|frame| {
                info = render(frame, &plain);
            })
            .expect("draw");
        assert!(
            info.card_rows.is_empty(),
            "nothing to expand, nothing to click"
        );
    }

    #[test]
    fn autocomplete_rows_are_bounded_click_targets() {
        let model = Model {
            intro: false,
            input: "/m".into(),
            input_caret: 2,
            ac_items: vec![
                ("model".into(), "switch model".into()),
                ("memory".into(), "list memory".into()),
            ],
            ..Default::default()
        };
        let backend = ratatui::backend::TestBackend::new(60, 14);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let mut info = RenderInfo::default();
        terminal
            .draw(|frame| info = render(frame, &model))
            .expect("draw");
        let second = info
            .hits
            .iter()
            .find(|hit| hit.target == HitTarget::Autocomplete(1))
            .expect("second completion");
        assert_eq!(
            hit_at(&info.hits, second.area.x, second.area.y),
            Some(HitTarget::Autocomplete(1))
        );
        assert_ne!(
            hit_at(&info.hits, second.area.right(), second.area.y),
            Some(HitTarget::Autocomplete(1)),
            "right edge is outside the bounded row"
        );
    }

    #[test]
    fn dialog_overlay_prevents_transcript_click_through() {
        let model = Model {
            intro: false,
            cards: vec![Card {
                id: 42,
                kind: CardKind::Tool,
                title: "tool".into(),
                body: "one\ntwo".into(),
                status: None,
                collapsed: true,
            }],
            dialog: Some(DialogState::new(
                "Pick",
                "/model ",
                vec![("fast".into(), "Fast".into())],
            )),
            ..Default::default()
        };
        let backend = ratatui::backend::TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let mut info = RenderInfo::default();
        terminal
            .draw(|frame| info = render(frame, &model))
            .expect("draw");
        let card = info
            .hits
            .iter()
            .find(|hit| hit.target == HitTarget::Card(42))
            .expect("card row");
        assert_eq!(
            hit_at(&info.hits, card.area.x, card.area.y),
            Some(HitTarget::Overlay)
        );
        let dialog = info
            .hits
            .iter()
            .find(|hit| hit.target == HitTarget::Dialog(0))
            .expect("dialog row");
        assert_eq!(
            hit_at(&info.hits, dialog.area.x, dialog.area.y),
            Some(HitTarget::Dialog(0))
        );
    }

    #[test]
    fn caret_lands_on_the_row_that_holds_the_typed_text() {
        // Reproduces the slash-menu report: with `/st` typed the caret sat one row below the
        // text and one column short of its end, because the block draws no top border and the
        // "\u{276f} " prefix is two cells wide.
        let model = Model {
            intro: false,
            input: "/st".into(),
            input_caret: 3,
            ac_items: vec![
                ("stats".into(), "Show current session usage".into()),
                ("status".into(), "Show project and connection status".into()),
            ],
            ..Default::default()
        };
        let ((x, y), row) = caret_and_row(&model);
        assert!(
            row.starts_with("\u{2502}\u{276f} /st"),
            "caret row holds the buffer: {row:?}"
        );
        let buffer_end = ("\u{2502}\u{276f} /st".chars().count()) as u16;
        assert_eq!(
            x, buffer_end,
            "caret sits immediately after the last character"
        );
        // And the cell under the caret is still empty, i.e. it did not land on a glyph.
        let backend = ratatui::backend::TestBackend::new(60, 12);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| {
                render(frame, &model);
            })
            .expect("draw");
        assert_eq!(terminal.backend().buffer()[(x, y)].symbol(), " ");
    }

    #[test]
    fn caret_follows_the_last_line_of_a_multiline_buffer() {
        let model = Model {
            intro: false,
            input: "first\nsecond".into(),
            input_caret: "first\nsecond".len(),
            ..Default::default()
        };
        let ((x, y), row) = caret_and_row(&model);
        assert!(
            row.starts_with("\u{2502}  second"),
            "caret row is the last segment: {row:?}"
        );
        assert_eq!(x, "\u{2502}  second".chars().count() as u16);
        // The earlier segment keeps its own row rather than being joined onto this one.
        let backend = ratatui::backend::TestBackend::new(60, 12);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| {
                render(frame, &model);
            })
            .expect("draw");
        let above: String = (0..60)
            .map(|col| terminal.backend().buffer()[(col, y - 1)].symbol())
            .collect();
        assert!(
            above.starts_with("\u{2502}\u{276f} first"),
            "previous segment on its own row: {above:?}"
        );
    }

    #[test]
    fn caret_stays_inside_the_editor_when_the_buffer_ends_with_a_newline() {
        let model = Model {
            intro: false,
            input: "done\n".into(),
            input_caret: "done\n".len(),
            ..Default::default()
        };
        let ((x, y), row) = caret_and_row(&model);
        assert_eq!(
            x, 3,
            "caret rests on the continuation indent of the empty last line"
        );
        assert!(
            row.trim_start_matches('\u{2502}').trim().is_empty(),
            "last row is the empty segment: {row:?}"
        );
        // Height is derived from the same split, so the caret cannot fall past the editor.
        assert!(y < 12);
    }

    #[test]
    fn wrapping_uses_display_width() {
        let rows = wrap_display("abc界def", 5);
        assert_eq!(rows, vec!["abc界", "def"]);
    }

    #[test]
    fn user_cards_use_thick_left_border() {
        let card = Card {
            id: 1,
            kind: CardKind::User,
            title: "User".into(),
            body: "hello".into(),
            status: None,
            collapsed: false,
        };
        let lines = card_lines(&card, 20);
        assert_eq!(lines.len(), 2, "speaker label then body");
        assert_eq!(lines[0].spans[0].content, "\u{258c} ");
        assert_eq!(lines[0].spans[0].style.fg, Some(ACCENT()));
        let label: String = lines[0]
            .spans
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert!(label.contains("You"), "{label:?}");
    }

    #[test]
    fn a_long_approval_body_scrolls_instead_of_being_cut() {
        // A patch diff is routinely longer than the modal. The body was cut at ten rows with
        // nothing saying so, which is being asked to authorise a change you cannot finish
        // reading.
        let body = (1..=40)
            .map(|i| format!("+ line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let perm = PermissionState {
            title: "Allow patch_file?".into(),
            body,
            selected: 0,
            scroll: 0,
            options: PERM_OPTIONS.to_vec(),
        };
        let backend = ratatui::backend::TestBackend::new(80, 30);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_permission(frame, &perm, area);
            })
            .expect("draw");
        let screen: String = {
            let buffer = terminal.backend().buffer().clone();
            (0..30)
                .map(|y| {
                    (0..80)
                        .map(|x| buffer[(x, y)].symbol().to_string())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        assert!(
            screen.contains("of 40 lines"),
            "the modal admits what it hid:\n{screen}"
        );
        assert!(screen.contains("PgUp/PgDn"), "and says how to see the rest");
        assert!(
            screen.contains("y") && screen.contains("Allow once"),
            "hotkeys are visible:\n{screen}"
        );
        assert!(screen.contains("line 1"), "the body starts at the top");

        // Scrolled down, later lines come into view.
        let scrolled = PermissionState { scroll: 20, ..perm };
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_permission(frame, &scrolled, area);
            })
            .expect("draw");
        let buffer = terminal.backend().buffer().clone();
        let screen: String = (0..30)
            .map(|y| {
                (0..80)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            screen.contains("line 30"),
            "scrolling reaches later lines:\n{screen}"
        );
    }

    #[test]
    fn assistant_cards_render_unboxed() {
        let card = Card {
            id: 1,
            kind: CardKind::Output,
            title: "Assistant".into(),
            body: "hello **world**".into(),
            status: None,
            collapsed: false,
        };
        let lines = card_lines(&card, 40);
        assert!(!lines.is_empty());
        // Every line starts with the accent bar and carries no background fill.
        for line in &lines {
            assert_eq!(line.spans[0].content, "\u{258c} ");
            assert_eq!(line.spans[0].style.fg, Some(BLUE()));
            assert!(line.spans[0].style.bg.is_none());
        }
    }

    #[test]
    fn collapsed_tool_output_shows_head_window_and_hint() {
        let card = Card {
            id: 1,
            kind: CardKind::Output,
            title: "Tool Output".into(),
            body: (1..=15)
                .map(|i| format!("line {i}"))
                .collect::<Vec<_>>()
                .join("\n"),
            status: None,
            collapsed: true,
        };
        let lines = card_lines(&card, 60);
        assert_eq!(lines.len(), COLLAPSED_PEEK + 1);
        let last = lines.last().unwrap();
        let text: String = last.spans.iter().map(|s| s.content.to_string()).collect();
        assert!(text.contains("13 more line(s)"));
        assert!(text.contains("ctrl+o"), "fold hint names the key: {text:?}");
    }

    #[test]
    fn a_long_error_folds_to_a_headline_and_hint() {
        let body = "error decoding response body for url (https://aisurplus.io/v1/chat/completions): invalid type: null, expected a sequence";
        assert!(
            error_should_fold(body),
            "URL + decode noise is a dump, not a one-liner"
        );
        assert_eq!(
            error_headline(body),
            "provider returned an invalid response"
        );
        let card = Card {
            id: 1,
            kind: CardKind::Error,
            title: "Error".into(),
            body: body.into(),
            status: None,
            collapsed: true,
        };
        let rows = rendered(&card, 40);
        assert!(
            rows[0].contains("Error"),
            "the card is headed as an error: {rows:?}"
        );
        assert_eq!(
            rows[0].chars().next(),
            Some('\u{258c}'),
            "rail grammar: {rows:?}"
        );
        let joined = rows.join("\n");
        assert!(
            joined.contains("ctrl+o"),
            "folded detail names how to expand: {rows:?}"
        );
        assert!(
            joined.contains("provider returned"),
            "headline is human, not the reqwest sentence: {rows:?}"
        );
        assert!(
            rows.iter().all(|row| !row.contains("expected a sequence")),
            "the serde dump stays behind the fold: {rows:?}"
        );
        assert!(
            rows.iter().all(|row| !row.contains("https://")),
            "the URL is not the title: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains('\u{2026}')),
            "the expand hint carries an ellipsis: {rows:?}"
        );
    }

    #[test]
    fn a_short_error_stays_open() {
        assert!(!error_should_fold("file not found"));
        let card = Card {
            id: 1,
            kind: CardKind::Error,
            title: "Error".into(),
            body: "file not found".into(),
            status: None,
            collapsed: false,
        };
        let rows = rendered(&card, 40);
        assert!(rows.iter().any(|row| row.contains("Error")), "{rows:?}");
        assert!(
            rows.iter().any(|row| row.contains("file not found")),
            "{rows:?}"
        );
        assert!(
            rows.iter().all(|row| !row.contains("ctrl+o")),
            "nothing to expand: {rows:?}"
        );
    }

    #[test]
    fn an_expanded_error_wraps_instead_of_dumping_one_row() {
        let body = "error decoding response body for url (https://aisurplus.io/v1/chat/completions): invalid type: null, expected a sequence";
        let card = Card {
            id: 1,
            kind: CardKind::Error,
            title: "Error".into(),
            body: body.into(),
            status: None,
            collapsed: false,
        };
        let rows = rendered(&card, 40);
        assert!(rows.len() > 2, "the body wraps across rows: {rows:?}");
        for row in &rows {
            assert!(row.chars().count() <= 42, "no full-width dump row: {row:?}");
        }
    }

    #[test]
    fn the_default_footer_is_a_quiet_help_hint() {
        let model = Model {
            intro: false,
            cards: vec![Card {
                id: 1,
                kind: CardKind::User,
                title: "User".into(),
                body: "hi".into(),
                status: None,
                collapsed: false,
            }],
            ..Default::default()
        };
        let backend = ratatui::backend::TestBackend::new(60, 16);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| {
                render(frame, &model);
            })
            .expect("draw");
        let buffer = terminal.backend().buffer().clone();
        let footer: String = (0..60).map(|x| buffer[(x, 15)].symbol()).collect();
        assert!(
            footer.contains("? help"),
            "the idle footer points at help: {footer:?}"
        );
        assert!(
            footer.contains("Ctrl+K"),
            "cold-start keys are teased: {footer:?}"
        );
        assert!(
            !footer.contains("! shell") && !footer.contains("Ctrl+O expand"),
            "the keymap dump is gone: {footer:?}"
        );
    }

    #[test]
    fn a_short_transcript_sits_just_above_the_editor() {
        let model = Model {
            intro: false,
            cards: vec![Card {
                id: 1,
                kind: CardKind::User,
                title: "User".into(),
                body: "hello".into(),
                status: None,
                collapsed: false,
            }],
            ..Default::default()
        };
        let backend = ratatui::backend::TestBackend::new(60, 20);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let mut info = RenderInfo::default();
        terminal
            .draw(|frame| {
                info = render(frame, &model);
            })
            .expect("draw");
        let buffer = terminal.backend().buffer().clone();
        let rows: Vec<String> = (0..20)
            .map(|y| (0..60).map(|x| buffer[(x, y)].symbol()).collect())
            .collect();
        let speaker_y = rows
            .iter()
            .position(|row| row.contains("You"))
            .expect("speaker is drawn");
        let text_y = rows
            .iter()
            .position(|row| row.contains("hello"))
            .expect("message is drawn");
        let editor_y = rows
            .iter()
            .position(|row| row.contains('\u{276f}'))
            .expect("editor is drawn");
        assert!(
            speaker_y > 0,
            "a short transcript must not stick to the top: {rows:?}"
        );
        assert!(text_y < editor_y, "message sits above the editor");
        assert!(
            editor_y - speaker_y <= 4,
            "content sits just above the editor (speaker_y={speaker_y}, editor_y={editor_y}): {rows:?}"
        );
        assert!(
            info.card_rows.iter().all(|(y, _)| *y >= speaker_y as u16),
            "top pad rows are not click targets: {:?}",
            info.card_rows
        );
    }
}

#[cfg(test)]
mod wrap_tests {
    use super::*;

    #[test]
    fn wrap_spans_splits_on_newline() {
        let spans = vec![Span::raw("aaa\nbbb\nccc")];
        let rows = wrap_spans(&spans, 20);
        assert_eq!(rows.len(), 3, "rows: {rows:?}");
    }
}

#[cfg(test)]
mod line_tests {
    use super::*;

    /// ratatui 0.30 strips embedded newlines from Span content at construction. This is WHY
    /// multi-line notices are split into separate Lines in transcript_rows.
    #[test]
    fn line_styled_strips_embedded_newlines() {
        let line = Line::styled("a\nb\nc".to_string(), Style::default());
        let txt: String = line.spans.iter().map(|sp| sp.content.as_ref()).collect();
        assert_eq!(txt.matches('\n').count(), 0);
    }

    #[test]
    fn multiline_notices_become_separate_lines() {
        let model = Model {
            intro: false,
            cards: vec![Card {
                id: 1,
                kind: CardKind::Note,
                title: String::new(),
                body: "line one\nline two".to_string(),
                status: None,
                collapsed: false,
            }],
            ..Model::default()
        };
        let rendered: Vec<String> = transcript_rows(&model, 80)
            .iter()
            .map(|(line, _)| line.spans.iter().map(|sp| sp.content.as_ref()).collect())
            .collect();
        // Each source line becomes its own row (the leading span is the cell's rail).
        assert!(
            rendered.iter().any(|row| row.ends_with("line one")),
            "{rendered:?}"
        );
        assert!(
            rendered.iter().any(|row| row.ends_with("line two")),
            "{rendered:?}"
        );
    }
}
#[test]
fn clipboard_reference_inserts_at_caret_with_sensible_spacing() {
    let mut middle = "hello world".to_string();
    let caret = insert_clipboard_reference(&mut middle, 5);
    assert_eq!(middle, "hello @clipboard world");
    assert_eq!(caret, "hello @clipboard".len());

    let mut after_space = "ask ".to_string();
    assert_eq!(insert_clipboard_reference(&mut after_space, 4), 14);
    assert_eq!(after_space, "ask @clipboard");

    let mut unicode = "éx".to_string();
    let caret = insert_clipboard_reference(&mut unicode, 2);
    assert_eq!(unicode, "é @clipboard x");
    assert_eq!(caret, "é @clipboard".len());
}
