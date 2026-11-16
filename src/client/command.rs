//! Display command arguments without executing or inferring a script's effects.
use serde_json::Value;

use super::activity;

pub(super) struct Command<'a> {
    pub invocation: String,
    pub script: Option<&'a str>,
}

impl<'a> Command<'a> {
    pub fn new(value: &'a Value) -> Self {
        let Some(args) = value.as_array() else {
            let text = value.as_str().unwrap_or("command");
            return if text.contains('\n') {
                Self {
                    invocation: "shell script".into(),
                    script: Some(text),
                }
            } else {
                Self {
                    invocation: text.into(),
                    script: None,
                }
            };
        };
        let program = args.first().and_then(Value::as_str).unwrap_or("");
        let script_index = args
            .iter()
            .enumerate()
            .skip(1)
            .take_while(|(index, _)| args[index - 1] != "--")
            .find(|(index, _)| {
                args[index - 1]
                    .as_str()
                    .is_some_and(|flag| script_flag(program, flag))
            })
            .and_then(|(index, value)| {
                value
                    .as_str()
                    .filter(|text| text.contains('\n') || text.chars().count() > 100)
                    .map(|_| index)
            });
        let invocation = args
            .iter()
            .enumerate()
            .map(|(index, value)| {
                if Some(index) == script_index {
                    "<script>".into()
                } else {
                    quote(value.as_str().unwrap_or(""))
                }
            })
            .collect::<Vec<_>>()
            .join(" ");
        Self {
            invocation,
            script: script_index.and_then(|index| args[index].as_str()),
        }
    }

    pub fn summary(&self) -> String {
        let Some(script) = self.script else {
            return activity::single_line(&self.invocation, 160);
        };
        // This is a literal source excerpt, not a claim that a file was read or
        // changed. Prefer an action over import boilerplate when one exists.
        let useful = |line: &&str| {
            let line = line.trim();
            !line.is_empty()
                && !line.starts_with('#')
                && !line.starts_with("import ")
                && !line.starts_with("from ")
                && !line.starts_with("using ")
        };
        let hint = script
            .lines()
            .find(useful)
            .or_else(|| script.lines().find(|line| !line.trim().is_empty()))
            .unwrap_or("");
        activity::single_line(&format!("{} · {}", self.invocation, hint.trim()), 160)
    }
}

fn script_flag(program: &str, flag: &str) -> bool {
    let program = program
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(program)
        .to_ascii_lowercase();
    let program = program.trim_end_matches(".exe");
    if program.starts_with("python") || program.starts_with("pypy") {
        return flag == "-c";
    }
    match program {
        "node" | "nodejs" | "bun" => matches!(flag, "-e" | "--eval" | "-p" | "--print"),
        "sh" | "bash" | "dash" | "ash" | "zsh" | "ksh" | "fish" => {
            flag.starts_with('-') && !flag.starts_with("--") && flag.contains('c')
        }
        "perl" | "ruby" => flag == "-e",
        "php" => flag == "-r",
        "powershell" | "pwsh" => flag.eq_ignore_ascii_case("-command") || flag == "-c",
        _ => false,
    }
}

// POSIX quoting is a readable argv representation. It is never passed back to
// a shell, and preserves spaces, empty arguments and literal single quotes.
fn quote(text: &str) -> String {
    if !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_alphanumeric() || "_./:=,@%+-".contains(c))
    {
        text.into()
    } else {
        format!("'{}'", text.replace('\'', "'\\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn exposes_script_actions_and_preserves_trailing_arguments() {
        let value = json!([
            "/usr/bin/python3",
            "-c",
            "from pathlib import Path\np = Path('Cargo.toml')\nprint(p.read_text())",
            "file with spaces"
        ]);
        let command = Command::new(&value);
        assert_eq!(
            command.invocation,
            "/usr/bin/python3 -c <script> 'file with spaces'"
        );
        assert!(command.summary().contains("Path('Cargo.toml')"));
        assert!(command.script.unwrap().contains("print(p.read_text())"));
    }

    #[test]
    fn ordinary_flags_are_not_mistaken_for_scripts() {
        let value = json!(["git", "-c", "long.setting=value\nsecond=value", "status"]);
        let command = Command::new(&value);
        assert!(command.script.is_none());
        assert!(command.invocation.contains("second=value"));
        assert_eq!(quote("a'b"), "'a'\\''b'");
        assert_eq!(quote(""), "''");
        let value = json!(["python3", "-c", "print(1)", "-c", "argument\nnot a script"]);
        assert!(Command::new(&value).script.is_none());
        let value = json!(["python3", "--", "-c", "argument\nnot a script"]);
        assert!(Command::new(&value).script.is_none());
    }

    #[test]
    fn handles_shell_flags_node_eval_and_multiline_command_strings() {
        for (program, flag) in [
            ("bash", "-lc"),
            ("node", "--eval"),
            ("PWSH.EXE", "-Command"),
        ] {
            let value = json!([program, flag, "first\nsecond"]);
            let command = Command::new(&value);
            assert_eq!(command.script, Some("first\nsecond"));
        }
        let value = json!("git status\ngit diff --stat");
        assert_eq!(Command::new(&value).script, value.as_str());
    }
}
