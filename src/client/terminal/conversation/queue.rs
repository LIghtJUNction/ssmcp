//! Request previews keep message text primary; Inspect retains exact identities.
use super::*;

fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

#[cfg(test)]
fn age(entry: &Value, now: u64) -> Option<String> {
    age_in(entry, now, Language::English)
}

fn age_in(entry: &Value, now: u64, language: Language) -> Option<String> {
    let timestamp = entry["updated_at_ms"]
        .as_u64()
        .filter(|value| *value > 0)
        .or_else(|| entry["created_at_ms"].as_u64().filter(|value| *value > 0))?;
    let minutes = now.saturating_sub(timestamp) / 60_000;
    Some(match minutes {
        0 => language.text("now", "刚刚").into(),
        1..60 => localized!(language, "{minutes}m", "{minutes} 分钟"),
        60..1440 => localized!(language, "{}h", "{} 小时", minutes / 60),
        _ => localized!(language, "{}d", "{} 天", minutes / 1440),
    })
}

#[cfg(test)]
fn metadata(entry: &Value, data: &Value, now: u64) -> String {
    metadata_in(entry, data, now, Language::English)
}

fn metadata_in(entry: &Value, data: &Value, now: u64, language: Language) -> String {
    let receipt = Receipt::from_entry(entry);
    let edit = if receipt.editable() {
        language.text(" · Enter edit", " · Enter 编辑")
    } else {
        ""
    };
    let mut detail = format!(
        "{} {}{edit} · #{}",
        receipt.marker(),
        language.receipt(receipt),
        short_id(message_id(entry))
    );
    if let Some(age) = age_in(entry, now, language) {
        detail.push_str(&format!(" · {age}"));
    }
    let target = field(entry, "target");
    let recipient = if target.is_empty() || target == "next" {
        language.text("next caller", "下一个调用方").into()
    } else {
        data["agents"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|agent| field(agent, "agent_id") == target)
            .map(|agent| field(agent, "name"))
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| localized!(language, "agent {}", "智能体 {}", short_id(target)))
    };
    detail.push_str(&format!(" · → {recipient}"));
    panel::safe_text(&detail)
}

#[cfg(test)]
fn preview(text: &str, width: u16) -> Vec<String> {
    preview_in(text, width, Language::English)
}

fn preview_in(text: &str, width: u16, language: Language) -> Vec<String> {
    // Redact each complete source line before wrapping or clipping a preview.
    let safe = references::source(text)
        .lines()
        .map(panel::safe_text)
        .map(|line| line.trim().to_owned())
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let width = usize::from(width.max(1));
    let mut remaining = safe.trim();
    let mut rows = Vec::new();
    while !remaining.is_empty() && rows.len() < 2 {
        let mut used = 0;
        let mut end = 0;
        for (offset, character) in remaining.char_indices() {
            let cells = character.width().unwrap_or(0);
            if used + cells > width {
                break;
            }
            used += cells;
            end = offset + character.len_utf8();
        }
        if end == 0 {
            let character = remaining.chars().next().unwrap();
            rows.push("…".into());
            remaining = &remaining[character.len_utf8()..];
            continue;
        }
        let boundary = if end < remaining.len() {
            remaining[..end]
                .rfind(' ')
                .filter(|offset| *offset > 0)
                .unwrap_or(end)
        } else {
            end
        };
        rows.push(remaining[..boundary].trim_end().to_owned());
        remaining = remaining[boundary..].trim_start();
    }
    if rows.is_empty() {
        rows.push(language.text("(empty request)", "（空请求）").into());
    } else if !remaining.is_empty() {
        let last = rows.last_mut().unwrap();
        *last = format!("{}…", fit(last, width.saturating_sub(1) as u16));
    }
    rows
}

impl State {
    fn queue_card(&self, entry: &Value, data: &Value, now: u64, selected: bool) -> Vec<String> {
        let width = self.geometry.left;
        let mut rows = Vec::new();
        for (line, text) in preview_in(
            field(entry, "text"),
            width.saturating_sub(2),
            self.ui.language,
        )
        .iter()
        .enumerate()
        {
            let marker = if selected && line == 0 { "› " } else { "  " };
            rows.push(self.chat_line(
                &format!("{marker}{text}"),
                if selected {
                    PanelTone::Accent
                } else {
                    PanelTone::Normal
                },
                width,
            ));
        }
        rows.push(self.chat_line(
            &format!(
                "  {}",
                panel::fit(
                    &metadata_in(entry, data, now, self.ui.language),
                    usize::from(width.saturating_sub(2))
                )
            ),
            if Receipt::from_entry(entry) == Receipt::Read {
                PanelTone::Success
            } else {
                PanelTone::Muted
            },
            width,
        ));
        rows
    }

