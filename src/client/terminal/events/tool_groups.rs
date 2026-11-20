//! Bounded, source-aware tool containers. Child identities and scroll anchors
//! survive projection; old full-height cards are never clipped into a new box.
use super::*;
use crate::{client::language::localized, runtime::call_scope::CallScope};
use std::{collections::BTreeMap, sync::Arc};

pub(super) const MAX_BODY_ROWS: usize = 8;

#[derive(Clone)]
pub(super) struct Group {
    pub(super) head: u64,
    pub(super) start: usize,
    pub(super) end: usize,
    source: Arc<CallScope>,
}

impl Group {
    pub(super) fn contains(&self, index: usize) -> bool {
        (self.start..self.end).contains(&index)
    }

    fn label(&self) -> String {
        let attribution = self.source.attribution().expect("group source is verified");
        let account = &attribution.account;
        match &attribution.agent {
            Some(agent) => format!("{} · {} #{}", agent.name, account.login, account.subject),
            None => format!("{} #{}", account.login, account.subject),
        }
    }
}

fn same_sender(left: &CallScope, right: &CallScope) -> bool {
    if left.connection() != right.connection() || left.thread() != right.thread() {
        return false;
    }
    let (Some(left_account), Some(right_account)) = (left.attribution(), right.attribution())
    else {
        return false;
    };
    if left_account.account.provider != right_account.account.provider
        || left_account.account.subject != right_account.account.subject
    {
        return false;
    }
    match (&left_account.agent, &right_account.agent) {
        (Some(left), Some(right)) => left.id == right.id,
        (None, None) => match (left.run_id(), right.run_id()) {
            (Some(left), Some(right)) => left == right,
            (None, None) => left.request_id() == right.request_id(),
            _ => false,
        },
        _ => false,
    }
}

fn source(event: &Event) -> Option<&Arc<CallScope>> {
    if event.meta.kind != Kind::Tool || event.image.is_some() || event.diagnostic.is_some() {
        return None;
    }
    event
        .meta
        .scope
        .as_ref()
        .filter(|scope| scope.attribution().is_some())
}

pub(super) fn groups(events: &VecDeque<Event>) -> Vec<Group> {
    let mut result = Vec::new();
    let mut index = 0;
    while index < events.len() {
        let Some(first) = source(&events[index]) else {
            index += 1;
            continue;
        };
        let start = index;
        index += 1;
        while index < events.len()
            && source(&events[index]).is_some_and(|next| same_sender(first, next))
        {
            index += 1;
        }
        if index - start > 1 {
            result.push(Group {
                head: events[start].id,
                start,
                end: index,
                source: first.clone(),
            });
        }
    }
    result
}

#[derive(Clone, Copy, Debug)]
struct Anchor {
    id: u64,
    row: usize,
}

#[derive(Clone, Debug, Default)]
pub(super) struct State {
    source: Option<Arc<CallScope>>,
    pub(super) members: Vec<u64>,
    anchor: Option<Anchor>,
    layout: Vec<(u64, usize, usize)>,
    pub(super) rows: usize,
    pub(super) page: usize,
}

impl State {
    pub(super) fn offset(&self) -> usize {
        let end = self.rows.saturating_sub(self.page);
        self.anchor.map_or(end, |anchor| {
            self.layout
                .iter()
                .find(|(id, _, _)| *id == anchor.id)
                .map_or(0, |(_, start, height)| {
                    start + anchor.row.min(height.saturating_sub(1))
                })
                .min(end)
        })
    }

    fn set_offset(&mut self, offset: usize) {
        let end = self.rows.saturating_sub(self.page);
        let offset = offset.min(end);
        self.anchor = if offset == end {
            None
        } else {
            self.layout
                .iter()
                .find(|(_, start, height)| (*start..*start + *height).contains(&offset))
                .map(|(id, start, _)| Anchor {
                    id: *id,
                    row: offset - start,
                })
        };
    }

    pub(super) fn scroll_by(&mut self, delta: isize) {
        self.set_offset(self.offset().saturating_add_signed(delta));
    }

    pub(super) fn following(&self) -> bool {
        self.anchor.is_none()
    }

    pub(super) fn latest(&mut self) {
        self.anchor = None;
    }

