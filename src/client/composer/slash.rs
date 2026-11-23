//! Local slash-word boundaries and insertion. Metadata never dispatches a command.
use super::{MAX_DRAFT_BYTES, SLASH_COMMANDS, SlashCommand};

#[derive(Clone, Copy, Debug)]
pub(super) struct Query<'a> {
    pub command: &'a str,
    pub prefix: &'a str,
    pub start: usize,
    pub end: usize,
    pub primary: bool,
}

impl Query<'_> {
    pub fn commands(&self) -> &'static [SlashCommand] {
        if self.primary {
            SLASH_COMMANDS
        } else {
            subcommands(self.command)
        }
    }

    pub fn matches(&self, command: &SlashCommand) -> bool {
        let token = if self.primary {
            command.name
        } else {
            command.name.split_once(' ').map_or("", |(_, token)| token)
        };
        token.starts_with(self.prefix)
    }
}

pub(super) fn query(text: &str, cursor: usize) -> Option<Query<'_>> {
    if text.contains(['\n', '\r']) || cursor > text.len() || !text.is_char_boundary(cursor) {
        return None;
    }
    let start = text.len() - text.trim_start().len();
    if cursor <= start || !text[start..].starts_with('/') {
        return None;
    }
    let first_end = text[start..]
        .find(char::is_whitespace)
        .map_or(text.len(), |offset| start + offset);
    let command = &text[start..first_end];
    if cursor <= first_end {
        return Some(Query {
            command,
            prefix: &text[start..cursor],
            start,
            end: first_end,
            primary: true,
        });
    }
    if subcommands(command).is_empty() {
        return None;
    }
    let remainder = &text[first_end..];
    let mut start = first_end + remainder.len() - remainder.trim_start().len();
    // Completing inside trailing separators inserts at the cursor, preserving
    // every other separator. A cursor before an existing word does not edit it.
    if start == text.len() {
        start = cursor;
    } else if cursor < start {
        return None;
    }
    let end = text[start..]
        .find(char::is_whitespace)
        .map_or(text.len(), |offset| start + offset);
    if cursor > end {
        return None;
    }
    Some(Query {
        command,
        prefix: &text[start..cursor],
        start,
        end,
        primary: false,
    })
}

pub(super) fn apply(text: &str, cursor: usize, selected: &str) -> Result<(String, usize), String> {
    let query =
        query(text, cursor).ok_or("The command input changed; choose a current suggestion")?;
    let replacement = if query.primary {
        selected
    } else {
        selected
            .split_once(' ')
            .filter(|(command, token)| *command == query.command && !token.is_empty())
            .map(|(_, token)| token)
            .ok_or("The command suggestions changed; choose a current subcommand")?
    };
    let count_prefix =
        !query.primary && query.command == "/worker" && matches!(replacement, "+" | "-");
    let mut end = query.end;
    if count_prefix {
        let word = &text[query.start..end];
        if let Some(digits) = word.strip_prefix(replacement)
            && !digits.is_empty()
            && digits.bytes().all(|byte| byte.is_ascii_digit())
        {
            // A cursor before an existing numeric suffix must not erase it.
            end = query.start + replacement.len();
        }
    }
    let suffix = &text[end..];
    let existing_space = suffix.chars().next().is_some_and(char::is_whitespace);
    let add_space = !count_prefix && !existing_space;
    let length = text.len() - (end - query.start) + replacement.len() + usize::from(add_space);
    if length > MAX_DRAFT_BYTES {
        return Err("Draft limit reached · 16 KiB".into());
    }
    let mut completed = String::with_capacity(length);
    completed.push_str(&text[..query.start]);
    completed.push_str(replacement);
    if add_space {
        completed.push(' ');
    }
    completed.push_str(suffix);
    let mut cursor = query.start + replacement.len();
    if !count_prefix {
        cursor += if add_space {
            1
        } else {
            suffix
                .chars()
                .take_while(|ch| ch.is_whitespace())
                .map(char::len_utf8)
                .sum()
        };
    }
    Ok((completed, cursor))
}

