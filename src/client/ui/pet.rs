//! Local, declarative ASCII footer settings; never interpreted as commands.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub(crate) const MAX_ROWS: usize = 8;
pub(crate) const MAX_COLUMNS: usize = 80;
const MAX_TEXT_BYTES: usize = 1024;

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Preset {
    #[default]
    Cat,
    Robot,
    Custom,
    Off,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Pet {
    pub preset: Preset,
    pub text: String,
}

impl Pet {
    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.text.len() > MAX_TEXT_BYTES {
            return Err("ui.pet.text must be at most 1024 bytes".into());
        }
        if self
            .text
            .bytes()
            .any(|byte| byte != b'\n' && !(b' '..=b'~').contains(&byte))
        {
            return Err("ui.pet.text must contain printable ASCII and newlines only".into());
        }
        if self.text.lines().count() > MAX_ROWS {
            return Err("ui.pet.text must have at most 8 lines".into());
        }
        if self.text.lines().any(|line| line.len() > MAX_COLUMNS) {
            return Err("ui.pet.text lines must be at most 80 columns".into());
        }
        if self.preset == Preset::Custom && self.text.trim().is_empty() {
            return Err("ui.pet.text is required for the custom preset".into());
        }
        Ok(())
    }

    pub(crate) fn lines(&self) -> Vec<&str> {
        match self.preset {
            Preset::Cat => " /\\_/\\ \n( o.o )\n > ^ < ".lines().collect(),
            Preset::Robot => " .---. \n |o_o| \n /|_|\\ ".lines().collect(),
            Preset::Custom => self.text.lines().collect(),
            Preset::Off => Vec::new(),
        }
    }

    pub(crate) fn schema() -> Value {
        json!({
            "type":"object",
            "additionalProperties":false,
            "description":"Local ASCII pet in the sidebar footer. Hidden when the complete drawing does not fit.",
            "properties":{
                "preset":{"type":"string","enum":["cat","robot","custom","off"],"default":"cat"},
                "text":{
                    "type":"string","maxLength":MAX_TEXT_BYTES,
                    "description":"Custom drawing: printable ASCII and newlines only, at most 8 lines and 80 columns per line. Required when preset is custom."
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_are_small_static_ascii_and_off_is_empty() {
        for preset in [Preset::Cat, Preset::Robot] {
            let pet = Pet {
                preset,
                ..Pet::default()
            };
            pet.validate().unwrap();
            assert_eq!(pet.lines().len(), 3);
            assert!(
                pet.lines()
                    .iter()
                    .all(|line| line.is_ascii() && line.len() == 7)
            );
        }
        assert!(
            Pet {
                preset: Preset::Off,
                ..Pet::default()
            }
            .lines()
            .is_empty()
        );
        assert_eq!(
            serde_json::to_value(Pet::default()).unwrap()["preset"],
            "cat"
        );
        assert!(serde_json::from_value::<Pet>(json!({"preset":"unknown"})).is_err());
        assert!(serde_json::from_value::<Pet>(json!({"preset":"cat","animation":true})).is_err());
    }

    #[test]
    fn custom_text_keeps_shape_but_rejects_controls_and_unbounded_drawings() {
        let text = "  /\\_/\\\n ( o.o )\n  > ^ <\n";
        let pet = Pet {
            preset: Preset::Custom,
            text: text.into(),
        };
        pet.validate().unwrap();
        assert_eq!(pet.lines(), ["  /\\_/\\", " ( o.o )", "  > ^ <"]);
        for text in [
            String::new(),
            " \n ".into(),
            "\x1b[2J".into(),
            "a\tb".into(),
            "a\rb".into(),
            "\u{7f}".into(),
            "猫".into(),
            "x".repeat(MAX_COLUMNS + 1),
            ["x"; MAX_ROWS + 1].join("\n"),
        ] {
            assert!(
                Pet {
                    preset: Preset::Custom,
                    text
                }
                .validate()
                .is_err()
            );
        }
        Pet {
            preset: Preset::Custom,
            text: vec!["x".repeat(MAX_COLUMNS); MAX_ROWS].join("\n"),
        }
        .validate()
        .unwrap();
    }
}
