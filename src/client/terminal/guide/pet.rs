//! A small text-only footer. No media, timers, subprocesses or external state.
use crate::client::ui::{Ui, pet::Preset};

// Preserve the status area that the previous guide footer reserved.
const STATUS_ROWS: usize = 8;
const SIDE_PADDING: usize = 2;

pub(crate) fn rows(width: u16, height: u16, ui: &Ui) -> Vec<String> {
    if ui.pet.validate().is_err() {
        return Vec::new();
    }
    let drawing = ui.pet.lines();
    if drawing.is_empty() {
        return Vec::new();
    }
    let width = usize::from(width);
    let height = usize::from(height);
    let drawing_width = drawing.iter().map(|line| line.len()).max().unwrap_or(0);
    let footer_height = drawing.len() + 2;
    // Keep the whole silhouette; squeezing or wrapping changes the drawing.
    if width < drawing_width + SIDE_PADDING * 2 || height < STATUS_ROWS + footer_height {
        return Vec::new();
    }
    let left = width - SIDE_PADDING - drawing_width;
    let mut rows = Vec::with_capacity(footer_height);
    rows.push(paint(&" ".repeat(width), "muted", ui));
    for (index, text) in drawing.into_iter().enumerate() {
        let line = format!(
            "{}{text}{}",
            " ".repeat(left),
            " ".repeat(width - left - text.len())
        );
        let tone = if ui.pet.preset != Preset::Custom && index == 1 {
            "accent"
        } else {
            "muted"
        };
        rows.push(paint(&line, tone, ui));
    }
    rows.push(paint(&" ".repeat(width), "muted", ui));
    rows
}

fn paint(text: &str, semantic: &str, ui: &Ui) -> String {
    if ui.color {
        format!(
            "\x1b[{};{}m{text}\x1b[0m",
            ui.background(),
            ui.color(semantic)
        )
    } else {
        text.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::ui::{Theme, pet::Pet};

    #[test]
    fn default_pet_keeps_margin_and_only_uses_its_footer_rows() {
        let ui = Ui {
            color: false,
            ..Ui::default()
        };
        let rows = rows(37, 29, &ui);
        assert_eq!(rows.len(), 5);
        assert!(
            rows.iter()
                .all(|row| row.len() == 37 && !row.contains('\x1b'))
        );
        assert_eq!(rows[2].trim(), "( o.o )");
        assert!(rows[2].ends_with("  "));
        assert!(rows[0].trim().is_empty() && rows[4].trim().is_empty());
        assert!(!rows.join("\n").contains("http"));
    }

    #[test]
    fn off_and_cramped_layouts_leave_all_space_to_the_panels() {
        let mut ui = Ui::default();
        for (width, height) in [(0, 29), (37, 0), (10, 29), (37, 12)] {
            assert!(rows(width, height, &ui).is_empty());
        }
        assert_eq!(rows(11, 13, &ui).len(), 5);
        ui.pet.preset = Preset::Off;
        assert!(rows(80, 40, &ui).is_empty());
    }

    #[test]
    fn custom_shape_is_preserved_without_clipping_or_ansi_interpretation() {
        let mut ui = Ui {
            color: false,
            pet: Pet {
                preset: Preset::Custom,
                text: " /\\\n<  >\n \\/\n".into(),
            },
            ..Ui::default()
        };
        let shown = rows(12, 20, &ui);
        assert_eq!(&shown[1][6..10], " /\\ ");
        assert_eq!(&shown[2][6..10], "<  >");
        assert_eq!(&shown[3][6..10], " \\/ ");
        assert!(rows(7, 20, &ui).is_empty());
        ui.pet.text = "\x1b[2J".into();
        assert!(rows(40, 20, &ui).is_empty());
    }

    #[test]
    fn pet_colors_follow_theme_and_semantic_overrides() {
        let mut ui = Ui {
            theme: Theme::Light,
            ..Ui::default()
        };
        ui.colors.accent = "#123456".into();
        ui.colors.muted = "#abcdef".into();
        let colored = rows(37, 29, &ui);
        assert!(colored[1].contains(&ui.color("muted")));
        assert!(colored[2].contains(&ui.color("accent")));
        assert!(colored.iter().all(|row| row.contains(&ui.background())));
        let mut screen = vt100::Parser::new(
            std::num::NonZeroU16::new(5).unwrap(),
            std::num::NonZeroU16::new(37).unwrap(),
            0,
        );
        screen.process(colored.join("\r\n").as_bytes());
        assert!(screen.screen().contents().contains("( o.o )"));
        ui.color = false;
        assert!(rows(37, 29, &ui).iter().all(|row| !row.contains('\x1b')));
    }
}
