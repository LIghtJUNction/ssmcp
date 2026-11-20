//! A bounded viewport over adjacent retained tool events; no output is copied
//! into a second history and individual event identities remain selectable.
use super::{
    Event, Ui, clip_plain, conversation_body, correlation::Kind, paint, semantic_text, styled_rows,
};
use std::collections::VecDeque;

#[derive(Clone, Debug)]
pub(super) struct Group {
    pub(super) start: usize,
    pub(super) end: usize,
    pub(super) members: Vec<usize>,
    pub(super) head: u64,
    pub(super) expanded: bool,
}

impl Group {
    pub(super) fn contains(&self, index: usize) -> bool {
        (self.start..self.end).contains(&index)
    }
}

#[derive(Default, serde::Serialize)]
pub(super) struct Stats {
    pub(super) running: usize,
    pub(super) done: usize,
    pub(super) failed: usize,
    pub(super) unknown: usize,
}

pub(super) fn stats(events: &VecDeque<Event>, group: &Group) -> Stats {
    let mut stats = Stats::default();
    for index in &group.members {
        match events[*index].meta.status {
            Some(super::Status::Running) => stats.running += 1,
            Some(super::Status::Done | super::Status::Exit(0)) => stats.done += 1,
            Some(status) if status.failed() => stats.failed += 1,
            _ => stats.unknown += 1,
        }
    }
    stats
}

/// Retained logical calls count once; stdout/lifecycle updates are already
/// correlated into those events. Native discovery bookkeeping is transparent.
/// Conversation, images and other diagnostics retain their original boundaries.
pub(super) fn groups(events: &VecDeque<Event>, owned: impl Fn(&Event) -> bool) -> Vec<Group> {
    let eligible = |index: usize| {
        let event = &events[index];
        owned(event) && event.meta.kind == Kind::Tool && event.image.is_none() && !event.reload_boundary
    };
    let mut result = Vec::new();
    let mut start = 0;
    while start < events.len() {
        if !eligible(start) {
            start += 1;
            continue;
        }
        let mut members = vec![start];
        let mut end = start + 1;
        while end < events.len() {
            if eligible(end) && events[end].historical == events[start].historical {
                members.push(end);
            } else if !(owned(&events[end])
                && events[end].meta.kind == Kind::Log
                && events[end].meta.read_bookkeeping
                && events[end].image.is_none()
                && !matches!(events[end].tone, "error" | "warning"))
            {
                break;
            }
            end += 1;
        }
        if members.len() > 1 {
            result.push(Group {
                start,
                end,
                members,
                head: events[start].id,
                expanded: events[start].group_expanded,
            });
        }
        start = end;
    }
    result
}

pub(super) struct Options<'a> {
    pub(super) width: usize,
    pub(super) page: usize,
    pub(super) palette: &'a str,
    pub(super) ui: &'a Ui,
    pub(super) selected: Option<u64>,
    pub(super) selected_group: Option<u64>,
    pub(super) focused: bool,
    pub(super) reveal: bool,
}

pub(super) struct Block {
    pub(super) rows: Vec<(u64, String)>,
    pub(super) panels: Vec<(u64, usize, usize)>,
    pub(super) selected_row: Option<usize>,
    pub(super) header_rows: usize,
}

