//! Single-column compatibility and bounded side-by-side unified diff previews.
use super::{Ui, paint, semantic_text, styled_rows, unstyled};
use unicode_width::UnicodeWidthStr;

const MIN_SPLIT_WIDTH: usize = 64;
const MIN_CONTENT_WIDTH: usize = 16;
const SEPARATOR: &str = " │ ";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum DiffMode {
    #[default]
    Single,
    Split,
}

impl DiffMode {
    pub(crate) fn parse(value: &str) -> Result<Self, String> {
        match value {
            "single" => Ok(Self::Single),
            "split" => Ok(Self::Split),
            _ => Err("Use /diff single or /diff split".into()),
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Single => "single",
            Self::Split => "split",
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Cell<'a> {
    number: Option<usize>,
    marker: char,
    text: &'a str,
    note: Option<&'a str>,
}

enum Row<'a> {
    Shared(&'a str),
    Pair(Option<Cell<'a>>, Option<Cell<'a>>),
}

struct Hunk {
    old: usize,
    new: usize,
    old_left: usize,
    new_left: usize,
}

impl Hunk {
    fn parse(line: &str) -> Option<Self> {
        fn range(field: &str, prefix: char) -> Option<(usize, usize)> {
            let field = field.strip_prefix(prefix)?;
            let (start, count) = field.split_once(',').unwrap_or((field, "1"));
            Some((start.parse().ok()?, count.parse().ok()?))
        }
        let mut fields = line.strip_prefix("@@ ")?.split_whitespace();
        let (old, old_left) = range(fields.next()?, '-')?;
        let (new, new_left) = range(fields.next()?, '+')?;
        (fields.next()? == "@@").then_some(Self {
            old,
            new,
            old_left,
            new_left,
        })
    }

    fn complete(&self) -> bool {
        self.old_left == 0 && self.new_left == 0
    }

    /// The existing native renderer prefixes hunk lines with both line numbers.
    /// Match its complete expected gutter, never arbitrary source indentation.
    fn source<'a>(&self, line: &'a str) -> &'a str {
        for (prefix, marker) in [
            (format!("{:>5}       │ ", self.old), '-'),
            (format!("      {:>5} │ ", self.new), '+'),
            (format!("{:>5} {:>5} │ ", self.old, self.new), ' '),
        ] {
            if let Some(body) = line.strip_prefix(&prefix)
                && body.starts_with(marker)
            {
                return body;
            }
        }
        line
    }
}

fn flush<'a>(rows: &mut Vec<Row<'a>>, removed: &mut Vec<Cell<'a>>, added: &mut Vec<Cell<'a>>) {
    let count = removed.len().max(added.len());
    let mut old = std::mem::take(removed).into_iter();
    let mut new = std::mem::take(added).into_iter();
    for _ in 0..count {
        rows.push(Row::Pair(old.next(), new.next()));
    }
}

/// Unrecognized or incomplete hunks use the original single-column rendering.
fn parse(text: &str) -> Option<Vec<Row<'_>>> {
    let mut rows = Vec::new();
    let mut removed: Vec<Cell<'_>> = Vec::new();
    let mut added: Vec<Cell<'_>> = Vec::new();
    let mut hunk: Option<Hunk> = None;
    let mut saw_hunk = false;
    let mut changed = false;

    for line in text.lines() {
        if let Some(next) = Hunk::parse(line) {
            if hunk.as_ref().is_some_and(|current| !current.complete()) {
                return None;
            }
            flush(&mut rows, &mut removed, &mut added);
            rows.push(Row::Shared(line));
            hunk = Some(next);
            saw_hunk = true;
            changed = true;
            continue;
        }
        if line.starts_with("@@") {
            return None;
        }
        if line == "\\ No newline at end of file" {
            if let Some(cell) = added.last_mut().or_else(|| removed.last_mut()) {
                cell.note = Some(line);
            } else if let Some(Row::Pair(old, new)) = rows.last_mut() {
                for cell in [old.as_mut(), new.as_mut()].into_iter().flatten() {
                    cell.note = Some(line);
                }
            } else {
                rows.push(Row::Shared(line));
            }
            continue;
        }
        if hunk.as_ref().is_some_and(Hunk::complete) {
            flush(&mut rows, &mut removed, &mut added);
            hunk = None;
        }
        if let Some(current) = hunk.as_mut() {
            let source = current.source(line);
            match source.as_bytes().first().copied() {
                Some(b'-') if current.old_left > 0 => {
                    removed.push(Cell {
                        number: Some(current.old),
                        marker: '-',
                        text: &source[1..],
                        note: None,
                    });
                    current.old = current.old.saturating_add(1);
                    current.old_left -= 1;
                }
                Some(b'+') if current.new_left > 0 => {
                    added.push(Cell {
                        number: Some(current.new),
                        marker: '+',
                        text: &source[1..],
                        note: None,
                    });
                    current.new = current.new.saturating_add(1);
                    current.new_left -= 1;
                }
                Some(b' ') if current.old_left > 0 && current.new_left > 0 => {
                    flush(&mut rows, &mut removed, &mut added);
                    let old = Cell {
                        number: Some(current.old),
                        marker: ' ',
                        text: &source[1..],
                        note: None,
                    };
                    rows.push(Row::Pair(
                        Some(old),
                        Some(Cell {
                            number: Some(current.new),
                            ..old
                        }),
                    ));
                    current.old = current.old.saturating_add(1);
                    current.new = current.new.saturating_add(1);
                    current.old_left -= 1;
                    current.new_left -= 1;
                }
                _ => return None,
            }
            continue;
        }

        // Small native previews may contain only +/- lines without a hunk header.
        let marker = line.as_bytes().first().copied();
        let file_header = line.starts_with("--- ") || line.starts_with("+++ ");
        if !saw_hunk && !file_header && matches!(marker, Some(b'-' | b'+')) {
            let marker = char::from(marker.unwrap());
            let cell = Cell {
                number: None,
                marker,
                text: &line[1..],
                note: None,
            };
            if marker == '-' {
                removed.push(cell);
            } else {
                added.push(cell);
            }
            changed = true;
        } else {
            flush(&mut rows, &mut removed, &mut added);
            rows.push(Row::Shared(line));
        }
    }
    if hunk.as_ref().is_some_and(|current| !current.complete()) {
        return None;
    }
    flush(&mut rows, &mut removed, &mut added);
    changed.then_some(rows)
}