// These are the literal branches accepted by the conversation/family parsers.
// Argument placeholders are descriptions; Tab never inserts an invented ID.
pub(super) fn subcommands(command: &str) -> &'static [SlashCommand] {
    macro_rules! commands {
        ($($name:literal => $description:literal),* $(,)?) => {
            &[$(SlashCommand { name: $name, description: $description }),*]
        };
    }
    match command {
        "/tasks" => commands! {
            "/tasks cancel" => "ID · cancel one exact local job",
        },
        "/goal" => commands! {
            "/goal show" => "Show the current conversation goal",
            "/goal set" => "TEXT · set the conversation goal",
            "/goal clear" => "Clear the current conversation goal",
        },
        "/chatgpt" => commands! {
            "/chatgpt set" => "URL · save this conversation's ChatGPT link",
            "/chatgpt open" => "Open the saved ChatGPT link",
            "/chatgpt copy" => "Copy the saved ChatGPT link",
            "/chatgpt clear" => "Clear the saved ChatGPT link",
        },
        "/plugin" => commands! {
            "/plugin list" => "List local native plugins",
            "/plugin load" => "NAME PATH · load a native plugin",
            "/plugin unload" => "NAME · unload a native plugin",
            "/plugin help" => "Show native plugin usage",
            "/plugin -h" => "Show native plugin usage",
            "/plugin --help" => "Show native plugin usage",
        },
        "/wait" => commands! {
            "/wait cancel" => "ID · cancel one user wait or timer",
        },
        "/diff" => commands! {
            "/diff single" => "Show changes in one column",
            "/diff split" => "Show old and new lines side by side",
        },
        "/allow" => commands! {
            "/allow add" => "USER · grant this session's access",
            "/allow remove" => "USER · revoke this session's access",
        },
        "/login" => commands! {
            "/login open" => "Open the pending GitHub sign-in link",
            "/login copy" => "Copy the pending GitHub sign-in link",
            "/login link" => "Show the pending GitHub sign-in link",
        },
        "/new" => commands! {
            "/new chat" => "Add a conversation panel",
            "/new statistics" => "Add a statistics panel",
            "/new audit" => "Add a code audit panel",
            "/new queue" => "Add a queued requests panel",
            "/new questions" => "Add a questions panel",
            "/new conversation" => "Add a conversation panel (alias of chat)",
        },
        "/filter" => commands! {
            "/filter --all" => "Show every log category",
            "/filter --commands" => "Show command logs",
            "/filter --errors" => "Show error logs",
        },
        "/theme" => commands! {
            "/theme dark" => "Dark theme",
            "/theme minimal" => "Minimal theme",
            "/theme codex-dark" => "Codex dark theme",
            "/theme nord" => "Nord theme",
            "/theme solarized" => "Solarized theme",
            "/theme light" => "Light theme",
        },
        "/group" => commands! {
            "/group pick" => "Choose exact message recipients",
            "/group list" => "Show selected recipients and saved groups",
            "/group add" => "EXACT_ID… · add verified recipients",
            "/group remove" => "EXACT_ID… · remove selected recipients",
            "/group clear" => "Clear this local recipient selection",
            "/group save" => "NAME · save the selected recipients",
            "/group use" => "NAME · select a saved group",
            "/group send" => "TEXT · send to the selected group",
            "/group status" => "[ID] · inspect retained group receipts",
            "/group retry" => "[ID] · retry unconfirmed failed targets",
        },
        "/paste-retry" => commands! {
            "/paste-retry discard" => "Discard the retained failed paste",
        },
        "/invite" => commands! {
            "/invite worker" => "Prepare an invitation for an external worker",
        },
        "/worker" => commands! {
            "/worker ls" => "List worker stations",
            "/worker +" => "N · add 1–128 stations; type the count next",
            "/worker -" => "N · retire 1–128 stations; type the count next",
        },
        _ => &[],
    }
}
