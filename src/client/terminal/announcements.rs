//! Session-local announcement banners and the full-screen conversation reader.
use super::*;

#[derive(Default)]
pub(super) struct Reader {
    // Slots follow the public account response: server first, then personal.
    entries: [Option<Entry>; 2],
    reading: Option<Page>,
    scroll: usize,
}

enum Page {
    Announcement(usize),
    Tool(String),
}

struct Entry {
    text: String,
    identity: String,
    dismissed: bool,
}

impl Reader {
    pub(super) fn is_reading(&self) -> bool {
        self.reading.is_some()
    }

    pub(super) fn visible(&self) -> impl Iterator<Item = (usize, &str)> {
        self.entries.iter().enumerate().filter_map(|(slot, entry)| {
            let entry = entry.as_ref()?;
            (!entry.dismissed).then_some((slot, entry.text.as_str()))
        })
    }
}

// The public API expands {time} before sending a greeting. Use the response's
// HTTP Date so a server clock different from the client's still works.
fn personal_identity(text: &str, seconds: u64) -> String {
    let mut identity = text.to_owned();
    if text.contains('T') && text.contains('Z') {
        // The account lookup and the HTTP Date can fall on adjacent seconds.
        for second in seconds.saturating_sub(3)..=seconds {
            if let Some(clock) = i64::try_from(second)
                .ok()
                .and_then(|n| n.checked_mul(1000))
                .and_then(|ms| crate::runtime::time::utc_at(ms).ok())
            {
                identity = identity.replace(&clock.replace(".000Z", "Z"), "{time}");
            }
        }
    }
    identity
}

impl State {
    pub(super) fn update_announcements(&mut self, lines: [Option<&str>; 2], seconds: u64) {
        let mut changed = false;
        for (slot, line) in lines.into_iter().enumerate() {
            let current = &mut self.reader.entries[slot];
            let text = line.map(panel::safe_text);
            if text.as_deref() == current.as_ref().map(|entry| entry.text.as_str()) {
                continue;
            }
            let updated = text.map(|text| {
                let identity = if slot == 1 {
                    personal_identity(&text, seconds)
                } else {
                    text.clone()
                };
                let dismissed = current
                    .as_ref()
                    .is_some_and(|entry| entry.identity == identity && entry.dismissed);
                Entry {
                    text,
                    identity,
                    dismissed,
                }
            });
            if matches!(self.reader.reading, Some(Page::Announcement(i)) if i == slot)
                && current.as_ref().map(|v| &v.identity) != updated.as_ref().map(|v| &v.identity)
            {
                self.reader.reading = None;
            }
            *current = updated;
            changed = true;
        }
        if changed {
            self.resize(self.geometry.rows, self.geometry.cols);
            self.dirty = true;
        }
    }

    pub(super) fn open_announcements(&mut self) -> Result<(), String> {
        let slot = self
            .reader
            .visible()
            .next()
            .map(|(slot, _)| slot)
            .or_else(|| self.reader.entries.iter().position(Option::is_some))
            .ok_or("No current announcements")?;
        self.reader.reading = Some(Page::Announcement(slot));
        self.reader.scroll = 0;
        self.dirty = true;
        Ok(())
    }

    pub(super) fn open_selected_tool(&mut self) {
        if let Some(text) = self.events.selected_tool_detail() {
            self.reader.reading = Some(Page::Tool(text));
            self.reader.scroll = 0;
        } else {
            self.notice = Some("Select an individual tool call or response to read".into());
        }
        self.dirty = true;
    }

    fn notice_row(&self, title: &str, width: u16, tone: PanelTone) -> String {
        if width < 12 {
            return self.chat_line(title, tone, width);
        }
        let title = fit(title, width - 4);
        let padding = usize::from(width - 3).saturating_sub(title.width());
        self.chat_line(&format!("{title}{}[×]", " ".repeat(padding)), tone, width)
    }

    fn dismiss_announcement(&mut self, slot: usize) {
        if let Some(entry) = &mut self.reader.entries[slot] {
            entry.dismissed = true;
        }
        self.reader.reading = None;
        self.resize(self.geometry.rows, self.geometry.cols);
        self.dirty = true;
    }

    pub(super) fn announcement_banner(&self, row: u16, width: u16) -> String {
        let text = self
            .reader
            .visible()
            .nth(usize::from(row))
            .map_or("", |(_, text)| text);
        self.notice_row(&format!(" ▸ {text}"), width, PanelTone::Muted)
    }