    pub(super) fn chat_queue_rows(&mut self, data: &Value, height: u16) -> Vec<String> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .min(u128::from(u64::MAX)) as u64;
        self.queue_rows_at(data, height, now)
    }

    fn queue_rows_at(&mut self, data: &Value, height: u16, now: u64) -> Vec<String> {
        self.conversation.queue_hit_rows.clear();
        self.conversation.list_start = 0;
        self.conversation.list_count = 0;
        if height == 0 {
            return Vec::new();
        }
        let width = self.geometry.left;
        let entries = items(data, "queue");
        self.conversation.selected = self
            .conversation
            .selected
            .min(entries.len().saturating_sub(1));
        let pending = entries
            .iter()
            .filter(|entry| Receipt::from_entry(entry).pending())
            .count();
        let edit = entries
            .get(self.conversation.selected)
            .is_some_and(|entry| Receipt::from_entry(entry).editable());
        let action = if edit {
            self.ui.language.text("Enter edit", "Enter 编辑")
        } else {
            self.ui.language.text("Enter inspect", "Enter 查看")
        };
        let heading = if self.conversation.detail {
            self.ui
                .language
                .text("  Requests · Esc back", "  请求 · Esc 返回")
                .into()
        } else if width >= 100 {
            localized!(
                self.ui.language,
                "  Requests · {pending} pending · {} total    {action} · i inspect · Del cancel · R resend",
                "  请求 · {pending} 条待处理 · 共 {} 条    {action} · i 查看 · Del 取消 · R 重发",
                entries.len()
            )
        } else if width >= 60 {
            localized!(
                self.ui.language,
                "  Requests · {pending} pending · {} total · i inspect",
                "  请求 · {pending} 条待处理 · 共 {} 条 · i 查看",
                entries.len()
            )
        } else {
            localized!(
                self.ui.language,
                "  Requests · {} total · i inspect",
                "  请求 · 共 {} 条 · i 查看",
                entries.len()
            )
        };
        let mut rows = vec![self.chat_line(&heading, PanelTone::Muted, width)];
        self.conversation.queue_hit_rows.push(None);
        if entries.is_empty() {
            if height > 1 {
                rows.push(self.chat_line(
                    self.ui.language.text(
                        "  No requests yet. Write a requirement below.",
                        "  尚无请求。请在下方输入需求。",
                    ),
                    PanelTone::Normal,
                    width,
                ));
            }
            return rows;
        }
        if self.conversation.detail {
            let entry = &entries[self.conversation.selected];
            let mut detail = vec![self.chat_line(
                &format!("  {}", metadata_in(entry, data, now, self.ui.language)),
                PanelTone::Muted,
                width,
            )];
            detail.extend(markdown::rows(
                &format!("ID: {}", message_id(entry)),
                width,
                &self.ui,
            ));
            if Receipt::from_entry(entry) == Receipt::Read {
                detail.extend(markdown::rows(
                    self.ui.language.text("● Read means delivered with a tool response; it does not confirm understanding or execution.", "● 已读表示已随工具响应送达，不代表模型已理解或执行。"),
                    width, &self.ui,
                ));
                detail.extend(markdown::rows(
                    &localized!(
                        self.ui.language,
                        "Delivery run: {}",
                        "送达调用：{}",
                        field(&entry["delivery"], "run_id")
                    ),
                    width,
                    &self.ui,
                ));
            }
            detail.push(self.chat_line("", PanelTone::Normal, width));
            detail.extend(markdown::rows(
                references::source(field(entry, "text")),
                width,
                &self.ui,
            ));
            let available = usize::from(height.saturating_sub(1));
            self.conversation.detail_scroll = self
                .conversation
                .detail_scroll
                .min(detail.len().saturating_sub(available));
            rows.extend(
                detail
                    .into_iter()
                    .skip(self.conversation.detail_scroll)
                    .take(available),
            );
            return rows;
        }
        if height == 1 {
            return rows;
        }

        let selected = self.conversation.selected;
        let available = usize::from(height - 1);
        let mut cards = BTreeMap::new();
        cards.insert(
            selected,
            self.queue_card(&entries[selected], data, now, true),
        );
        let mut used = cards[&selected].len().min(available);
        let mut start = selected;
        // Only prepare nearby cards that can be shown in this viewport.
        while start > 0 {
            let card = self.queue_card(&entries[start - 1], data, now, false);
            if used + 1 + card.len() > available {
                break;
            }
            used += 1 + card.len();
            start -= 1;
            cards.insert(start, card);
        }
        self.conversation.list_start = start;
        for (index, entry) in entries.iter().enumerate().skip(start) {
            let card = cards
                .remove(&index)
                .unwrap_or_else(|| self.queue_card(entry, data, now, index == selected));
            let gap = usize::from(self.conversation.list_count > 0);
            if index != selected && rows.len() + gap + card.len() > usize::from(height) {
                break;
            }
            if gap > 0 {
                rows.push(self.chat_line("", PanelTone::Normal, width));
                self.conversation.queue_hit_rows.push(None);
            }
            for line in card {
                if rows.len() >= usize::from(height) {
                    break;
                }
                rows.push(line);
                self.conversation
                    .queue_hit_rows
                    .push(Some(message_id(entry).into()));
            }
            self.conversation.list_count += 1;
            if rows.len() >= usize::from(height) {
                break;
            }
        }
        rows
    }

    pub(super) fn chat_queue_hit(&self, data: &Value, row: u16) -> Option<usize> {
        if self.conversation.list_count == 0 || self.conversation.detail {
            return None;
        }
        let relative = row.checked_sub(body_top(self.geometry.rows))?;
        if relative >= self.geometry.log_rows {
            return None;
        }
        let id = self
            .conversation
            .queue_hit_rows
            .get(usize::from(relative))?
            .as_deref()?;
        data["queue"]
            .as_array()?
            .iter()
            .position(|entry| message_id(entry) == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_800_000_000_000;
    const FIRST: &str = "a194f219abcdef0123456789abcdef012";
    const SECOND: &str = "e917ab23abcdef0123456789abcdef012";
    const THIRD: &str = "c42f9d10abcdef0123456789abcdef012";

    fn data() -> Value {
        json!({"agents":[{"agent_id":"builder","name":"Builder"}],"queue":[
            {"id":FIRST,"text":"Verify the command output before publishing the update.","status":"delivered","target":"next","updated_at_ms":NOW-120_000},
            {"id":SECOND,"text":"Keep the request list easy to scan. 显示正文，保留完整身份。","status":"queued","target":"builder","updated_at_ms":NOW-30_000},
            {"id":THIRD,"text":"Preserve this cancelled request for inspection.","status":"cancelled","target":"next","updated_at_ms":NOW-3_600_000}
        ]})
    }

    fn state(cols: u16) -> State {
        let mut state = super::super::super::preview_state(28, cols, &Ui::default());
        state.chat_change_view(View::Queue);
        state.conversation.selected = 1;
        state.conversation.composer.set_text("Keep this draft");
        state
    }

    fn save_preview(state: &mut State, data: &Value, name: &str) {
        if let Some(path) = std::env::var_os("SSMCP_QUEUE_PREVIEW_DIR") {
            let selected = state.conversation.selected;
            let mut frame = state.chat_frame(Instant::now());
            state.conversation.selected = selected;
            let body = state.queue_rows_at(data, state.geometry.log_rows, NOW);
            for index in 0..usize::from(state.geometry.log_rows) {
                frame.rows[usize::from(body_top(state.geometry.rows)) + index] = body
                    .get(index)
                    .cloned()
                    .unwrap_or_else(|| state.chat_line("", PanelTone::Normal, state.geometry.left));
            }
            let path = std::path::PathBuf::from(path);
            std::fs::create_dir_all(&path).unwrap();
            std::fs::write(
                path.join(format!("{name}-{}.ansi", state.geometry.cols)),
                frame.diff(None),
            )
            .unwrap();
        }
    }

    #[test]
    fn request_cards_prioritize_body_and_preserve_full_inspection_and_exact_mouse_targets() {
        let data = data();
        for cols in [120, 48] {
            let mut state = state(cols);
            save_preview(&mut state, &data, "requests");
            state.ui.color = false;
            let rows = state.queue_rows_at(&data, state.geometry.log_rows, NOW);
            let shown = rows.join("\n");
            assert!(shown.contains("› Keep the request list"), "{shown}");
            assert!(!shown.contains(FIRST) && !shown.contains(SECOND));
            let meta = rows
                .iter()
                .position(|row| row.contains("○ Waiting · Enter edit"))
                .unwrap();
            assert!(
                rows[meta.saturating_sub(2)..meta]
                    .iter()
                    .any(|row| row.contains("› Keep the request list"))
            );
            assert!(rows.iter().any(|row| row.trim().is_empty()));
            let row = body_top(state.geometry.rows) + meta as u16;
            assert_eq!(state.chat_queue_hit(&data, row), Some(1));
            let mut moved = data.clone();
            moved["queue"].as_array_mut().unwrap().swap(0, 1);
            assert_eq!(
                state.chat_queue_hit(&moved, row),
                Some(0),
                "mouse targets the full retained identity after reordering"
            );
            moved["queue"].as_array_mut().unwrap().remove(0);
            assert_eq!(state.chat_queue_hit(&moved, row), None);
            let blank = rows.iter().position(|row| row.trim().is_empty()).unwrap();
            assert_eq!(
                state.chat_queue_hit(&data, body_top(state.geometry.rows) + blank as u16),
                None
            );

            state.conversation.detail = true;
            let detail = state
                .queue_rows_at(&data, state.geometry.log_rows, NOW)
                .join("\n");
            assert!(
                detail.contains(SECOND) && detail.contains("保留完整身份"),
                "{detail}"
            );
            assert_eq!(state.conversation.composer.text(), "Keep this draft");
            state.ui.color = true;
            save_preview(&mut state, &data, "request-inspect");
        }
    }

    #[test]
    fn request_preview_is_bounded_and_metadata_handles_missing_or_future_times() {
        for width in [1, 2, 10, 24, 48] {
            let rows = preview(
                "A long request with 中文 characters and many words to inspect later.",
                width,
            );
            assert!(rows.len() <= 2);
            assert!(rows.iter().all(|row| row.width() <= usize::from(width)));
        }
        assert_eq!(age(&json!({}), NOW), None);
        assert_eq!(
            age(&json!({"updated_at_ms":NOW+1000}), NOW).as_deref(),
            Some("now")
        );
        assert_eq!(
            age(&json!({"created_at_ms":NOW-3_600_000}), NOW).as_deref(),
            Some("1h")
        );
        let entry = json!({"id":FIRST,"status":"queued","target":"missing-agent","updated_at_ms":NOW-120_000});
        assert_eq!(
            metadata(&entry, &json!({}), NOW),
            "○ Waiting · Enter edit · #a194f219 · 2m · → agent missing-"
        );
    }
    #[test]
    fn receipt_request_cards_keep_read_proof_and_edit_hints_distinct_at_narrow_widths() {
        let delivered = json!({"id":FIRST,"text":"Unicode 回执 🙂 stays readable",
            "status":"delivered","target":"next","updated_at_ms":NOW,
            "delivery":{"run_id":"run_confirmed","agent_id":null,"included_at_ms":NOW}});
        for cols in [12, 24, 48, 120] {
            let mut state = state(cols);
            state.ui.color = false;
            state.conversation.selected = 0;
            let cursor = state.conversation.composer.cursor();
            let data = json!({"queue":[delivered.clone()]});
            let rows = state.queue_rows_at(&data, state.geometry.log_rows, NOW);
            let shown = rows.join("\n");
            assert!(shown.contains("● Read"), "{cols}: {shown}");
            assert!(!shown.contains("Enter edit"), "{cols}: {shown}");
            assert!(
                rows.iter()
                    .all(|row| row.width() <= usize::from(state.geometry.left))
            );
            assert_eq!(state.conversation.composer.cursor(), cursor);
            assert_eq!(state.conversation.composer.text(), "Keep this draft");
            let mut waiting = delivered.clone();
            waiting["status"] = json!("queued");
            let rows =
                state.queue_rows_at(&json!({"queue":[waiting]}), state.geometry.log_rows, NOW);
            let shown = rows.join("\n");
            assert!(shown.contains("○ Waiting"), "{cols}: {shown}");
            assert!(!shown.contains("● Read"), "{cols}: {shown}");
            if cols >= 48 {
                assert!(shown.contains("Enter edit"));
            }
        }
        let mut state = state(120);
        state.ui.color = false;
        state.conversation.selected = 0;
        state.conversation.detail = true;
        let detail = state
            .queue_rows_at(&json!({"queue":[delivered]}), state.geometry.log_rows, NOW)
            .join("\n");
        assert!(detail.contains("tool response") && detail.contains("execution"));
        assert!(detail.contains("run_confirmed"));
    }
}