pub(super) fn render(events: &mut VecDeque<Event>, group: &Group, options: Options<'_>) -> Block {
    let Options {
        width,
        page,
        palette,
        ui,
        selected,
        selected_group,
        focused,
        reveal,
    } = options;
    let stats = stats(events, group);
    let active = selected_group == Some(group.head);
    let marker = if group.expanded { "▾" } else { "▸" };
    let history = if group.members.iter().all(|index| events[*index].historical) {
        " · history"
    } else {
        ""
    };
    let title = format!(
        "{} {marker} Tools · {} calls{history}",
        if active && focused { "›" } else { " " },
        group.members.len()
    );
    let status = [
        (stats.running, "run"),
        (stats.done, "done"),
        (stats.failed, "fail"),
        (stats.unknown, "unknown"),
    ]
    .into_iter()
    .filter(|(count, _)| *count > 0)
    .map(|(count, label)| format!("{label} {count}"))
    .collect::<Vec<_>>()
    .join(" · ");
    let tone = if stats.failed > 0 {
        "warning"
    } else if active && focused {
        "accent"
    } else {
        "muted"
    };
    let mut header = if title.chars().count() + status.chars().count() + 3 <= width {
        vec![(
            group.head,
            paint(
                &clip_plain(&format!("{title} · {status}"), width),
                tone,
                ui,
                active && focused,
            ),
        )]
    } else {
        vec![
            (
                group.head,
                paint(&clip_plain(&title, width), tone, ui, active && focused),
            ),
            (
                group.head,
                paint(
                    &clip_plain(&format!("    {status}"), width),
                    "muted",
                    ui,
                    false,
                ),
            ),
        ]
    };
    let header_rows = header.len();
    if !group.expanded {
        return Block {
            rows: header,
            panels: Vec::new(),
            selected_row: active.then_some(0),
            header_rows,
        };
    }

    let gutter_width = 4.min(width.saturating_sub(1));
    let body_width = width.saturating_sub(gutter_width).max(1);
    let indent = 4.min(body_width.saturating_sub(1));
    let detail_width = body_width.saturating_sub(indent).max(1);
    let window_page = page.saturating_sub(header_rows + 1).clamp(1, 12);
    let mut body = Vec::new();
    let mut panels = Vec::new();
    let mut selected_row = None;
    let mut selected_end = None;
    for index in &group.members {
        let event = &mut events[*index];
        let command = event.meta.command && !event.meta.diff;
        let selected = selected == Some(event.id) && !active;
        if selected {
            selected_row = Some(body.len());
        }
        let expanded = event.chat_expanded;
        let marker = if expanded { "▾" } else { "▸" };
        let prefix = clip_plain(
            &format!("{} {marker} ", if selected && focused { "›" } else { " " }),
            indent,
        );
        let tone = event.chat_tone(selected && focused);
        body.push((
            event.id,
            format!(
                "{}{}",
                paint(&prefix, tone, ui, selected && focused),
                paint(
                    &clip_plain(&event.chat_title(), detail_width),
                    tone,
                    ui,
                    selected && focused,
                ),
            ),
        ));

        let cache_palette = format!("{palette}:tool-window:{expanded}");
        if event
            .cache
            .as_ref()
            .is_none_or(|(cached_width, cached_palette, _)| {
                *cached_width != detail_width || *cached_palette != cache_palette
            })
        {
            let rows = if command {
                let text = if expanded {
                    // This is the same bounded, redacted source used by the
                    // standalone command panel, never native argument metadata.
                    format!("Command\n{}", event.command_detail_text().trim_end())
                } else {
                    event.command_output()
                };
                if text.is_empty() {
                    Vec::new()
                } else {
                    styled_rows(
                        &semantic_text(&text, ui, event.meta.language),
                        detail_width,
                        ui.color,
                    )
                }
            } else if expanded {
                let detail = event
                    .meta
                    .read
                    .as_ref()
                    .map(|read| read.details(&event.text));
                let text = detail
                    .as_deref()
                    .unwrap_or_else(|| conversation_body(&event.text));
                styled_rows(
                    &semantic_text(text, ui, event.meta.language),
                    detail_width,
                    ui.color,
                )
            } else {
                let mut preview = event.chat_preview(detail_width);
                // One shared footer describes expansion for the whole window.
                preview.pop();
                preview
            };
            event.cache = Some((detail_width, cache_palette, rows));
        }
        let content = &event.cache.as_ref().unwrap().2;
        if command {
            event.panel.rows = content.len();
            let limit = if expanded {
                window_page.saturating_sub(2).clamp(1, 10)
            } else {
                3
            };
            event.panel.page = limit.min(content.len());
            let offset = event.panel.offset();
            if event.panel.scroll.is_some() {
                event.panel.scroll = Some(offset);
            }
            let panel_start = body.len();
            for row in content.iter().skip(offset).take(event.panel.page) {
                body.push((event.id, format!("{}{row}", " ".repeat(indent))));
            }
            if expanded && !content.is_empty() {
                let range = format!(
                    "{}–{}/{}",
                    offset + 1,
                    offset + event.panel.page,
                    content.len()
                );
                let partial = if event.truncated
                    || event
                        .command_source
                        .as_ref()
                        .is_some_and(|source| source.truncated)
                {
                    " · partial"
                } else {
                    ""
                };
                let hint = if body_width >= 62 {
                    format!("{range} · wheel/PgUp/PgDn · Home command · Enter fold{partial}")
                } else {
                    format!("{range} · Home cmd · Enter fold{partial}")
                };
                body.push((
                    event.id,
                    paint(&clip_plain(&hint, body_width), "muted", ui, false),
                ));
                // The item's title belongs to the outer scroll region. Its
                // expanded details and footer own the inner viewport.
                panels.push((event.id, panel_start, body.len()));
            }
        } else {
            for row in content {
                body.push((event.id, format!("{}{row}", " ".repeat(indent))));
            }
        }
        if selected {
            selected_end = Some(body.len());
        }
    }

    let panel = &mut events[group.start].group_panel;
    panel.rows = body.len();
    panel.page = window_page.min(body.len());
    let mut offset = panel.offset();
    if reveal
        && let Some(row) = selected_row
        && (row < offset || selected_end.unwrap_or(row + 1) > offset.saturating_add(panel.page))
    {
        offset = row.min(panel.rows.saturating_sub(panel.page));
        panel.scroll = Some(offset);
    } else if panel.scroll.is_some() {
        panel.scroll = Some(offset);
    }

    let mut rows = Vec::new();
    rows.append(&mut header);
    let gutter = paint(&clip_plain("  │ ", gutter_width), "muted", ui, false);
    rows.extend(
        body.into_iter()
            .skip(offset)
            .take(panel.page)
            .map(|(id, row)| (id, format!("{gutter}{row}"))),
    );
    let range = format!("{}–{}/{}", offset + 1, offset + panel.page, panel.rows);
    let hint = if width >= 70 {
        format!("  └ {range} · wheel/PgUp/PgDn scroll tools · header folds")
    } else {
        format!("  └ {range} · scroll tools · header folds")
    };
    rows.push((
        group.head,
        paint(&clip_plain(&hint, width), "muted", ui, false),
    ));
    let panels = panels
        .into_iter()
        .filter_map(|(id, start, end)| {
            let start = start.max(offset);
            let end = end.min(offset + panel.page);
            (start < end).then_some((id, header_rows + start - offset, header_rows + end - offset))
        })
        .collect();
    let selected_row = selected_row
        .filter(|row| *row >= offset && *row < offset + panel.page)
        .map(|row| header_rows + row - offset);
    Block {
        rows,
        panels,
        selected_row: if active { Some(0) } else { selected_row },
        header_rows,
    }
}

#[cfg(test)]
mod tests;