fn cell_rows(cell: Option<Cell<'_>>, width: usize, digits: usize, ui: &Ui) -> Vec<String> {
    let Some(cell) = cell else {
        return Vec::new();
    };
    let content_width = width - digits - 3;
    let number = cell
        .number
        .map(|number| number.to_string())
        .unwrap_or_default();
    let tone = match cell.marker {
        '-' => "error",
        '+' => "success",
        _ => "foreground",
    };
    let mut rows = Vec::new();
    for (index, body) in styled_rows(cell.text, content_width, false)
        .into_iter()
        .enumerate()
    {
        let prefix = if index == 0 {
            format!("{number:>digits$} {} ", cell.marker)
        } else {
            " ".repeat(digits + 3)
        };
        rows.push(paint(&format!("{prefix}{body}"), tone, ui, false));
    }
    if let Some(note) = cell.note {
        for body in styled_rows(note, content_width, false) {
            rows.push(paint(
                &format!("{}{body}", " ".repeat(digits + 3)),
                "muted",
                ui,
                false,
            ));
        }
    }
    rows
}

fn padded(text: &str, width: usize) -> String {
    format!(
        "{text}{}",
        " ".repeat(width.saturating_sub(unstyled(text).width()))
    )
}

fn split(text: &str, width: usize, ui: &Ui) -> Option<Vec<String>> {
    if width < MIN_SPLIT_WIDTH {
        return None;
    }
    let plain = unstyled(text);
    let parsed = parse(&plain)?;
    let digits = parsed
        .iter()
        .filter_map(|row| match row {
            Row::Pair(old, new) => Some([old, new]),
            Row::Shared(_) => None,
        })
        .flatten()
        .filter_map(|cell| cell.as_ref().and_then(|cell| cell.number))
        .map(|number| number.to_string().len())
        .max()
        .unwrap_or(1);
    let left_width = (width - SEPARATOR.width()) / 2;
    let right_width = width - SEPARATOR.width() - left_width;
    if left_width.min(right_width) < digits + 3 + MIN_CONTENT_WIDTH {
        return None;
    }
    let separator = paint(SEPARATOR, "muted", ui, false);
    let mut rows = vec![format!(
        "{}{separator}{}",
        padded(&paint("Old", "muted", ui, true), left_width),
        padded(&paint("New", "muted", ui, true), right_width)
    )];
    for row in parsed {
        match row {
            Row::Shared(line) => {
                rows.extend(styled_rows(&semantic_text(line, ui, ""), width, ui.color))
            }
            Row::Pair(old, new) => {
                let old = cell_rows(old, left_width, digits, ui);
                let new = cell_rows(new, right_width, digits, ui);
                for index in 0..old.len().max(new.len()) {
                    rows.push(format!(
                        "{}{separator}{}",
                        padded(old.get(index).map_or("", String::as_str), left_width),
                        padded(new.get(index).map_or("", String::as_str), right_width),
                    ));
                }
            }
        }
    }
    Some(rows)
}