    pub(super) fn hold(&mut self) {
        if self.anchor.is_none() {
            let offset = self.offset();
            self.anchor = self
                .layout
                .iter()
                .find(|(_, start, height)| (*start..*start + *height).contains(&offset))
                .map(|(id, start, _)| Anchor {
                    id: *id,
                    row: offset - start,
                });
        }
    }

    pub(super) fn has_layout(&self) -> bool {
        !self.layout.is_empty()
    }

    fn reveal(&mut self, id: u64) {
        let Some((_, start, _)) = self.layout.iter().find(|(member, _, _)| *member == id) else {
            return;
        };
        let offset = self.offset();
        if *start < offset || *start >= offset.saturating_add(self.page) {
            self.set_offset(*start);
        }
    }
}

pub(super) fn reconcile(
    states: &mut BTreeMap<u64, State>,
    groups: &[Group],
    events: &VecDeque<Event>,
) {
    let mut previous = std::mem::take(states);
    for group in groups {
        let members: Vec<_> = (group.start..group.end)
            .map(|index| events[index].id)
            .collect();
        let inherited = previous
            .iter()
            .find(|(_, state)| {
                state
                    .source
                    .as_ref()
                    .is_some_and(|source| same_sender(source, &group.source))
                    && state.members.iter().any(|id| members.contains(id))
            })
            .map(|(head, _)| *head);
        let mut state = previous
            .remove(&group.head)
            .or_else(|| inherited.and_then(|head| previous.remove(&head)))
            .unwrap_or_default();
        state.source = Some(group.source.clone());
        state.members = members;
        states.insert(group.head, state);
    }
}

pub(super) struct Row {
    pub(super) id: u64,
    pub(super) text: String,
    pub(super) selectable: bool,
    pub(super) output: bool,
}

/// A row's outer viewport and, only for actual output, its inner viewport.
/// Frame coordinates are discarded whenever projection/filtering changes.
#[derive(Clone, Copy, Debug)]
pub(super) struct Hit {
    pub(super) head: u64,
    pub(super) child: Option<u64>,
    pub(super) output: bool,
}

#[cfg(all(test, unix))]
mod tests;

pub(super) struct Rendered {
    pub(super) rows: Vec<Row>,
    pub(super) selected_row: Option<usize>,
}

pub(super) struct Layout<'a> {
    pub(super) width: usize,
    pub(super) page: usize,
    pub(super) ui: &'a Ui,
    pub(super) focused: bool,
    pub(super) selected: Option<u64>,
    pub(super) reveal: bool,
    pub(super) diff_mode: DiffMode,
}

