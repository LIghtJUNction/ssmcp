//! Project the final PTY cells, including their measured terminal width, to the WebUI.
use serde::Serialize;

#[derive(Clone, Copy, Default, PartialEq, Eq, Serialize)]
struct Attributes {
    #[serde(skip_serializing_if = "Option::is_none")]
    fg: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bg: Option<u32>,
    flags: u8,
}

// Indexed colors occupy 0..=255. The marker keeps RGB black distinct from palette entry zero.
fn color(value: vt100::Color) -> Option<u32> {
    match value {
        vt100::Color::Default => None,
        vt100::Color::Idx(i) => Some(u32::from(i)),
        vt100::Color::Rgb(r, g, b) => Some(u32::from_be_bytes([1, r, g, b])),
    }
}

#[derive(Serialize)]
pub(super) struct Run(u16, String, Attributes);

pub(super) fn rows(screen: &vt100::Screen) -> Vec<Vec<Run>> {
    let (height, width) = screen.size();
    (0..height.get())
        .map(|row| {
            let mut runs: Vec<Run> = Vec::new();
            let mut previous_ascii = false;
            for col in 0..width.get() {
                let Some(cell) = screen.cell(row, col) else {
                    continue;
                };
                if cell.is_wide_continuation() {
                    continue;
                }
                let attributes = Attributes {
                    fg: color(cell.fgcolor()),
                    bg: color(cell.bgcolor()),
                    flags: u8::from(cell.bold())
                        | (u8::from(cell.dim()) << 1)
                        | (u8::from(cell.italic()) << 2)
                        | (u8::from(cell.underline()) << 3)
                        | (u8::from(cell.inverse()) << 4),
                };
                let contents = cell.contents();
                let text = if contents.is_empty() { " " } else { contents };
                let ascii = text.len() == 1 && text.is_ascii();
                if ascii
                    && previous_ascii
                    && let Some(last) = runs.last_mut()
                    && last.2 == attributes
                {
                    last.0 += 1;
                    last.1.push_str(text);
                } else {
                    runs.push(Run(
                        if cell.is_wide() { 2 } else { 1 },
                        text.into(),
                        attributes,
                    ));
                }
                previous_ascii = ascii;
            }
            // Omit only default trailing blanks. Colored blank cells paint a background.
            if let Some(last) = runs.last_mut()
                && last.2 == Attributes::default()
            {
                let keep = last.1.trim_end_matches(' ').len();
                last.0 -= (last.1.len() - keep) as u16;
                last.1.truncate(keep);
                if last.0 == 0 {
                    runs.pop();
                }
            }
            runs
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cells_preserve_columns_styles_and_background_spaces() {
        let mut p = vt100::Parser::default();
        p.process("A中e\u{301}\x1b[38;5;42;48;2;1;2;3;1mX \x1b[0m".as_bytes());
        let rows = rows(p.screen());
        let row = &rows[0];
        assert_eq!(
            row.iter().map(|run| run.0).collect::<Vec<_>>(),
            [1, 2, 1, 2]
        );
        assert_eq!(row[2].1, "e\u{301}");
        assert_eq!(row[3].1, "X ");
        assert_eq!(row[3].2.fg, Some(42));
        assert_eq!(row[3].2.bg, Some(0x01010203));
        assert_eq!(row[3].2.flags, 1);
    }
}
