//! JSONL decoder for the real PTY transcript used by e2e_dashboard.py.
//! This example never reads a physical terminal or installs dependencies.
use base64::Engine as _;
use serde::Deserialize;
use serde_json::{Value, json};
use std::io::{self, BufRead as _, Write as _};
use std::num::NonZeroU16;

const MAX_LINE_BYTES: usize = 2 * 1024 * 1024;
const MAX_FEED_BYTES: usize = 1024 * 1024;

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    Feed {
        base64: String,
    },
    Resize {
        rows: u16,
        cols: u16,
    },
    Snapshot {
        #[serde(default)]
        cells: bool,
    },
    Quit,
}

fn size(value: u16) -> Result<NonZeroU16, String> {
    if value > 1000 {
        return Err("screen dimensions must be between 1 and 1000".into());
    }
    NonZeroU16::new(value).ok_or_else(|| "screen dimensions must be between 1 and 1000".into())
}

fn color(value: vt100::Color) -> Value {
    match value {
        vt100::Color::Default => Value::Null,
        vt100::Color::Idx(index) => json!(index),
        vt100::Color::Rgb(r, g, b) => json!([r, g, b]),
    }
}

fn handle(parser: &mut vt100::Parser, request: Request) -> Result<Value, String> {
    match request {
        Request::Feed { base64 } => {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(base64)
                .map_err(|error| error.to_string())?;
            if bytes.len() > MAX_FEED_BYTES {
                return Err("feed exceeds one MiB".into());
            }
            parser.process(&bytes);
            Ok(json!({"ok": true}))
        }
        Request::Resize { rows, cols } => {
            parser.set_size(size(rows)?, size(cols)?);
            Ok(json!({"ok": true}))
        }
        Request::Snapshot { cells } => {
            let screen = parser.screen();
            let (rows, cols) = screen.size();
            let (row, col) = screen.cursor_position();
            let mut result = json!({
                "ok": true,
                "size": [rows.get(), cols.get()],
                "rows": screen.rows(0, cols.get()).collect::<Vec<_>>(),
                "cursor": [row, col],
                "alternate_screen": screen.alternate_screen(),
                "hide_cursor": screen.hide_cursor(),
                "application_cursor": screen.application_cursor(),
                "application_keypad": screen.application_keypad(),
                "bracketed_paste": screen.bracketed_paste()
            });
            if cells {
                let grid = (0..rows.get())
                    .map(|row| {
                        (0..cols.get())
                            .map(|col| {
                                let cell = screen.cell(row, col).expect("valid screen position");
                                json!({
                                    "text": cell.contents(),
                                    "wide": cell.is_wide(),
                                    "continuation": cell.is_wide_continuation(),
                                    "foreground": color(cell.fgcolor()),
                                    "background": color(cell.bgcolor()),
                                    "bold": cell.bold(),
                                    "dim": cell.dim(),
                                    "inverse": cell.inverse()
                                })
                            })
                            .collect::<Vec<_>>()
                    })
                    .collect::<Vec<_>>();
                result["cells"] = json!(grid);
            }
            Ok(result)
        }
        Request::Quit => Ok(json!({"ok": true})),
    }
}

fn main() -> io::Result<()> {
    let mut parser = vt100::Parser::new(
        NonZeroU16::new(24).expect("nonzero rows"),
        NonZeroU16::new(80).expect("nonzero columns"),
        0,
    );
    let stdin = io::stdin();
    let mut input = stdin.lock();
    let stdout = io::stdout();
    let mut output = stdout.lock();
    let mut line = String::new();
    loop {
        line.clear();
        if input.read_line(&mut line)? == 0 {
            break;
        }
        let request = if line.len() > MAX_LINE_BYTES {
            Err("request exceeds two MiB".to_string())
        } else {
            serde_json::from_str::<Request>(&line).map_err(|error| error.to_string())
        };
        let quit = matches!(&request, Ok(Request::Quit));
        let result = request.and_then(|request| handle(&mut parser, request));
        let reply = match result {
            Ok(value) => value,
            Err(error) => json!({"ok": false, "error": error}),
        };
        serde_json::to_writer(&mut output, &reply)?;
        output.write_all(b"\n")?;
        output.flush()?;
        if quit {
            break;
        }
    }
    Ok(())
}