pub(super) fn render(
    events: &mut VecDeque<Event>,
    group: &Group,
    state: &mut State,
    layout: Layout<'_>,
) -> Rendered {
    let Layout {
        width,
        page,
        ui,
        focused,
        selected,
        reveal,
        diff_mode,
    } = layout;
    let heading = usize::from(page >= 2);
    let footer = usize::from(page >= 3);
    let body_limit = (page / 2)
        .clamp(1, MAX_BODY_ROWS)
        .min(page.saturating_sub(heading + footer).max(1));
    let inset = 5.min(width.saturating_sub(1));
    let content_width = width.saturating_sub(inset).max(1);
    let palette = format!(
        "{}:{}:tool-group:{}",
        palette_key(ui),
        ui.language.code(),
        diff_mode.name()
    );
    state.layout.clear();
    state.rows = 0;
    for event in events.iter_mut().take(group.end).skip(group.start) {
        let expanded = event.chat_expanded;
        let mut height = 1;
        if expanded {
            if event
                .cache
                .as_ref()
                .is_none_or(|(cached_width, cached_palette, _)| {
                    *cached_width != content_width || *cached_palette != palette
                })
            {
                let body = if event.meta.command && !event.meta.diff {
                    format!(
                        "{}\n{}",
                        ui.language.text("Command", "命令"),
                        event.text.trim_end()
                    )
                } else if let Some(read) = &event.meta.read {
                    read.details(&event.text)
                } else {
                    event.text.clone()
                };
                let lines = if event.meta.diff {
                    diff::rows(&body, content_width, ui, event.meta.language, diff_mode).into()
                } else {
                    styled_rows(
                        &semantic_text(&body, ui, event.meta.language),
                        content_width,
                        ui.color,
                    )
                };
                event.cache = Some((content_width, palette.clone(), lines));
            }
            event.panel.rows = event.cache.as_ref().unwrap().2.len();
            event.panel.page = event.panel.rows.min((body_limit / 2).clamp(1, 4));
            let offset = event.panel.offset();
            event.panel.scroll =
                (offset != event.panel.rows.saturating_sub(event.panel.page)).then_some(offset);
            height += event.panel.page + usize::from(event.panel.rows > 0);
        } else {
            event.panel.rows = 0;
            event.panel.page = 0;
        }
        state.layout.push((event.id, state.rows, height));
        state.rows += height;
    }
    state.page = body_limit.min(state.rows);
    if reveal
        && let Some(selected) = selected
        && state.layout.iter().any(|(id, _, _)| *id == selected)
    {
        state.reveal(selected);
        // Reading a child pins the containing view too, even if that view
        // happens to end at its current last row. New tools cannot hide it.
        state.hold();
    }
    let offset = state.offset();
    let mut rows = Vec::with_capacity(state.page + heading + footer);
    if heading != 0 {
        rows.push(Row {
            id: group.head,
            text: paint(
                &clip_plain(
                    &localized!(
                        ui.language,
                        "╭ Tools · {} · {}",
                        "╭ 工具 · {} · {}",
                        group.end - group.start,
                        group.label()
                    ),
                    width,
                ),
                "muted",
                ui,
                false,
            ),
            selectable: false,
            output: false,
        });
    }
    let mut selected_row = None;
    // Materialize only visible rows. The child layout retains precise IDs and
    // inner-output ownership independently of this bounded viewport.
    for line in offset..offset + state.page {
        let child = state
            .layout
            .iter()
            .position(|(_, start, height)| (*start..*start + *height).contains(&line))
            .expect("a bounded tool row belongs to one child");
        let (_, start, _) = state.layout[child];
        let event = &events[group.start + child];
        let relative = line - start;
        let is_selected = selected == Some(event.id);
        if is_selected && selected_row.is_none() {
            selected_row = Some(rows.len());
        }
        let (text, output) = if relative == 0 {
            let marker = if event.chat_expanded { "▾" } else { "▸" };
            let focus = if focused && is_selected { "›" } else { " " };
            let title = format!("│{focus}{marker} {}", event.chat_title());
            (
                paint(
                    &clip_plain(&title, width),
                    event.chat_tone(is_selected && focused),
                    ui,
                    is_selected && focused,
                ),
                false,
            )
        } else if relative <= event.panel.page {
            let index = event.panel.offset() + relative - 1;
            let body = &event.cache.as_ref().unwrap().2[index];
            let gutter = paint(&clip_plain("│  │ ", inset), "muted", ui, false);
            (format!("{gutter}{body}"), true)
        } else {
            let text = localized!(
                ui.language,
                "│  └ {}–{}/{} · output",
                "│  └ {}–{}/{} · 输出",
                event.panel.offset() + 1,
                event.panel.offset() + event.panel.page,
                event.panel.rows
            );
            (paint(&clip_plain(&text, width), "muted", ui, false), true)
        };
        rows.push(Row {
            id: event.id,
            text,
            selectable: true,
            output,
        });
    }
    let hint = if width >= 68 {
        ui.language.text(
            " · left wheel inside · right wheel chat",
            " · 左半滚轮内部 · 右半滚轮对话",
        )
    } else if width >= 32 {
        ui.language
            .text(" · PgUp/PgDn · Esc back", " · PgUp/PgDn · Esc 返回")
    } else {
        ""
    };
    let detail = localized!(
        ui.language,
        "╰ {}–{}/{} rows{hint}",
        "╰ {}–{}/{} 行{hint}",
        offset + 1,
        offset + state.page,
        state.rows
    );
    if footer != 0 {
        rows.push(Row {
            id: group.head,
            text: paint(&clip_plain(&detail, width), "muted", ui, false),
            selectable: false,
            output: false,
        });
    }
    Rendered { rows, selected_row }
}
