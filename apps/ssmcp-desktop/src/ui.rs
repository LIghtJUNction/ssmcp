//! Native drawing and explicit user actions. There is no network work here.
use crate::{
    engine::{Handle, Intent, RequestIds},
    model::*,
};
use serde_json::{Value, json};
use std::collections::VecDeque;
use unicode_width::UnicodeWidthChar;
use zgpui::{
    bridge::protocol::*,
    native::{ButtonStyle, Canvas, Event as NativeEvent, Rect, Ui},
};

pub use zgpui::native::colors::BG;
use zgpui::native::colors::{
    BLUE as ACCENT, BLUE_PALE as SELECTED, GREEN as SUCCESS, LINE, MUTED, RED, SURFACE, TEXT,
};
const SIDE: f32 = 256.;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Conversation,
    Events,
    Terminal,
    Workers,
    Receipts,
    Waits,
}

impl Page {
    fn index(self) -> usize {
        match self {
            Self::Conversation => 0,
            Self::Events => 1,
            Self::Terminal => 2,
            Self::Workers => 3,
            Self::Receipts => 4,
            Self::Waits => 5,
        }
    }
}

enum SidebarRow<'a> {
    Page(Page, &'static str),
    SessionsHeading,
    Filter,
    Session(usize, &'a Session),
    SessionPages(usize),
    Empty,
    Control,
    Disconnect,
    Theme,
    NamedSession,
    ObserveNamed,
    Setup,
    Update,
}

fn sidebar_link(
    canvas: &mut Canvas<'_>,
    ui: &mut Ui,
    id: u64,
    rect: Rect,
    label: &str,
    selected: bool,
    enabled: bool,
) -> bool {
    if selected {
        canvas.rect(rect, 10., SELECTED);
    }
    let clicked = ui.button(canvas, id, rect, "", ButtonStyle::Quiet, enabled);
    canvas.ellipsis(
        label,
        rect.x + 14.,
        rect.y + (rect.h - 19.) / 2.,
        rect.w - 28.,
        15,
        selected,
        if enabled { TEXT } else { MUTED },
    );
    clicked
}

struct Layout {
    sequence: Counter,
    page: usize,
    expanded: bool,
    lines: Vec<String>,
    more: bool,
    width: f32,
}

struct Pending {
    request_id: Id,
    command: String,
    generation: Id,
    connection: u64,
    target: Option<String>,
    draft: Option<String>,
}

struct SessionTab {
    target: Target,
    control: bool,
    draft: String,
    unknown: Option<String>,
}

pub struct Desktop {
    tabs: Vec<SessionTab>,
    selected: Option<usize>,
    next_session: Option<Target>,
    sidebar_open: bool,
    setup: bool,
    page: Page,
    session_filter: String,
    named_session: String,
    sidebar_settings: bool,
    sidebar_offset: usize,
    session_page: usize,
    sidebar_redraw: bool,
    toggle_theme: bool,
    composer: String,
    composer_editor: crate::composer::Composer,
    terminal_input: String,
    terminal_id: String,
    rows: String,
    cols: String,
    worker_total: String,
    worker_generation: String,
    local_notice: String,
    pending: Option<Pending>,
    unknown_draft: Option<String>,
    scroll: [f32; 6],
    wait_layout: Option<(Vec<String>, i32, Vec<String>)>,
    pointer: (f32, f32),
    expanded: Option<(usize, Counter)>,
    layouts: VecDeque<Layout>,
    layout_width: f32,
    selected_message: Option<(String, String)>,
    message_boxes: Vec<(Rect, String, String)>,
    reply_thread: ThreadChoice,
    thread_picker: bool,
    thread_scroll: usize,
    queue_view: bool,
    ids: RequestIds,
    generation: Option<Id>,
}

impl Default for Desktop {
    fn default() -> Self {
        Self {
            tabs: Vec::new(),
            selected: None,
            next_session: None,
            sidebar_open: false,
            setup: false,
            page: Page::Conversation,
            session_filter: String::new(),
            named_session: String::new(),
            sidebar_settings: false,
            sidebar_offset: 0,
            session_page: 0,
            sidebar_redraw: false,
            toggle_theme: false,
            composer: String::new(),
            composer_editor: crate::composer::Composer::default(),
            terminal_input: String::new(),
            terminal_id: String::new(),
            rows: "24".into(),
            cols: "80".into(),
            worker_total: String::new(),
            worker_generation: String::new(),
            local_notice: String::new(),
            pending: None,
            unknown_draft: None,
            scroll: [0.; 6],
            wait_layout: None,
            pointer: (0., 0.),
            expanded: None,
            layouts: VecDeque::new(),
            layout_width: 0.,
            selected_message: None,
            message_boxes: Vec::new(),
            reply_thread: ThreadChoice::default(),
            thread_picker: false,
            thread_scroll: 0,
            queue_view: false,
            ids: RequestIds::new("action"),
            generation: None,
        }
    }
}

impl Desktop {
    fn quote_message(&mut self, id: &str, text: &str) {
        match ssmcp::client::replies::quote_reply(id, text, &self.composer, 4096) {
            Ok(draft) => {
                self.composer = draft;
                self.page = Page::Conversation;
                self.composer_editor.focus_end(&self.composer);
                self.local_notice = "已引用原消息；在输入框补充回复，可编辑引用。".into();
            }
            Err(error) => self.local_notice = error,
        }
    }

    pub fn notice(&mut self, text: String) {
        self.local_notice = text;
    }

    pub fn take_sidebar_redraw(&mut self) -> bool {
        std::mem::take(&mut self.sidebar_redraw)
    }

    pub fn take_theme_toggle(&mut self) -> bool {
        std::mem::take(&mut self.toggle_theme)
    }

    pub fn take_setup(&mut self) -> bool {
        std::mem::take(&mut self.setup)
    }

    pub fn take_clipboard(&mut self) -> Option<String> {
        self.composer_editor.take_clipboard()
    }

    pub fn event(&mut self, event: &NativeEvent) -> bool {
        if self.sidebar_open {
            match event {
                NativeEvent::Key { code: 27, .. } => {
                    self.sidebar_open = false;
                    return true;
                }
                NativeEvent::Scroll(delta) if self.pointer.0 < SIDE => {
                    self.sidebar_offset = if *delta > 0 {
                        self.sidebar_offset.saturating_sub(2)
                    } else {
                        self.sidebar_offset.saturating_add(2)
                    };
                    return true;
                }
                _ => {}
            }
        }
        if matches!(
            event,
            NativeEvent::Key {
                code: 82 | 114,
                control: true,
                shift: true,
                ..
            }
        ) && self.page == Page::Conversation
            && self.pending.is_none()
        {
            if let Some((id, text)) = self.selected_message.clone() {
                self.quote_message(&id, &text);
            } else {
                self.local_notice = "先选择一条消息，再按 Ctrl+Shift+R 引用回复。".into();
            }
            return true;
        }
        if self.page == Page::Conversation
            && self
                .composer_editor
                .event(event, &mut self.composer, self.pending.is_none())
        {
            return true;
        }
        match event {
            NativeEvent::Down(x, y) => {
                self.pointer = (*x, *y);
                if let Some((_, id, text)) = self
                    .message_boxes
                    .iter()
                    .find(|(r, _, _)| *x >= r.x && *x < r.x + r.w && *y >= r.y && *y < r.y + r.h)
                {
                    self.selected_message = Some((id.clone(), text.clone()));
                }
            }
            NativeEvent::Move(x, y) | NativeEvent::Up(x, y) => self.pointer = (*x, *y),
            NativeEvent::Scroll(delta) if !self.sidebar_open || self.pointer.0 > SIDE => {
                if self.thread_picker {
                    self.thread_scroll = if *delta > 0 {
                        self.thread_scroll.saturating_sub(2)
                    } else {
                        self.thread_scroll.saturating_add(2)
                    };
                } else {
                    let direction = if matches!(self.page, Page::Conversation | Page::Events) {
                        1.
                    } else {
                        -1.
                    };
                    let next =
                        self.scroll[self.page.index()] + delta.signum() as f32 * 72. * direction;
                    self.scroll[self.page.index()] = next.max(0.);
                }
            }
            _ => {}
        }
        false
    }

    pub fn sync(&mut self, view: &View) {
        self.composer_editor
            .set_available_commands(|capability| view.command(capability));
        if let Some(target) = &view.target {
            let index = self
                .tabs
                .iter()
                .position(|tab| &tab.target == target)
                .unwrap_or_else(|| {
                    self.tabs.push(SessionTab {
                        target: target.clone(),
                        control: false,
                        draft: String::new(),
                        unknown: None,
                    });
                    self.tabs.len() - 1
                });
            if self.selected != Some(index) {
                if let Some(previous) = self.selected {
                    self.tabs[previous].draft = std::mem::take(&mut self.composer);
                    self.tabs[previous].unknown = self.unknown_draft.take();
                }
                self.selected = Some(index);
                self.composer = std::mem::take(&mut self.tabs[index].draft);
                self.unknown_draft = self.tabs[index].unknown.take();
                self.terminal_input.clear();
                self.page = Page::Conversation;
                self.layouts.clear();
            }
            if view.phase.connected() {
                self.tabs[index].control = view.phase == Phase::Controlling;
            }
        }
        if self.reply_thread != view.thread || self.generation != view.generation {
            self.selected_message = None;
            self.message_boxes.clear();
            self.reply_thread = view.thread.clone();
        }
        if view.generation.is_some() && self.generation != view.generation {
            self.generation = view.generation.clone();
            self.layouts.clear();
            self.scroll = [0.; 6];
            self.wait_layout = None;
            self.expanded = None;
            self.thread_picker = false;
        }
        if let Some(pending) = &self.pending {
            if let Some(receipt) = view.receipts.iter().find(|receipt| {
                receipt.request_id == pending.request_id
                    && receipt.generation == pending.generation
                    && receipt.connection == pending.connection
                    && receipt.command.as_str() == pending.command
                    && receipt.target == pending.target
                    && receipt.state != ReceiptState::Sending
            }) {
                let same_context = view.connection == pending.connection
                    && view.generation.as_ref() == Some(&pending.generation);
                if pending.command == "chat.send"
                    && same_context
                    && pending.target.as_deref() == Some(view.thread.input_target())
                    && pending.draft.as_ref() == Some(&self.composer)
                {
                    if receipt.state == ReceiptState::Confirmed {
                        self.composer.clear();
                        self.unknown_draft = None;
                    }
                    if receipt.state == ReceiptState::Unknown {
                        self.unknown_draft = pending.draft.clone();
                    }
                }
                if same_context && matches!(pending.command.as_str(), "session.cd" | "session.new")
                {
                    if receipt.state == ReceiptState::Confirmed {
                        if pending.draft.as_ref() == Some(&self.composer) {
                            self.composer.clear();
                        }
                        if let Some(target) = &receipt.new_session {
                            self.next_session = Some(Target::Named(target.clone()));
                        }
                    } else {
                        self.local_notice = receipt.detail.clone();
                        if receipt.state == ReceiptState::Unknown {
                            self.unknown_draft = pending.draft.clone();
                        }
                    }
                }
                if pending.command == "terminal.write"
                    && same_context
                    && pending.target.as_deref() == Some(self.terminal_id.as_str())
                    && pending.draft.as_ref() == Some(&self.terminal_input)
                    && receipt.state == ReceiptState::Confirmed
                {
                    self.terminal_input.clear();
                }
                self.pending = None;
            } else if !view.busy
                && !view.receipt_capacity()
                && !view
                    .receipts
                    .iter()
                    .any(|receipt| receipt.request_id == pending.request_id)
            {
                self.local_notice = "回执容量已满，新的操作未发送。请先核对未解决的结果。".into();
                self.pending = None;
            }
        }
        if !view
            .terminals
            .iter()
            .any(|terminal| terminal.id == self.terminal_id)
        {
            if let Some(terminal) = view
                .terminals
                .iter()
                .find(|terminal| terminal.selected)
                .or_else(|| view.terminals.first())
            {
                self.terminal_id = terminal.id.clone();
                self.rows = terminal.rows.to_string();
                self.cols = terminal.cols.to_string();
            }
        }
        if let Some(workers) = &view.workers {
            if self.worker_generation != workers.generation {
                self.worker_generation = workers.generation.clone();
                self.worker_total = workers.desired.to_string();
            }
        }
    }

    fn send(&mut self, handle: &Handle, intent: Intent) {
        match handle.send(intent) {
            Ok(()) => self.local_notice.clear(),
            Err(error) => self.local_notice = error.into(),
        }
    }

    fn can_command(&self, view: &View, command: &str) -> bool {
        self.pending.is_none() && view.command(command)
    }

    fn command(&mut self, handle: &Handle, view: &View, command: &str, mut arguments: Value) {
        if !self.can_command(view, command) {
            return;
        }
        let Some(generation) = view.generation.clone() else {
            return;
        };
        let request_id = self.ids.next();
        if command == "chat.send" {
            arguments["request_id"] = json!(request_id);
        }
        let target = match command {
            "chat.send" => arguments["target"].as_str().or(Some("next")),
            "terminal.write" => arguments["terminal_id"].as_str(),
            _ => None,
        }
        .map(str::to_owned);
        let draft = match command {
            "session.new" if self.composer.trim() == "/new" => Some(self.composer.clone()),
            "session.cd" => Some(self.composer.clone()),
            "chat.send" => arguments["text"].as_str().map(str::to_owned),
            "terminal.write"
                if arguments["data"].as_str().is_some_and(|data| {
                    data == self.terminal_input || data == format!("{}\r", self.terminal_input)
                }) =>
            {
                Some(self.terminal_input.clone())
            }
            _ => None,
        };
        let pending = Pending {
            request_id: request_id.clone(),
            command: command.to_owned(),
            generation: generation.clone(),
            connection: view.connection,
            target,
            draft,
        };
        let request = Request {
            version: VERSION,
            id: request_id.clone(),
            operation: Operation::Command {
                generation,
                command: Id::new(command).expect("static command ID"),
                arguments,
            },
        };
        match handle.send(Intent::Command {
            connection: view.connection,
            request,
        }) {
            Ok(()) => {
                self.pending = Some(pending);
                self.local_notice.clear();
            }
            Err(error) => self.local_notice = error.into(),
        }
    }

    pub fn draw(
        &mut self,
        canvas: &mut Canvas<'_>,
        ui: &mut Ui,
        handle: &Handle,
        view: &View,
        width: f32,
        height: f32,
    ) {
        if self.page != Page::Conversation {
            self.composer_editor.blur();
        }
        if let Some(target) = self.next_session.take() {
            self.send(
                handle,
                Intent::Connect {
                    target,
                    control: true,
                },
            );
            return;
        }
        if ui.button(
            canvas,
            1,
            Rect::new(10., 12., 42., 34.),
            if self.sidebar_open { "×" } else { "☰" },
            ButtonStyle::Quiet,
            true,
        ) {
            self.sidebar_open = !self.sidebar_open;
            if self.sidebar_open {
                self.sidebar_settings = false;
                self.sidebar_offset = 0;
                self.composer_editor.blur();
            }
        }
        let mut x = 24.;
        if self.sidebar_open && width > 540. {
            self.sidebar(canvas, ui, handle, view, height);
            x += SIDE;
        } else if self.sidebar_open {
            self.sidebar(canvas, ui, handle, view, height);
            return;
        }
        let w = (width - x - 24.).max(1.);
        let start = if self.sidebar_open { x } else { x + 38. };
        let right = width - 20.;
        let max_tabs = (((right - start - 178.) / 94.).floor().max(1.)) as usize;
        let first = self
            .selected
            .unwrap_or(0)
            .saturating_add(1)
            .saturating_sub(max_tabs);
        let mut tab_x = start;
        for index in first..self.tabs.len().min(first + max_tabs) {
            let tab = &self.tabs[index];
            let title = format!("会话 {}", index + 1);
            if ui.button(
                canvas,
                6000 + index as u64,
                Rect::new(tab_x, 12., 88., 34.),
                &title,
                ButtonStyle::Quiet,
                self.pending.is_none() && !view.busy,
            ) && self.selected != Some(index)
            {
                self.send(
                    handle,
                    Intent::Connect {
                        target: tab.target.clone(),
                        control: tab.control,
                    },
                );
            }
            if self.selected == Some(index) {
                canvas.rect(Rect::new(tab_x + 14., 48., 60., 2.), 1., ACCENT);
            }
            tab_x += 94.;
        }
        if ui.button(
            canvas,
            4,
            Rect::new(tab_x, 12., 76., 34.),
            "新会话",
            ButtonStyle::Quiet,
            self.can_command(view, "session.new"),
        ) {
            self.command(handle, view, "session.new", json!({}));
        }
        let status = match view.phase {
            Phase::Controlling => "已连接",
            Phase::Observing => "只读",
            Phase::Connecting => "连接中",
            Phase::Closing => "断开中",
            Phase::Disconnected => "未连接",
        };
        let color = match view.phase {
            Phase::Controlling => ACCENT,
            Phase::Observing => SUCCESS,
            Phase::Connecting | Phase::Closing => MUTED,
            Phase::Disconnected => MUTED,
        };
        canvas.rect(Rect::new(right - 80., 26., 7., 7.), 3., color);
        if self.page == Page::Receipts
            && self.can_command(view, "client.update")
            && ui.button(
                canvas,
                110,
                Rect::new(right - 95., 60., 92., 27.),
                "安装更新",
                ButtonStyle::Quiet,
                true,
            )
        {
            self.command(handle, view, "client.update", json!({"check":false}));
        }
        canvas.ellipsis(status, right - 66., 21., 64., 12, false, MUTED);
        canvas.line((x, 54.), (right, 54.), 1., LINE);
        if !view.cwd.is_empty() {
            canvas.ellipsis(&view.cwd, x, 64., w, 12, false, MUTED);
        }
        let notice = if self.local_notice.is_empty() {
            &view.notice
        } else {
            &self.local_notice
        };
        let top = if notice.is_empty() {
            90.
        } else {
            canvas.ellipsis(notice, x, 90., w, 13, false, TEXT);
            120.
        };
        let bounds = Rect::new(x, top, w, (height - top - 12.).max(120.));
        match self.page {
            Page::Conversation => self.conversation(canvas, ui, handle, view, bounds),
            Page::Events => self.events(canvas, ui, view, bounds),
            Page::Terminal => self.terminal(canvas, ui, handle, view, bounds),
            Page::Workers => self.workers(canvas, ui, handle, view, bounds),
            Page::Receipts => self.receipts(canvas, ui, handle, view, bounds),
            Page::Waits => self.waits(canvas, view, bounds),
        }
        if self.thread_picker {
            self.threads(canvas, ui, handle, view, bounds);
        }
    }

    fn sidebar(
        &mut self,
        canvas: &mut Canvas<'_>,
        ui: &mut Ui,
        handle: &Handle,
        view: &View,
        height: f32,
    ) {
        const TOP: f32 = 62.;
        const ROW: f32 = 44.;
        let footer = (height - 54.).max(TOP);
        let visible = ((footer - TOP) / ROW).floor() as usize;
        let mut rows = Vec::new();
        if self.sidebar_settings {
            rows.extend([
                SidebarRow::Page(Page::Events, "事件"),
                SidebarRow::Page(Page::Receipts, "回执"),
                SidebarRow::Theme,
                SidebarRow::Control,
                SidebarRow::Disconnect,
            ]);
            if !view.window_sessions {
                rows.extend([SidebarRow::NamedSession, SidebarRow::ObserveNamed]);
            }
            rows.push(SidebarRow::Setup);
            if view.command("client.update") {
                rows.push(SidebarRow::Update);
            }
        } else {
            rows.push(SidebarRow::SessionsHeading);
            if view.sessions.len() > 4 || !self.session_filter.is_empty() {
                rows.push(SidebarRow::Filter);
            }
            let filter = self.session_filter.to_lowercase();
            let sessions: Vec<_> = view
                .sessions
                .iter()
                .enumerate()
                .filter(|(_, session)| session.name.to_lowercase().contains(&filter))
                .collect();
            let per_page = visible.saturating_sub(7).clamp(1, 5);
            let pages = sessions.len().div_ceil(per_page).max(1);
            self.session_page = self.session_page.min(pages - 1);
            if sessions.is_empty() {
                rows.push(SidebarRow::Empty);
            } else {
                rows.extend(
                    sessions
                        .into_iter()
                        .skip(self.session_page * per_page)
                        .take(per_page)
                        .map(|(index, session)| SidebarRow::Session(index, session)),
                );
            }
            if pages > 1 {
                rows.push(SidebarRow::SessionPages(pages));
            }
            rows.extend([
                SidebarRow::Page(Page::Conversation, "对话"),
                SidebarRow::Page(Page::Terminal, "终端"),
                SidebarRow::Page(Page::Workers, "Workers"),
                SidebarRow::Page(Page::Waits, "等待"),
            ]);
        }
        let limit = rows.len().saturating_sub(visible);
        self.sidebar_offset = self.sidebar_offset.min(limit);
        canvas.rect(
            Rect::new(0., 50., SIDE, (height - 50.).max(0.)),
            0.,
            SURFACE,
        );
        canvas.line((SIDE, 0.), (SIDE, height), 1., LINE);
        canvas.text(
            if self.sidebar_settings {
                "连接与设置"
            } else {
                "ssmcp"
            },
            62.,
            18.,
            17,
            true,
            TEXT,
        );
        for (index, row) in rows
            .into_iter()
            .skip(self.sidebar_offset)
            .take(visible)
            .enumerate()
        {
            let y = TOP + index as f32 * ROW;
            let rect = Rect::new(12., y, SIDE - 24., ROW - 2.);
            match row {
                SidebarRow::Page(page, label) => {
                    if sidebar_link(
                        canvas,
                        ui,
                        20 + page.index() as u64,
                        rect,
                        label,
                        self.page == page,
                        true,
                    ) {
                        self.page = page;
                        self.thread_picker = false;
                        self.sidebar_open = false;
                        self.sidebar_redraw = true;
                    }
                }
                SidebarRow::SessionsHeading => {
                    canvas.text(
                        if view.window_sessions {
                            "窗口会话"
                        } else {
                            "本地会话"
                        },
                        26.,
                        y + 15.,
                        12,
                        true,
                        MUTED,
                    );
                    if ui.button(
                        canvas,
                        101,
                        Rect::new(SIDE - 68., y + 3., 54., 36.),
                        "刷新",
                        ButtonStyle::Quiet,
                        !view.sessions_loading,
                    ) {
                        self.send(handle, Intent::RefreshSessions);
                        if view.phase.connected() {
                            self.send(handle, Intent::Refresh);
                        }
                    }
                }
                SidebarRow::Filter => {
                    if ui.input(
                        canvas,
                        102,
                        rect.inset(3.),
                        &mut self.session_filter,
                        "筛选会话",
                        false,
                        true,
                    ) {
                        self.session_page = 0;
                        self.sidebar_redraw = true;
                    }
                }
                SidebarRow::Session(session_index, session) => {
                    let selected = matches!(view.target.as_ref(), Some(Target::Named(name)) if name == &session.name);
                    if selected {
                        canvas.rect(rect, 10., SELECTED);
                    }
                    let enabled = self.pending.is_none() && !view.busy;
                    if ui.button(
                        canvas,
                        50_000 + session_index as u64,
                        rect,
                        "",
                        ButtonStyle::Quiet,
                        enabled,
                    ) {
                        self.named_session = session.name.clone();
                        self.layouts.clear();
                        self.send(
                            handle,
                            Intent::Connect {
                                target: Target::Named(session.name.clone()),
                                control: false,
                            },
                        );
                    }
                    canvas.ellipsis(
                        &session.name,
                        rect.x + 14.,
                        y + 3.,
                        rect.w - 28.,
                        14,
                        selected,
                        if enabled { TEXT } else { MUTED },
                    );
                    let state = if view.window_sessions {
                        "由本窗口创建"
                    } else {
                        match session.connection.as_str() {
                            "connected" => "已连接",
                            "disconnected" => "未连接",
                            _ => "状态待确认",
                        }
                    };
                    canvas.ellipsis(state, rect.x + 14., y + 23., rect.w - 28., 11, false, MUTED);
                }
                SidebarRow::SessionPages(pages) => {
                    let right = rect.x + rect.w;
                    if ui.button(
                        canvas,
                        51_100,
                        Rect::new(rect.x, y + 2., 40., 38.),
                        "‹",
                        ButtonStyle::Quiet,
                        self.session_page > 0,
                    ) {
                        self.session_page -= 1;
                        self.sidebar_redraw = true;
                    }
                    let label = format!("{} / {}", self.session_page + 1, pages);
                    let x = (SIDE - canvas.measure(&label, 12, false)) / 2.;
                    canvas.text(&label, x, y + 13., 12, false, MUTED);
                    if ui.button(
                        canvas,
                        51_101,
                        Rect::new(right - 40., y + 2., 40., 38.),
                        "›",
                        ButtonStyle::Quiet,
                        self.session_page + 1 < pages,
                    ) {
                        self.session_page += 1;
                        self.sidebar_redraw = true;
                    }
                }
                SidebarRow::Empty => {
                    let label = if view.sessions_loading {
                        "正在读取…"
                    } else if !self.session_filter.is_empty() {
                        "没有匹配的会话"
                    } else {
                        "暂无会话"
                    };
                    canvas.text(label, 26., y + 13., 13, false, MUTED);
                }
                SidebarRow::Theme => {
                    if sidebar_link(canvas, ui, 51_102, rect, "切换明暗", false, true) {
                        self.toggle_theme = true;
                        self.sidebar_redraw = true;
                    }
                }
                SidebarRow::Control => {
                    let controlling = view.phase == Phase::Controlling;
                    if sidebar_link(
                        canvas,
                        ui,
                        11,
                        rect,
                        if controlling {
                            "切回只读"
                        } else {
                            "申请控制"
                        },
                        false,
                        view.target.is_some() && self.pending.is_none() && !view.busy,
                    ) && let Some(target) = view.target.clone()
                    {
                        self.send(
                            handle,
                            Intent::Connect {
                                target,
                                control: !controlling,
                            },
                        );
                    }
                }
                SidebarRow::Disconnect => {
                    if sidebar_link(
                        canvas,
                        ui,
                        12,
                        rect,
                        "断开连接",
                        false,
                        view.phase.connected() && self.pending.is_none(),
                    ) {
                        self.send(handle, Intent::Disconnect);
                    }
                }
                SidebarRow::NamedSession => {
                    ui.input(
                        canvas,
                        105,
                        rect.inset(3.),
                        &mut self.named_session,
                        "输入已有会话名",
                        false,
                        true,
                    );
                }
                SidebarRow::ObserveNamed => {
                    if sidebar_link(
                        canvas,
                        ui,
                        106,
                        rect,
                        "观察连接",
                        false,
                        !self.named_session.trim().is_empty()
                            && self.named_session.len() <= 128
                            && self.pending.is_none(),
                    ) {
                        self.layouts.clear();
                        self.send(
                            handle,
                            Intent::Connect {
                                target: Target::Named(self.named_session.trim().to_owned()),
                                control: false,
                            },
                        );
                    }
                }
                SidebarRow::Setup => {
                    if sidebar_link(
                        canvas,
                        ui,
                        108,
                        rect,
                        "连接设置",
                        false,
                        self.pending.is_none(),
                    ) {
                        self.setup = true;
                    }
                }
                SidebarRow::Update => {
                    if sidebar_link(
                        canvas,
                        ui,
                        109,
                        rect,
                        "检查更新",
                        false,
                        self.pending.is_none(),
                    ) {
                        self.page = Page::Receipts;
                        self.command(handle, view, "client.update", json!({"check":true}));
                    }
                }
            }
        }
        canvas.line((24., footer), (SIDE - 24., footer), 1., LINE);
        let width = if limit > 0 { SIDE - 110. } else { SIDE - 24. };
        if sidebar_link(
            canvas,
            ui,
            107,
            Rect::new(12., footer + 5., width, 42.),
            if self.sidebar_settings {
                "‹  返回导航"
            } else {
                "连接与设置  ›"
            },
            false,
            true,
        ) {
            self.sidebar_settings = !self.sidebar_settings;
            self.sidebar_offset = 0;
            self.sidebar_redraw = true;
        }
        if limit > 0 {
            for (id, x, label, up) in [(103, SIDE - 94., "↑", true), (104, SIDE - 52., "↓", false)]
            {
                if ui.button(
                    canvas,
                    id,
                    Rect::new(x, footer + 5., 40., 42.),
                    label,
                    ButtonStyle::Quiet,
                    if up {
                        self.sidebar_offset > 0
                    } else {
                        self.sidebar_offset < limit
                    },
                ) {
                    self.sidebar_offset = if up {
                        self.sidebar_offset.saturating_sub(visible.max(1))
                    } else {
                        self.sidebar_offset
                            .saturating_add(visible.max(1))
                            .min(limit)
                    };
                    self.sidebar_redraw = true;
                }
            }
        }
    }

    fn conversation(
        &mut self,
        canvas: &mut Canvas<'_>,
        ui: &mut Ui,
        handle: &Handle,
        view: &View,
        bounds: Rect,
    ) {
        let title = match &view.thread {
            ThreadChoice::All => "全部来源",
            ThreadChoice::Main => "主会话",
            ThreadChoice::Agent(id) => {
                view.agents
                    .iter()
                    .find(|agent| &agent.id == id)
                    .map_or(id.as_str(), |agent| {
                        if agent.name.is_empty() {
                            &agent.id
                        } else {
                            &agent.name
                        }
                    })
            }
        };
        if ui.button(
            canvas,
            301,
            Rect::new(bounds.x + bounds.w - 176., bounds.y - 4., 176., 34.),
            title,
            ButtonStyle::Secondary,
            view.resource("threads"),
        ) {
            self.thread_picker = !self.thread_picker;
        }
        let suggestion_height =
            if self.pending.is_none() && self.composer_editor.has_suggestions(&self.composer) {
                crate::composer::SUGGESTION_HEIGHT
            } else {
                0.
            };
        let history_bounds = Rect::new(
            bounds.x,
            bounds.y + 36.,
            bounds.w,
            (bounds.h - 164. - suggestion_height).max(1.),
        );
        if view.resource("conversation") {
            self.history(canvas, ui, &view.conversation, history_bounds, 0);
        }
        let y = bounds.y + bounds.h - 110.;
        canvas.line(
            (bounds.x, y - 15.),
            (bounds.x + bounds.w, y - 15.),
            1.,
            LINE,
        );
        let exact_target = match &view.thread {
            ThreadChoice::Agent(id) => view.agents.iter().any(|agent| &agent.id == id),
            _ => true,
        };
        let verb = self.composer.split_whitespace().next();
        let can_send = match crate::composer::SLASH_COMMANDS
            .iter()
            .find(|command| Some(command.verb) == verb)
        {
            Some(command) => self.can_command(view, command.capability),
            None => {
                self.can_command(view, "chat.send")
                    && view.can_send
                    && !view.reloading
                    && exact_target
            }
        };
        if suggestion_height > 0. {
            self.composer_editor.draw_suggestions(
                canvas,
                ui,
                Rect::new(
                    bounds.x,
                    y - 14. - suggestion_height,
                    (bounds.w - 92.).max(1.),
                    suggestion_height - 3.,
                ),
                &self.composer,
            );
        }
        self.composer_editor.draw(
            canvas,
            ui,
            Rect::new(bounds.x, y, (bounds.w - 92.).max(1.), 64.),
            &self.composer,
            self.pending.is_none(),
            "输入消息或命令…",
        );
        let unknown_same = self.unknown_draft.as_ref() == Some(&self.composer);
        let can_submit =
            can_send && self.pending.is_none() && !self.composer.trim().is_empty() && !unknown_same;
        let clicked = ui.button(
            canvas,
            303,
            Rect::new(bounds.x + bounds.w - 80., y + 10., 80., 42.),
            "发送",
            ButtonStyle::Primary,
            can_submit,
        );
        let shortcut = self.composer_editor.take_submit();
        if can_submit && (clicked || shortcut) {
            let input = self.composer.trim();
            let (verb, arg) = input.split_once(char::is_whitespace).unwrap_or((input, ""));
            match (verb, arg.trim()) {
                ("/new", "") => self.command(handle, view, "session.new", json!({})),
                ("/new", _) => self.local_notice = "用法：/new".into(),
                ("/cd", "") => self.local_notice = "用法：/cd 目录".into(),
                ("/cd", path) => self.command(handle, view, "session.cd", json!({"path":path})),
                _ => self.command(
                    handle,
                    view,
                    "chat.send",
                    json!({"text":self.composer,"target":view.thread.input_target()}),
                ),
            }
        }
        let target = match &view.thread {
            ThreadChoice::Agent(id) => format!("目标线程：{id}"),
            _ => "Enter 换行 · Ctrl+Enter 发送".into(),
        };
        canvas.ellipsis(
            &target,
            bounds.x,
            y + 74.,
            bounds.w - 170.,
            12,
            false,
            MUTED,
        );
        canvas.text(
            &format!("{} / 4096 B", self.composer.len()),
            bounds.x + bounds.w - 108.,
            y + 74.,
            11,
            false,
            MUTED,
        );
        let hint = if unknown_same {
            "上次发送结果未知。草稿已保留；请先检查输入队列和原回执。"
        } else if !exact_target {
            "所选线程不在当前线程页中；请重新选择后发送。"
        } else if view.reloading {
            "会话正在重新连接，暂不能发送。"
        } else if !view.receipt_capacity() {
            "回执容量已满，请先核对未解决的操作结果。"
        } else if !view.command("chat.send") {
            "发送需要控制连接与当前会话实际公布的输入能力。"
        } else {
            ""
        };
        canvas.ellipsis(
            hint,
            bounds.x,
            y + 94.,
            bounds.w,
            12,
            false,
            if unknown_same { TEXT } else { MUTED },
        );
    }

    fn events(&mut self, canvas: &mut Canvas<'_>, ui: &mut Ui, view: &View, bounds: Rect) {
        canvas.text("会话事件", bounds.x, bounds.y, 19, true, TEXT);
        let cursor = view
            .cursor
            .map_or_else(|| "—".into(), |cursor| cursor.0.to_string());
        canvas.ellipsis(
            &format!("已应用游标 {cursor} · 缺口 {}", view.gaps),
            bounds.x + 140.,
            bounds.y + 5.,
            bounds.w - 140.,
            12,
            false,
            MUTED,
        );
        self.history(
            canvas,
            ui,
            &view.timeline,
            Rect::new(bounds.x, bounds.y + 45., bounds.w, bounds.h - 45.),
            1,
        );
    }

    fn history(
        &mut self,
        canvas: &mut Canvas<'_>,
        ui: &mut Ui,
        history: &History,
        bounds: Rect,
        page: usize,
    ) {
        if page == 0 {
            self.message_boxes.clear();
        }
        if history.rows.is_empty() {
            if page != 0 {
                canvas.text("暂无事件", bounds.x, bounds.y, 13, false, MUTED);
            }
            return;
        }
        if (self.layout_width - bounds.w).abs() > 1. {
            self.layouts.clear();
            self.layout_width = bounds.w;
        }
        let mut heights = Vec::with_capacity(history.rows.len());
        let mut layouts = Vec::with_capacity(history.rows.len());
        for row in &history.rows {
            let content_width = if row.input_side {
                bounds.w * 0.82
            } else {
                bounds.w
            };
            let expanded = self.expanded == Some((page, row.sequence));
            let existing = self.layouts.iter().find(|layout| {
                layout.page == page
                    && layout.sequence == row.sequence
                    && layout.expanded == expanded
                    && (layout.width - content_width).abs() <= 1.
            });
            let (lines, more) = if let Some(layout) = existing {
                (layout.lines.clone(), layout.more)
            } else {
                let (lines, more) = wrap(
                    canvas,
                    &row.text,
                    content_width - 40.,
                    15,
                    if expanded { 180 } else { 6 },
                );
                self.layouts.push_back(Layout {
                    sequence: row.sequence,
                    width: content_width,
                    page,
                    expanded,
                    lines: lines.clone(),
                    more,
                });
                while self.layouts.len() > 720 {
                    self.layouts.pop_front();
                }
                (lines, more)
            };
            let height = 50. + lines.len().max(1) as f32 * 23. + 30.;
            heights.push(height + 12.);
            layouts.push((lines, more));
        }
        let total: f32 = heights.iter().sum();
        let max_scroll = (total - bounds.h).max(0.);
        self.scroll[page] = self.scroll[page].clamp(0., max_scroll);
        let mut y = if total < bounds.h {
            bounds.y
        } else {
            bounds.y + bounds.h - total + self.scroll[page]
        };
        canvas.clip(bounds);
        for (index, row) in history.rows.iter().enumerate() {
            let h = heights[index] - 12.;
            let row_width = if row.input_side {
                bounds.w * 0.82
            } else {
                bounds.w
            };
            let row_x = bounds.x + bounds.w - row_width;
            if y + h >= bounds.y && y <= bounds.y + bounds.h {
                let card = Rect::new(row_x, y, row_width, h);
                let selected = row.message_id.as_ref().is_some_and(|id| {
                    self.selected_message
                        .as_ref()
                        .is_some_and(|(selected, _)| selected == id)
                });
                canvas.rect(card, 10., if selected { ACCENT } else { LINE });
                canvas.rect(
                    card.inset(1.),
                    9.,
                    if row.input_side { SELECTED } else { SURFACE },
                );
                if page == 0
                    && let Some(id) = &row.message_id
                {
                    let top = y.max(bounds.y);
                    let bottom = (y + h).min(bounds.y + bounds.h);
                    self.message_boxes.push((
                        Rect::new(row_x, top, row_width, bottom - top),
                        id.clone(),
                        row.text.clone(),
                    ));
                }
                let kind = match row.input_origin.as_deref() {
                    Some("human") => "用户",
                    Some("system_hook") => "系统提示",
                    Some("agent_timer") => "定时提示",
                    Some("worker_event") => "Worker 事件",
                    Some("maintenance") => "维护提示",
                    _ => kind_label(&row.kind),
                };
                canvas.text(
                    kind,
                    row_x + 18.,
                    y + 13.,
                    12,
                    true,
                    if row.kind == "answer" { ACCENT } else { TEXT },
                );
                canvas.ellipsis(
                    &format!("{}  {}", row.source, row.status),
                    row_x + 105.,
                    y + 13.,
                    row_width - 235.,
                    12,
                    false,
                    MUTED,
                );
                canvas.ellipsis(
                    &format!("#{}", row.sequence.0),
                    row_x + row_width - 126.,
                    y + 13.,
                    108.,
                    11,
                    false,
                    MUTED,
                );
                for (line_index, line) in layouts[index].0.iter().enumerate() {
                    canvas.text(
                        line,
                        row_x + 18.,
                        y + 41. + line_index as f32 * 23.,
                        15,
                        false,
                        TEXT,
                    );
                }
                let footer_y = y + h - 31.;
                if footer_y >= bounds.y && footer_y + 26. <= bounds.y + bounds.h {
                    if layouts[index].1 || self.expanded == Some((page, row.sequence)) {
                        if ui.button(
                            canvas,
                            10_000 + index as u64 * 2,
                            Rect::new(row_x + 10., footer_y, 64., 26.),
                            if self.expanded == Some((page, row.sequence)) {
                                "收起"
                            } else {
                                "展开"
                            },
                            ButtonStyle::Quiet,
                            true,
                        ) {
                            self.expanded = if self.expanded == Some((page, row.sequence)) {
                                None
                            } else {
                                Some((page, row.sequence))
                            };
                        }
                    }
                    if page == 0
                        && let Some(id) = &row.message_id
                    {
                        use std::hash::{Hash, Hasher};
                        let mut key = std::hash::DefaultHasher::new();
                        ("reply", row.sequence.0, id).hash(&mut key);
                        if ui.button(
                            canvas,
                            key.finish(),
                            Rect::new(row_x + row_width - 166., footer_y, 86., 26.),
                            "引用回复",
                            ButtonStyle::Quiet,
                            self.pending.is_none(),
                        ) {
                            self.selected_message = Some((id.clone(), row.text.clone()));
                            self.quote_message(id, &row.text);
                        }
                    }
                    if ui.button(
                        canvas,
                        10_001 + index as u64 * 2,
                        Rect::new(row_x + row_width - 70., footer_y, 60., 26.),
                        "复制",
                        ButtonStyle::Quiet,
                        true,
                    ) {
                        ui.clipboard = Some(row.text.clone());
                    }
                }
                if row.lossy {
                    canvas.text(
                        "已截断 / 有损文本",
                        row_x + row_width - 190.,
                        y + h - 20.,
                        10,
                        false,
                        TEXT,
                    );
                }
            }
            y += heights[index];
        }
        canvas.unclip();
        if self.scroll[page] > 0. {
            if ui.button(
                canvas,
                390 + page as u64,
                Rect::new(
                    bounds.x + bounds.w - 114.,
                    bounds.y + bounds.h - 41.,
                    106.,
                    32.,
                ),
                "回到最新",
                ButtonStyle::Secondary,
                true,
            ) {
                self.scroll[page] = 0.;
            }
        }
        if history.omitted > 0 {
            canvas.text(
                &format!("本窗口已省略 {} 条较早记录", history.omitted),
                bounds.x + 6.,
                bounds.y + bounds.h - 13.,
                10,
                false,
                MUTED,
            );
        }
    }

    fn terminal(
        &mut self,
        canvas: &mut Canvas<'_>,
        ui: &mut Ui,
        handle: &Handle,
        view: &View,
        bounds: Rect,
    ) {
        canvas.text("终端", bounds.x, bounds.y, 19, true, TEXT);
        let Some(terminal) = view
            .terminals
            .iter()
            .find(|terminal| terminal.id == self.terminal_id)
        else {
            self.empty(
                canvas,
                Rect::new(bounds.x, bounds.y + 48., bounds.w, bounds.h - 48.),
                "当前没有终端资源",
                "后端在线并公布 terminals 能力后，真实终端会出现在这里。",
            );
            return;
        };
        let selected = view
            .terminals
            .iter()
            .position(|item| item.id == terminal.id)
            .unwrap_or(0);
        if ui.button(
            canvas,
            401,
            Rect::new(bounds.x + 80., bounds.y - 5., 180., 34.),
            if terminal.name.is_empty() {
                &terminal.id
            } else {
                &terminal.name
            },
            ButtonStyle::Secondary,
            view.terminals.len() > 1,
        ) {
            let next = &view.terminals[(selected + 1) % view.terminals.len()];
            self.terminal_id = next.id.clone();
            self.rows = next.rows.to_string();
            self.cols = next.cols.to_string();
            self.scroll[2] = 0.;
        }
        let enabled = self.can_command(view, "terminal.write") && terminal.running;
        let mut bx = bounds.x + bounds.w - 222.;
        for (index, (label, data)) in [
            ("Ctrl+C", "\u{3}"),
            ("Tab", "\t"),
            ("↑", "\u{1b}[A"),
            ("↓", "\u{1b}[B"),
        ]
        .into_iter()
        .enumerate()
        {
            let width = if index == 0 { 78. } else { 44. };
            if ui.button(
                canvas,
                410 + index as u64,
                Rect::new(bx, bounds.y - 5., width, 34.),
                label,
                ButtonStyle::Quiet,
                enabled,
            ) {
                self.command(
                    handle,
                    view,
                    "terminal.write",
                    json!({"terminal_id":terminal.id,"data":data}),
                );
            }
            bx += width + 4.;
        }
        let screen = Rect::new(
            bounds.x,
            bounds.y + 46.,
            bounds.w,
            (bounds.h - 220.).max(160.),
        );
        canvas.rect(screen, 10., SURFACE);
        canvas.clip(screen.inset(15.));
        if terminal.screen.is_empty() {
            canvas.text(
                "从当前实时输出开始观察终端。",
                screen.x + 20.,
                screen.y + 21.,
                14,
                false,
                MUTED,
            );
        }
        let lines: Vec<_> = terminal.screen.lines().collect();
        let cell_w = canvas.measure("W", 14, false).max(8.);
        let visible = ((screen.h - 30.) / 20.).max(1.) as usize;
        let max_scroll = lines.len().saturating_sub(visible) as f32 * 20.;
        self.scroll[2] = self.scroll[2].clamp(0., max_scroll);
        let first = (self.scroll[2] / 20.) as usize;
        for (row, line) in lines.iter().skip(first).take(visible).enumerate() {
            let mut col = 0;
            let mut previous_col = 0;
            for ch in line.chars() {
                let cells = ch.width().unwrap_or(0);
                let x =
                    screen.x + 18. + if cells == 0 { previous_col } else { col } as f32 * cell_w;
                if x + cell_w > screen.x + screen.w - 12. {
                    break;
                }
                if ch != ' ' {
                    canvas.text(
                        &ch.to_string(),
                        x,
                        screen.y + 16. + row as f32 * 20.,
                        14,
                        false,
                        TEXT,
                    );
                }
                previous_col = col;
                col += cells;
            }
        }
        canvas.unclip();
        let y = screen.y + screen.h + 15.;
        ui.input(
            canvas,
            420,
            Rect::new(bounds.x, y, bounds.w - 242., 40.),
            &mut self.terminal_input,
            "向所选终端写入文字",
            false,
            self.pending.is_none(),
        );
        if ui.button(
            canvas,
            421,
            Rect::new(bounds.x + bounds.w - 232., y, 88., 40.),
            "仅写入",
            ButtonStyle::Secondary,
            enabled && !self.terminal_input.is_empty(),
        ) {
            self.command(
                handle,
                view,
                "terminal.write",
                json!({"terminal_id":terminal.id,"data":self.terminal_input}),
            );
        }
        if ui.button(
            canvas,
            422,
            Rect::new(bounds.x + bounds.w - 134., y, 134., 40.),
            "写入并回车",
            ButtonStyle::Primary,
            enabled && !self.terminal_input.is_empty(),
        ) {
            self.command(
                handle,
                view,
                "terminal.write",
                json!({"terminal_id":terminal.id,"data":format!("{}\r",self.terminal_input)}),
            );
        }
        let y = y + 55.;
        canvas.text("尺寸请求", bounds.x, y + 10., 12, false, MUTED);
        ui.input(
            canvas,
            423,
            Rect::new(bounds.x + 67., y, 66., 34.),
            &mut self.rows,
            "行",
            false,
            true,
        );
        canvas.text("×", bounds.x + 143., y + 10., 13, false, MUTED);
        ui.input(
            canvas,
            424,
            Rect::new(bounds.x + 164., y, 66., 34.),
            &mut self.cols,
            "列",
            false,
            true,
        );
        let size = self
            .rows
            .parse::<u16>()
            .ok()
            .zip(self.cols.parse::<u16>().ok())
            .filter(|(rows, cols)| *rows > 0 && *rows <= 1000 && *cols > 0 && *cols <= 1000);
        if ui.button(
            canvas,
            425,
            Rect::new(bounds.x + 243., y, 130., 34.),
            "调整所有终端",
            ButtonStyle::Secondary,
            self.can_command(view, "terminal.resize") && size.is_some(),
        ) {
            if let Some((rows, cols)) = size {
                self.command(
                    handle,
                    view,
                    "terminal.resize",
                    json!({"rows":rows,"cols":cols}),
                );
            }
        }
        if ui.button(
            canvas,
            426,
            Rect::new(bounds.x + bounds.w - 104., y, 104., 34.),
            "复制画面",
            ButtonStyle::Quiet,
            !terminal.screen.is_empty(),
        ) {
            ui.clipboard = Some(terminal.screen.clone());
        }
        canvas.ellipsis(
            if terminal.incomplete {
                "当前投影不完整：曾发生事件缺口，或终端尺寸超出本地 200 × 300 单元上限。"
            } else if !terminal.running {
                "终端进程已退出；当前保留最后观察到的画面。"
            } else if !terminal.dimensions_confirmed {
                "当前会话尚未提供终端尺寸。默认画布为 24 × 80，可调整所有终端。"
            } else {
                "实时 VT 文本投影 · 尺寸来自最近的调整回执 · 不补造连接前输出"
            },
            bounds.x,
            y + 49.,
            bounds.w,
            11,
            false,
            if terminal.incomplete { TEXT } else { MUTED },
        );
    }

    fn waits(&mut self, canvas: &mut Canvas<'_>, view: &View, bounds: Rect) {
        canvas.text("等待与计时", bounds.x, bounds.y, 19, true, TEXT);
        let Some(snapshot) = &view.time else {
            self.empty(
                canvas,
                Rect::new(bounds.x, bounds.y + 48., bounds.w, bounds.h - 48.),
                "等待数据尚不可用",
                "连接支持的后端后显示真实等待、读取状态与定时计划。",
            );
            return;
        };
        canvas.ellipsis(
            "最近一次实际快照 · 只读 · 刷新状态不会确认或重放结果",
            bounds.x,
            bounds.y + 34.,
            bounds.w,
            12,
            false,
            MUTED,
        );
        let body = Rect::new(bounds.x, bounds.y + 68., bounds.w, (bounds.h - 68.).max(1.));
        let width = body.w.max(1.) as i32;
        if self
            .wait_layout
            .as_ref()
            .is_none_or(|(source, previous_width, _)| {
                source != &snapshot.lines || *previous_width != width
            })
        {
            let (mut lines, more) = wrap(canvas, &snapshot.lines.join("\n"), body.w, 13, 16_384);
            if more {
                lines.push("显示内容已达到当前面板上限。".into());
            }
            self.wait_layout = Some((snapshot.lines.clone(), width, lines));
        }
        let lines = &self.wait_layout.as_ref().expect("prepared wait layout").2;
        let row_height = 24.;
        self.scroll[5] = self.scroll[5].min((lines.len() as f32 * row_height - body.h).max(0.));
        canvas.clip(body);
        for (index, line) in lines.iter().enumerate() {
            let y = body.y + index as f32 * row_height - self.scroll[5];
            if y + row_height < body.y || y > body.y + body.h {
                continue;
            }
            canvas.text(line, body.x, y, 13, false, TEXT);
        }
        canvas.unclip();
    }

    fn workers(
        &mut self,
        canvas: &mut Canvas<'_>,
        ui: &mut Ui,
        handle: &Handle,
        view: &View,
        bounds: Rect,
    ) {
        canvas.text("Workers", bounds.x, bounds.y, 19, true, TEXT);
        let Some(workers) = &view.workers else {
            self.empty(
                canvas,
                Rect::new(bounds.x, bounds.y + 48., bounds.w, bounds.h - 48.),
                "当前没有 worker 池",
                "会话配置并公布 workers 资源后，这里会显示实际站位与任务。",
            );
            return;
        };
        let metrics = [
            ("Dreaming", workers.dreaming),
            ("空闲", Some(workers.idle)),
            ("工作中", Some(workers.working)),
            ("离线", Some(workers.offline)),
            ("退役中", Some(workers.retiring)),
            ("总容量", Some(workers.total)),
            ("已登记", Some(workers.registered)),
        ];
        let chart_width = 208.;
        let side_by_side = workers.productivity.is_some() && bounds.w >= 600.;
        let metric_width = bounds.w - if side_by_side { chart_width + 16. } else { 0. };
        let columns = ((metric_width / 94.) as usize).clamp(2, metrics.len());
        let rows = metrics.len().div_ceil(columns);
        let mw = (metric_width - (columns - 1) as f32 * 10.) / columns as f32;
        for (index, (label, value)) in metrics.into_iter().enumerate() {
            let x = bounds.x + (index % columns) as f32 * (mw + 10.);
            let y = bounds.y + 43. + (index / columns) as f32 * 86.;
            canvas.rect(Rect::new(x, y, mw, 76.), 8., SURFACE);
            canvas.ellipsis(
                label,
                x + 12.,
                y + 12.,
                (mw - 24.).max(1.),
                11,
                false,
                MUTED,
            );
            canvas.text(
                &value.map_or_else(|| "—".into(), |count| count.to_string()),
                x + 12.,
                y + 37.,
                24,
                true,
                TEXT,
            );
        }
        let mut y = bounds.y + 136. + (rows - 1) as f32 * 86.;
        if let Some(calls) = workers.productivity {
            let (x, chart_y) = if side_by_side {
                (bounds.x + metric_width + 16., bounds.y + 43.)
            } else {
                let chart_y = y - 6.;
                y += 106.;
                (bounds.x, chart_y)
            };
            let width = chart_width.min(bounds.w);
            canvas.text(
                "Worker 工具调用 / 分钟（对数）",
                x,
                chart_y,
                11,
                false,
                MUTED,
            );
            let base = chart_y + 72.;
            let peak = calls.iter().copied().max().unwrap_or(0).max(1) as f64;
            let mut previous = None;
            for (index, count) in calls.into_iter().enumerate() {
                let point = (
                    x + index as f32 * (width - 4.) / 9.,
                    base - (48. * (count as f64).ln_1p() / peak.ln_1p()) as f32,
                );
                if let Some(last) = previous {
                    canvas.line(last, point, 1., ACCENT);
                }
                canvas.rect(Rect::new(point.0, point.1, 3., 3.), 1., ACCENT);
                previous = Some(point);
            }
            canvas.text("−10 分钟", x, base + 8., 10, false, MUTED);
            canvas.text("现在", x + width - 26., base + 8., 10, false, MUTED);
        }
        canvas.text("目标容量", bounds.x, y + 11., 13, false, MUTED);
        ui.input(
            canvas,
            501,
            Rect::new(bounds.x + 78., y, 74., 36.),
            &mut self.worker_total,
            "0–128",
            false,
            true,
        );
        let total = self
            .worker_total
            .parse::<u16>()
            .ok()
            .filter(|total| *total <= 128);
        if ui.button(
            canvas,
            502,
            Rect::new(bounds.x + 166., y, 108., 36.),
            "调整容量",
            ButtonStyle::Secondary,
            self.can_command(view, "workers.resize")
                && total.is_some()
                && !workers.closed
                && !workers.stopping,
        ) {
            self.command(
                handle,
                view,
                "workers.resize",
                json!({"pool_generation":workers.generation,"total":total}),
            );
        }
        let narrow = bounds.w < 520.;
        let (summary_x, summary_y) = if narrow {
            (bounds.x, y + 46.)
        } else {
            (bounds.x + 300., y + 12.)
        };
        canvas.ellipsis(
            &format!(
                "当前目标 {} · 排队任务 {}{}",
                workers.desired,
                workers.queued,
                if workers.stopping {
                    " · 正在停止"
                } else {
                    ""
                }
            ),
            summary_x,
            summary_y,
            (bounds.x + bounds.w - summary_x).max(1.),
            12,
            false,
            MUTED,
        );
        let list_y = if narrow {
            y + 75.
        } else {
            canvas.text("站位", bounds.x + 14., y + 63., 12, true, MUTED);
            canvas.text("状态", bounds.x + 104., y + 63., 12, true, MUTED);
            canvas.text("任务 / 进度", bounds.x + 262., y + 63., 12, true, MUTED);
            y + 88.
        };
        let body = Rect::new(
            bounds.x,
            list_y,
            bounds.w,
            (bounds.y + bounds.h - list_y - 24.).max(1.),
        );
        let row_height = if narrow { 136. } else { 86. };
        let stride = row_height + 8.;
        self.scroll[3] = self.scroll[3].min((workers.slots.len() as f32 * stride - body.h).max(0.));
        canvas.clip(body);
        for (index, worker) in workers.slots.iter().enumerate() {
            let y = body.y + index as f32 * stride - self.scroll[3];
            if y + row_height < body.y || y > body.y + body.h {
                continue;
            }
            canvas.rect(Rect::new(body.x, y, body.w, row_height), 8., SURFACE);
            canvas.text(
                &worker.station_label(),
                body.x + 14.,
                y + 12.,
                17,
                true,
                TEXT,
            );
            let (epoch_x, epoch_y, epoch_w) = if narrow {
                (body.x + 50., y + 16., 76.)
            } else {
                (body.x + 14., y + 45., 82.)
            };
            canvas.ellipsis(
                &format!("epoch {}", worker.epoch),
                epoch_x,
                epoch_y,
                epoch_w,
                10,
                false,
                MUTED,
            );
            let state_x = body.x + if narrow { 136. } else { 104. };
            canvas.ellipsis(
                worker_label(&worker.state),
                state_x,
                y + 14.,
                (body.x + body.w - state_x - 14.).clamp(1., 144.),
                13,
                true,
                if worker.state == "working" {
                    ACCENT
                } else if worker.registered {
                    SUCCESS
                } else {
                    MUTED
                },
            );
            let metadata_x = body.x + if narrow { 14. } else { 104. };
            canvas.text(
                worker.registration_label(),
                metadata_x,
                y + 37.,
                11,
                false,
                MUTED,
            );
            if worker.quiet_10m == Some(true) {
                canvas.text("10 分钟无工具调用", metadata_x, y + 59., 11, false, MUTED);
            }
            let (task_x, task_y) = if narrow {
                (body.x + 14., y + 85.)
            } else {
                (body.x + 262., y + 14.)
            };
            let task_width = (body.x + body.w - task_x - 14.).max(1.);
            canvas.ellipsis(
                if worker.task.is_empty() {
                    "暂无任务"
                } else {
                    &worker.task
                },
                task_x,
                task_y,
                task_width,
                13,
                false,
                TEXT,
            );
            canvas.ellipsis(
                &format!("{}  {}", worker.progress, worker.elapsed),
                task_x,
                task_y + 23.,
                task_width,
                12,
                false,
                MUTED,
            );
        }
        canvas.unclip();
        canvas.ellipsis(
            "缩容会等待真实 worker 释放站位；未登记的容量保留为离线。",
            bounds.x,
            bounds.y + bounds.h - 12.,
            bounds.w,
            11,
            false,
            MUTED,
        );
    }

    fn receipts(
        &mut self,
        canvas: &mut Canvas<'_>,
        ui: &mut Ui,
        handle: &Handle,
        view: &View,
        bounds: Rect,
    ) {
        canvas.text(
            if self.queue_view {
                "输入队列"
            } else {
                "操作回执"
            },
            bounds.x,
            bounds.y,
            19,
            true,
            TEXT,
        );
        if ui.button(
            canvas,
            601,
            Rect::new(bounds.x + bounds.w - 210., bounds.y - 5., 100., 34.),
            "操作回执",
            if self.queue_view {
                ButtonStyle::Quiet
            } else {
                ButtonStyle::Secondary
            },
            true,
        ) {
            self.queue_view = false;
            self.scroll[4] = 0.;
        }
        if ui.button(
            canvas,
            602,
            Rect::new(bounds.x + bounds.w - 102., bounds.y - 5., 102., 34.),
            "输入队列",
            if self.queue_view {
                ButtonStyle::Secondary
            } else {
                ButtonStyle::Quiet
            },
            view.resource("chat.queue"),
        ) {
            self.queue_view = true;
            self.scroll[4] = 0.;
        }
        let body = Rect::new(bounds.x, bounds.y + 48., bounds.w, bounds.h - 92.);
        if self.queue_view {
            if view.queue.is_empty() {
                self.empty(
                    canvas,
                    body,
                    "这一页没有输入记录",
                    "队列显示当前会话的消息投递进度。",
                );
            }
            self.scroll[4] = self.scroll[4].min((view.queue.len() as f32 * 122. - body.h).max(0.));
            canvas.clip(body);
            for (index, item) in view.queue.iter().enumerate() {
                let y = body.y + index as f32 * 122. - self.scroll[4];
                if y + 112. < body.y || y > body.y + body.h {
                    continue;
                }
                canvas.rect(Rect::new(body.x, y, body.w, 112.), 8., SURFACE);
                canvas.text(
                    &item.status,
                    body.x + 16.,
                    y + 14.,
                    13,
                    true,
                    if item.uncertain { MUTED } else { TEXT },
                );
                canvas.ellipsis(
                    &format!("目标 {} · {}", item.target, item.id),
                    body.x + 180.,
                    y + 14.,
                    body.w - 200.,
                    11,
                    false,
                    MUTED,
                );
                canvas.ellipsis(
                    &item.text,
                    body.x + 16.,
                    y + 44.,
                    body.w - 32.,
                    14,
                    false,
                    TEXT,
                );
                canvas.text(
                    if item.uncertain {
                        "结果未知，请核对原接收端；不会重发。"
                    } else if item.retarget_required {
                        "目标需要重新选择；客户端不会自动转发。"
                    } else {
                        "显示当前会话的投递状态。"
                    },
                    body.x + 16.,
                    y + 78.,
                    11,
                    false,
                    MUTED,
                );
            }
            canvas.unclip();
            if ui.button(
                canvas,
                603,
                Rect::new(bounds.x, bounds.y + bounds.h - 32., 86., 32.),
                "上一页",
                ButtonStyle::Quiet,
                view.queue_offset > 0,
            ) {
                self.send(
                    handle,
                    Intent::QueuePage(view.queue_offset.saturating_sub(16)),
                );
                self.scroll[4] = 0.;
            }
            if ui.button(
                canvas,
                604,
                Rect::new(bounds.x + 96., bounds.y + bounds.h - 32., 86., 32.),
                "下一页",
                ButtonStyle::Quiet,
                view.queue_next.is_some(),
            ) {
                if let Some(next) = view.queue_next {
                    self.send(handle, Intent::QueuePage(next));
                    self.scroll[4] = 0.;
                }
            }
        } else {
            if view.receipts.is_empty() {
                self.empty(
                    canvas,
                    body,
                    "还没有发出操作",
                    "只有明确点击的写入操作会产生回执。连接与观察不会执行任务。",
                );
            }
            self.scroll[4] =
                self.scroll[4].min((view.receipts.len() as f32 * 149. - body.h).max(0.));
            canvas.clip(body);
            for (index, receipt) in view.receipts.iter().enumerate() {
                let y = body.y + index as f32 * 149. - self.scroll[4];
                if y + 137. < body.y || y > body.y + body.h {
                    continue;
                }
                canvas.rect(Rect::new(body.x, y, body.w, 137.), 8., SURFACE);
                let color = match receipt.state {
                    ReceiptState::Unknown => TEXT,
                    ReceiptState::Rejected => RED,
                    ReceiptState::Confirmed => SUCCESS,
                    ReceiptState::Sending => ACCENT,
                };
                canvas.text(
                    receipt.state.label(),
                    body.x + 16.,
                    y + 13.,
                    13,
                    true,
                    color,
                );
                canvas.text(
                    receipt.command.as_str(),
                    body.x + 150.,
                    y + 13.,
                    13,
                    false,
                    TEXT,
                );
                canvas.ellipsis(
                    &receipt.detail,
                    body.x + 16.,
                    y + 43.,
                    body.w - 32.,
                    13,
                    false,
                    TEXT,
                );
                canvas.ellipsis(
                    &format!("请求 {}", receipt.request_id),
                    body.x + 16.,
                    y + 73.,
                    body.w - 112.,
                    11,
                    false,
                    MUTED,
                );
                canvas.ellipsis(
                    &format!("原代次 {}", receipt.generation),
                    body.x + 16.,
                    y + 100.,
                    body.w - 112.,
                    11,
                    false,
                    MUTED,
                );
                if y + 94. >= body.y && y + 127. <= body.y + body.h {
                    if ui.button(
                        canvas,
                        610 + index as u64,
                        Rect::new(body.x + body.w - 84., y + 94., 68., 30.),
                        "复制",
                        ButtonStyle::Quiet,
                        true,
                    ) {
                        ui.clipboard = Some(format!(
                            "{}\nrequest_id={}\ngeneration={}\n{}",
                            receipt.command, receipt.request_id, receipt.generation, receipt.detail
                        ));
                    }
                }
            }
            canvas.unclip();
            canvas.ellipsis(
                "最多 24 条回执 · 未解决结果持续保留 · 满额时暂停写入 · 关闭窗口后不持久化",
                bounds.x,
                bounds.y + bounds.h - 15.,
                bounds.w,
                11,
                false,
                MUTED,
            );
        }
    }

    fn threads(
        &mut self,
        canvas: &mut Canvas<'_>,
        ui: &mut Ui,
        handle: &Handle,
        view: &View,
        bounds: Rect,
    ) {
        let panel = Rect::new(
            bounds.x + bounds.w - 370.,
            bounds.y + 37.,
            370.,
            bounds.h.min(445.),
        );
        canvas.rect(
            Rect::new(panel.x - 3., panel.y - 3., panel.w + 6., panel.h + 6.),
            12.,
            LINE,
        );
        canvas.rect(panel, 10., SURFACE);
        canvas.text("选择对话来源", panel.x + 18., panel.y + 16., 16, true, TEXT);
        if ui.button(
            canvas,
            701,
            Rect::new(panel.x + panel.w - 54., panel.y + 7., 42., 32.),
            "×",
            ButtonStyle::Quiet,
            true,
        ) {
            self.thread_picker = false;
        }
        if ui.button(
            canvas,
            702,
            Rect::new(panel.x + 14., panel.y + 53., 162., 34.),
            "全部来源",
            ButtonStyle::Secondary,
            true,
        ) {
            self.send(handle, Intent::SelectThread(ThreadChoice::All));
            self.thread_picker = false;
            self.layouts.clear();
        }
        if ui.button(
            canvas,
            703,
            Rect::new(panel.x + 190., panel.y + 53., 162., 34.),
            "主会话",
            ButtonStyle::Secondary,
            true,
        ) {
            self.send(handle, Intent::SelectThread(ThreadChoice::Main));
            self.thread_picker = false;
            self.layouts.clear();
        }
        let visible = ((panel.h - 154.) / 53.).floor().max(1.) as usize;
        self.thread_scroll = self
            .thread_scroll
            .min(view.agents.len().saturating_sub(visible));
        for (index, agent) in view
            .agents
            .iter()
            .skip(self.thread_scroll)
            .take(visible)
            .enumerate()
        {
            let y = panel.y + 103. + index as f32 * 53.;
            if ui.button(
                canvas,
                720 + index as u64,
                Rect::new(panel.x + 12., y, 346., 30.),
                if agent.name.is_empty() {
                    &agent.id
                } else {
                    &agent.name
                },
                ButtonStyle::Quiet,
                true,
            ) {
                self.send(
                    handle,
                    Intent::SelectThread(ThreadChoice::Agent(agent.id.clone())),
                );
                self.thread_picker = false;
                self.layouts.clear();
            }
            canvas.ellipsis(
                &format!("{} · {}", agent.role, agent.status),
                panel.x + 25.,
                y + 31.,
                320.,
                10,
                false,
                MUTED,
            );
        }
        if ui.button(
            canvas,
            704,
            Rect::new(panel.x + 14., panel.y + panel.h - 42., 86., 30.),
            "上一页",
            ButtonStyle::Quiet,
            view.threads_offset > 0,
        ) {
            self.send(
                handle,
                Intent::ThreadsPage(view.threads_offset.saturating_sub(32)),
            );
            self.thread_scroll = 0;
        }
        if ui.button(
            canvas,
            705,
            Rect::new(panel.x + 270., panel.y + panel.h - 42., 86., 30.),
            "下一页",
            ButtonStyle::Quiet,
            view.threads_next.is_some(),
        ) {
            if let Some(next) = view.threads_next {
                self.send(handle, Intent::ThreadsPage(next));
                self.thread_scroll = 0;
            }
        }
        canvas.text(
            "滚轮浏览线程",
            panel.x + 127.,
            panel.y + panel.h - 32.,
            11,
            false,
            MUTED,
        );
    }

    fn empty(&self, canvas: &mut Canvas<'_>, bounds: Rect, title: &str, detail: &str) {
        let width = (bounds.w - 32.).clamp(280., 592.);
        let (lines, _) = wrap(canvas, detail, width - 104., 14, 4);
        let height = 96. + lines.len().max(1) as f32 * 23.;
        let x = bounds.x + (bounds.w - width) / 2.;
        let y = bounds.y + ((bounds.h - height) * 0.38).max(16.);
        let card = Rect::new(x, y, width, height);
        canvas.rect(card, 12., LINE);
        canvas.rect(card.inset(1.), 11., SURFACE);
        connection_mark(canvas, x + 24., y + 25., 30., MUTED);
        canvas.text(title, x + 78., y + 24., 19, false, TEXT);
        for (index, line) in lines.iter().enumerate() {
            canvas.text(
                line,
                x + 78.,
                y + 58. + index as f32 * 23.,
                14,
                false,
                MUTED,
            );
        }
    }
}

// Same seven rounded strokes as assets/ssmcp-desktop.svg and the Web mark.
fn connection_mark(canvas: &mut Canvas<'_>, x: f32, y: f32, size: f32, color: u32) {
    let scale = size / 32.;
    for (left, top, width, height) in [
        (5., 7., 2., 18.),
        (5., 7., 7., 2.),
        (5., 23., 7., 2.),
        (25., 7., 2., 18.),
        (20., 7., 7., 2.),
        (20., 23., 7., 2.),
        (11., 15., 10., 2.),
    ] {
        canvas.rect(
            Rect::new(
                x + left * scale,
                y + top * scale,
                width * scale,
                height * scale,
            ),
            scale,
            color,
        );
    }
}

fn kind_label(kind: &str) -> &str {
    match kind {
        "answer" => "回复",
        "progress" => "进展",
        "ssmcp.tool_call" => "工具调用",
        "ssmcp.user_input" => "用户输入",
        "ssmcp.backend" => "后端状态",
        "ssmcp.finish" => "会话结束",
        _ => kind,
    }
}

fn worker_label(state: &str) -> &str {
    match state {
        "dreaming" => "☾ Dreaming",
        "working" => "工作中",
        "standby" | "idle" => "待命",
        "offline" => "离线",
        "retiring" => "退役中",
        _ => state,
    }
}

/// UTF-8-safe, measured line breaks, with bounded work for collapsed records.
fn wrap(
    canvas: &Canvas<'_>,
    text: &str,
    width: f32,
    size: i32,
    maximum: usize,
) -> (Vec<String>, bool) {
    let mut output = Vec::new();
    let mut remaining = text;
    while !remaining.is_empty() && output.len() < maximum {
        let end = remaining.find('\n').unwrap_or(remaining.len());
        let paragraph = &remaining[..end];
        if paragraph.is_empty() {
            output.push(String::new());
            remaining = &remaining[end.saturating_add(1).min(remaining.len())..];
            continue;
        }
        let boundaries: Vec<_> = paragraph
            .char_indices()
            .map(|(index, _)| index)
            .chain(std::iter::once(paragraph.len()))
            .collect();
        let (mut low, mut high) = (1, boundaries.len() - 1);
        while low < high {
            let mid = (low + high).div_ceil(2);
            if canvas.measure(&paragraph[..boundaries[mid]], size, false) <= width {
                low = mid;
            } else {
                high = mid - 1;
            }
        }
        let mut take = boundaries[low];
        if take < paragraph.len() {
            if let Some(space) = paragraph[..take].rfind(' ') {
                if space > take / 2 {
                    take = space + 1;
                }
            }
        }
        output.push(paragraph[..take].trim_end().to_owned());
        let skip = take + usize::from(take == end && end < remaining.len());
        remaining = &remaining[skip..];
    }
    (output, !remaining.is_empty())
}

#[cfg(test)]
mod receipt_regressions {
    use super::*;

    fn confirmed(request_id: &str, command: &str) -> Receipt {
        Receipt {
            connection: 1,
            target: Some("agent-one".into()),
            request_id: Id::new(request_id).unwrap(),
            generation: Id::new("old-generation").unwrap(),
            command: Id::new(command).unwrap(),
            state: ReceiptState::Confirmed,
            detail: String::new(),
            new_session: None,
        }
    }

    fn pending(draft: &str) -> Pending {
        Pending {
            connection: 1,
            generation: Id::new("old-generation").unwrap(),
            request_id: Id::new("send-one").unwrap(),
            command: "chat.send".into(),
            target: Some("agent-one".into()),
            draft: Some(draft.into()),
        }
    }

    fn view() -> View {
        let mut view = View {
            connection: 1,
            generation: Some(Id::new("old-generation").unwrap()),
            thread: ThreadChoice::Agent("agent-one".into()),
            ..View::default()
        };
        view.record(confirmed("send-one", "chat.send"));
        view
    }

    #[test]
    fn another_generation_receipt_does_not_clear_the_current_draft() {
        let mut desktop = Desktop::default();
        desktop.composer = "keep this draft".into();
        desktop.pending = Some(pending("keep this draft"));
        let mut view = view();
        view.generation = Some(Id::new("new-generation").unwrap());
        desktop.sync(&view);
        assert_eq!(desktop.composer, "keep this draft");
    }

    #[test]
    fn a_receipt_never_clears_a_draft_edited_after_dispatch() {
        let mut desktop = Desktop::default();
        desktop.composer = "replacement draft".into();
        desktop.pending = Some(pending("original draft"));
        desktop.sync(&view());
        assert_eq!(desktop.composer, "replacement draft");
    }

    #[test]
    fn another_connection_or_target_preserves_the_draft() {
        for change_connection in [true, false] {
            let mut desktop = Desktop::default();
            desktop.composer = "original draft".into();
            desktop.pending = Some(pending("original draft"));
            let mut view = view();
            if change_connection {
                view.connection = 2;
            } else {
                view.thread = ThreadChoice::Agent("agent-two".into());
            }
            desktop.sync(&view);
            assert_eq!(desktop.composer, "original draft");
        }
    }

    #[test]
    fn matching_confirmed_receipt_clears_the_original_draft() {
        let mut desktop = Desktop::default();
        desktop.composer = "original draft".into();
        desktop.pending = Some(pending("original draft"));
        desktop.sync(&view());
        assert!(desktop.composer.is_empty());
        assert!(desktop.pending.is_none());
    }

    #[test]
    fn unknown_reply_preserves_the_original_draft_and_request() {
        let mut desktop = Desktop::default();
        desktop.composer = "original draft".into();
        desktop.pending = Some(pending("original draft"));
        let mut view = view();
        view.receipts[0].state = ReceiptState::Unknown;
        desktop.sync(&view);
        assert_eq!(desktop.composer, "original draft");
        assert_eq!(desktop.unknown_draft.as_deref(), Some("original draft"));
        assert_eq!(view.receipts[0].request_id.as_str(), "send-one");
        assert!(desktop.pending.is_none());
    }

    #[test]
    fn terminal_control_key_receipt_does_not_clear_unsent_text() {
        let mut desktop = Desktop::default();
        desktop.terminal_input = "unsent text".into();
        desktop.terminal_id = "terminal-one".into();
        desktop.pending = Some(Pending {
            connection: 1,
            generation: Id::new("old-generation").unwrap(),
            request_id: Id::new("control-key").unwrap(),
            command: "terminal.write".into(),
            target: Some("terminal-one".into()),
            draft: None,
        });
        let mut view = view();
        let mut receipt = confirmed("control-key", "terminal.write");
        receipt.target = Some("terminal-one".into());
        view.record(receipt);
        desktop.sync(&view);
        assert_eq!(desktop.terminal_input, "unsent text");
    }
}
