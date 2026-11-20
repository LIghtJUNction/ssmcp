//! A bounded viewport over adjacent retained tool events; no output is copied
//! into a second history and individual event identities remain selectable.
use super::{
    Event, Ui, clip_plain, conversation_body, correlation::Kind, paint, read_activity,
    semantic_text, styled_rows,
};
use std::collections::VecDeque;

#[derive(Clone, Debug)]
pub(super) struct Group {
    pub(super) start: usize,
    pub(super) end: usize,
    pub(super) head: u64,
}

impl Group {
    pub(super) fn contains(&self, index: usize) -> bool {
        (self.start..self.end).contains(&index)
    }
}

/// Prose, diagnostics, images, diffs and existing compact read summaries keep
/// their boundaries. Filtering never makes originally separated calls adjacent.
pub(super) fn groups(events: &VecDeque<Event>, reads: &[read_activity::Group]) -> Vec<Group> {
    let eligible = |index: usize| {
        let event = &events[index];
        event.meta.kind == Kind::Tool
            && !event.meta.diff
            && event.image.is_none()
            && !reads.iter().any(|group| group.contains(index))
    };
    let mut result = Vec::new();
    let mut start = 0;
    while start < events.len() {
        if !eligible(start) {
            start += 1;
            continue;
        }
        let mut end = start + 1;
        while end < events.len() && eligible(end) {
            end += 1;
        }
        if end - start > 1 {
            result.push(Group {
                start,
                end,
                head: events[start].id,
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
    pub(super) focused: bool,
    pub(super) reveal: bool,
}

pub(super) struct Block {
    pub(super) rows: Vec<(u64, String)>,
    pub(super) panels: Vec<(u64, usize, usize)>,
    pub(super) selected_row: Option<usize>,
}

pub(super) fn render(events: &mut VecDeque<Event>, group: &Group, options: Options<'_>) -> Block {
    let Options {
        width,
        page,
        palette,
        ui,
        selected,
        focused,
        reveal,
    } = options;
    let gutter_width = 4.min(width.saturating_sub(1));
    let body_width = width.saturating_sub(gutter_width).max(1);
    let indent = 4.min(body_width.saturating_sub(1));
    let detail_width = body_width.saturating_sub(indent).max(1);
    let window_page = page.saturating_sub(2).clamp(1, 12);
    let mut body = Vec::new();
    let mut panels = Vec::new();
    let mut selected_row = None;
    let mut selected_end = None;
    let mut failed = 0;
    let mut running = 0;

    for index in group.start..group.end {
        let event = &mut events[index];
        failed += usize::from(event.meta.status.is_some_and(|status| status.failed()));
        running += usize::from(event.meta.status == Some(super::Status::Running));
        let selected = selected == Some(event.id);
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
            let rows = if event.meta.command {
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
        if event.meta.command {
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

    let state = if failed > 0 {
        format!(" · {failed} failed")
    } else if running > 0 {
        format!(" · {running} running")
    } else {
        String::new()
    };
    let header = format!("  Tools · {} calls{state}", group.end - group.start);
    let mut rows = vec![(
        group.head,
        paint(
            &clip_plain(&header, width),
            if failed > 0 { "warning" } else { "muted" },
            ui,
            false,
        ),
    )];
    let gutter = paint(&clip_plain("  │ ", gutter_width), "muted", ui, false);
    rows.extend(
        body.into_iter()
            .skip(offset)
            .take(panel.page)
            .map(|(id, row)| (id, format!("{gutter}{row}"))),
    );
    let range = format!("{}–{}/{}", offset + 1, offset + panel.page, panel.rows);
    let hint = if width >= 70 {
        format!("  └ {range} · wheel/PgUp/PgDn scroll tools · Enter details")
    } else {
        format!("  └ {range} · scroll tools · Enter details")
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
            (start < end).then_some((id, 1 + start - offset, 1 + end - offset))
        })
        .collect();
    let selected_row = selected_row
        .filter(|row| *row >= offset && *row < offset + panel.page)
        .map(|row| 1 + row - offset);
    Block {
        rows,
        panels,
        selected_row,
    }
}

#[cfg(test)]
mod tests;
