//! Session-local announcement banners and their full-screen reader.
use super::*;

#[derive(Default)]
pub(super) struct Announcements {
    // Slots follow the public account response: server first, then personal.
    entries: [Option<Entry>; 2],
    reading: Option<usize>,
    scroll: usize,
}

struct Entry {
    text: String,
    identity: String,
    dismissed: bool,
}

impl Announcements {
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
            let current = &mut self.announcements.entries[slot];
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
            if self.announcements.reading == Some(slot)
                && current.as_ref().map(|v| &v.identity) != updated.as_ref().map(|v| &v.identity)
            {
                self.announcements.reading = None;
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
            .announcements
            .visible()
            .next()
            .map(|(slot, _)| slot)
            .or_else(|| self.announcements.entries.iter().position(Option::is_some))
            .ok_or("No current announcements")?;
        self.announcements.reading = Some(slot);
        self.announcements.scroll = 0;
        self.dirty = true;
        Ok(())
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
        if let Some(entry) = &mut self.announcements.entries[slot] {
            entry.dismissed = true;
        }
        self.announcements.reading = None;
        self.resize(self.geometry.rows, self.geometry.cols);
        self.dirty = true;
    }

    pub(super) fn announcement_banner(&self, row: u16, width: u16) -> String {
        let text = self
            .announcements
            .visible()
            .nth(usize::from(row))
            .map_or("", |(_, text)| text);
        self.notice_row(&format!(" ▸ {text}"), width, PanelTone::Muted)
    }

    pub(super) fn announcement_key(&mut self, key: Key) -> bool {
        let Some(slot) = self.announcements.reading else {
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
                    .announcements
                    .visible()
                    .nth(usize::from(y - 1))
                    .map(|(slot, _)| slot);
                if let Some(slot) = selected
                    && self.geometry.left >= 12
                    && x >= self.geometry.left - 3
                {
                    self.dismiss_announcement(slot);
                } else {
                    self.announcements.reading = selected;
                    self.announcements.scroll = 0;
                    self.dirty = true;
                }
                return true;
            }
            return false;
        };
        let page = usize::from(self.geometry.rows.saturating_sub(2).max(1));
        let scroll = &mut self.announcements.scroll;
        match key {
            Key::Escape | Key::Interrupt => self.announcements.reading = None,
            Key::Mouse {
                button: 0,
                x,
                y: 0,
                release: false,
            } if self.geometry.rows > 1
                && self.geometry.cols >= 12
                && x >= self.geometry.cols - 3 =>
            {
                self.announcements.reading = None;
            }
            Key::Back | Key::Open => {
                let other = 1 - slot;
                if self.announcements.entries[other].is_some() {
                    self.announcements.reading = Some(other);
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

    pub(super) fn announcement_text(&mut self, bytes: &[u8]) -> bool {
        let Some(slot) = self.announcements.reading else {
            return false;
        };
        if !self.keyboard.pasting {
            match bytes {
                b"d" | b"D" => self.dismiss_announcement(slot),
                b"\x1b" | b"q" => self.announcements.reading = None,
                _ => {}
            }
        }
        self.dirty = true;
        true
    }

    pub(super) fn announcement_frame(&mut self) -> Option<Frame> {
        let slot = self.announcements.reading?;
        let entry = self.announcements.entries[slot].as_ref()?;
        let g = self.geometry;
        let height = usize::from(g.rows.saturating_sub(2).max(1));
        let body = markdown::literal_rows(&entry.text, g.cols, &self.ui);
        let start = self
            .announcements
            .scroll
            .min(body.len().saturating_sub(height));
        self.announcements.scroll = start;
        let title = if slot == 0 {
            "Server announcement"
        } else {
            "Account announcement"
        };
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
            rows.push(self.chat_line(
                "Esc back · D dismiss · ←/→ notices · ↑↓ / PgUp/PgDn scroll",
                PanelTone::Muted,
                g.cols,
            ));
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