pub(super) fn rows(
    text: &str,
    width: usize,
    ui: &Ui,
    language: &str,
    mode: DiffMode,
) -> Vec<String> {
    if mode == DiffMode::Split
        && let Some(rows) = split(text, width, ui)
    {
        return rows;
    }
    styled_rows(&semantic_text(text, ui, language), width, ui.color)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ui() -> Ui {
        Ui {
            color: false,
            ..Ui::default()
        }
    }

    #[test]
    fn mode_names_are_explicit_and_single_is_the_default() {
        assert_eq!(DiffMode::default(), DiffMode::Single);
        for mode in [DiffMode::Single, DiffMode::Split] {
            assert_eq!(DiffMode::parse(mode.name()).unwrap(), mode);
        }
        assert!(DiffMode::parse("sideways").is_err());
    }

    #[test]
    fn unified_changes_pair_context_and_keep_unequal_change_blocks() {
        let text = "diff --git a/demo b/demo\n--- a/demo\n+++ b/demo\n@@ -3,4 +3,5 @@\n context\n-old one\n-old two\n+new one\n+new two\n+new three\n tail\n";
        let parsed = parse(text).unwrap();
        let pairs = parsed
            .iter()
            .filter_map(|row| match row {
                Row::Pair(old, new) => Some((
                    old.map(|cell| (cell.number, cell.text)),
                    new.map(|cell| (cell.number, cell.text)),
                )),
                Row::Shared(_) => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            pairs,
            vec![
                (Some((Some(3), "context")), Some((Some(3), "context"))),
                (Some((Some(4), "old one")), Some((Some(4), "new one"))),
                (Some((Some(5), "old two")), Some((Some(5), "new two"))),
                (None, Some((Some(6), "new three"))),
                (Some((Some(6), "tail")), Some((Some(7), "tail"))),
            ]
        );
        let rendered = rows(text, 100, &ui(), "", DiffMode::Split).join("\n");
        for header in [
            "diff --git a/demo b/demo",
            "--- a/demo",
            "+++ b/demo",
            "@@ -3,4 +3,5 @@",
        ] {
            assert!(rendered.contains(header), "{rendered}");
        }
    }

    #[test]
    fn insertions_deletions_and_no_newline_notes_keep_their_own_sides() {
        let text = "@@ -0,0 +1,2 @@\n+new first\n+new last\n\\ No newline at end of file\n@@ -5,2 +7,0 @@\n-old first\n-old last\n";
        let parsed = parse(text).unwrap();
        let pairs = parsed
            .iter()
            .filter_map(|row| match row {
                Row::Pair(old, new) => Some((old, new)),
                Row::Shared(_) => None,
            })
            .collect::<Vec<_>>();
        assert!(pairs[0].0.is_none());
        assert_eq!(
            pairs[1].1.unwrap().note,
            Some("\\ No newline at end of file")
        );
        assert!(pairs[2].1.is_none());
        assert_eq!(pairs[3].0.unwrap().number, Some(6));
        let rendered = rows(text, 100, &ui(), "", DiffMode::Split).join("\n");
        assert!(rendered.contains("new last") && rendered.contains("old last"));
        assert!(rendered.contains("\\ No newline at end of file"));
    }

    #[test]
    fn numbered_native_diff_rows_match_the_original_unified_diff() {
        let raw = "@@ -2,2 +2,3 @@\n-old\n+new one\n+new two\n same\n";
        let native = "@@ -2,2 +2,3 @@\n    2       │ -old\n          2 │ +new one\n          3 │ +new two\n    3     4 │  same\n";
        assert_eq!(
            rows(raw, 100, &ui(), "", DiffMode::Split),
            rows(native, 100, &ui(), "", DiffMode::Split)
        );
    }

    #[test]
    fn long_unicode_cells_wrap_without_losing_characters_or_crossing_the_separator() {
        let old = "中文甲乙e\u{301}XYZ".repeat(12);
        let new = "新版本内容abcdef".repeat(11);
        let text = format!("@@ -1 +1 @@\n-{old}\n+{new}\n");
        for width in [64, 81] {
            let rendered = rows(&text, width, &ui(), "", DiffMode::Split);
            assert!(rendered.iter().all(|row| row.width() <= width));
            let mut old_text = String::new();
            let mut new_text = String::new();
            for row in rendered.iter().skip(1) {
                if let Some((left, right)) = row.split_once(SEPARATOR) {
                    old_text.push_str(left.get(4..).unwrap().trim_end());
                    new_text.push_str(right.get(4..).unwrap().trim_end());
                }
            }
            assert_eq!(old_text, old);
            assert_eq!(new_text, new);
        }
    }

    #[test]
    fn narrow_or_incomplete_diffs_preserve_the_single_column_output() {
        let text = "@@ -1 +1 @@\n-旧内容不会丢失\n+新增内容完整保留\n";
        for width in [20, 40, MIN_SPLIT_WIDTH - 1] {
            assert_eq!(
                rows(text, width, &ui(), "", DiffMode::Split),
                rows(text, width, &ui(), "", DiffMode::Single)
            );
        }
        let partial = "@@ -1,9 +1 @@\n-old\n+new\n";
        assert_eq!(
            rows(partial, 100, &ui(), "", DiffMode::Split),
            rows(partial, 100, &ui(), "", DiffMode::Single)
        );
    }
}