    pub(super) fn reader_key(&mut self, key: Key) -> bool {
        if self.reader.reading.is_none() {
            if let Key::Mouse {
                button: 0,
                x,
                y,
                release: false,
            } = key
                && !self.selection
                && x < self.geometry.left
                && (1..self.geometry.chat_body_top()).contains(&y)
            {
                let selected = self
                    .reader
                    .visible()
                    .nth(usize::from(y - 1))
                    .map(|(slot, _)| slot);
                if let Some(slot) = selected
                    && self.geometry.left >= 12
                    && x >= self.geometry.left - 3
                {
                    self.dismiss_announcement(slot);
                } else {
                    self.reader.reading = selected.map(Page::Announcement);
                    self.reader.scroll = 0;
                    self.dirty = true;
                }
                return true;
            }
            return false;
        }
        let page = usize::from(self.geometry.rows.saturating_sub(2).max(1));
        let scroll = &mut self.reader.scroll;
        match key {
            Key::Escape | Key::Interrupt => self.reader.reading = None,
            Key::Mouse {
                button: 0,
                x,
                y: 0,
                release: false,
            } if self.geometry.rows > 1
                && self.geometry.cols >= 12
                && x >= self.geometry.cols - 3 =>
            {
                self.reader.reading = None;
            }
            Key::Back | Key::Open => {
                if let Some(Page::Announcement(slot)) = self.reader.reading
                    && self.reader.entries[1 - slot].is_some()
                {
                    let other = 1 - slot;
                    self.reader.reading = Some(Page::Announcement(other));
                    *scroll = 0;
                }
            }
            Key::Home | Key::HistoryStart => *scroll = 0,
            Key::End | Key::HistoryEnd => *scroll = usize::MAX,
            Key::AuditUp => *scroll = scroll.saturating_sub(1),
            Key::AuditDown => *scroll = scroll.saturating_add(1),
            Key::Up => *scroll = scroll.saturating_sub(page),
            Key::Down => *scroll = scroll.saturating_add(page),
            Key::Mouse {
                button: 64,
                release: false,
                ..
            } => *scroll = scroll.saturating_sub(3),
            Key::Mouse {
                button: 65,
                release: false,
                ..
            } => *scroll = scroll.saturating_add(3),
            _ => {}
        }
        self.dirty = true;
        true
    }

    pub(super) fn reader_text(&mut self, bytes: &[u8]) -> bool {
        if self.reader.reading.is_none() {
            return false;
        }
        if !self.keyboard.pasting {
            match bytes {
                b"d" | b"D" => {
                    if let Some(Page::Announcement(slot)) = self.reader.reading {
                        self.dismiss_announcement(slot);
                    }
                }
                b"\x1b" | b"q" => self.reader.reading = None,
                _ => {}
            }
        }
        self.dirty = true;
        true
    }

    pub(super) fn reader_frame(&mut self) -> Option<Frame> {
        let (title, text, footer) = match self.reader.reading.as_ref()? {
            Page::Announcement(slot) => (
                if *slot == 0 {
                    "Server announcement"
                } else {
                    "Account announcement"
                },
                self.reader.entries[*slot].as_ref()?.text.as_str(),
                "Esc back · D dismiss · ←/→ notices · ↑↓ / PgUp/PgDn scroll",
            ),
            Page::Tool(text) => (
                "Tool call / response",
                text.as_str(),
                "Esc back · ↑↓ / PgUp/PgDn scroll",
            ),
        };
        let g = self.geometry;
        let height = usize::from(g.rows.saturating_sub(2).max(1));
        let body = markdown::literal_rows(text, g.cols, &self.ui);
        let start = self.reader.scroll.min(body.len().saturating_sub(height));
        self.reader.scroll = start;
        let mut rows = Vec::with_capacity(usize::from(g.rows));
        if g.rows > 1 {
            rows.push(self.notice_row(title, g.cols, PanelTone::Heading));
        }
        for i in 0..height {
            rows.push(
                body.get(start + i)
                    .cloned()
                    .unwrap_or_else(|| self.chat_line("", PanelTone::Normal, g.cols)),
            );
        }
        if g.rows > 2 {
            rows.push(self.chat_line(footer, PanelTone::Muted, g.cols));
        }
        Some(Frame {
            geometry: g,
            rows,
            cursor: (0, 0),
            hidden: true,
            application_cursor: false,
            application_keypad: false,
            bracketed_paste: true,
            mouse_capture: true,
        })
    }
}
