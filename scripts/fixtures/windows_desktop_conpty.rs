//! Standalone fixture run only as a disposable sibling of the Windows desktop test binary.
use std::ffi::c_void;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::Path;

#[repr(C)]
#[derive(Default)]
struct Coord {
    x: i16,
    y: i16,
}
#[repr(C)]
#[derive(Default)]
struct BufferInfo {
    size: Coord,
    cursor: Coord,
    attributes: u16,
    window: [i16; 4],
    largest: Coord,
}
#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetStdHandle(id: u32) -> *mut c_void;
    fn GetConsoleMode(handle: *mut c_void, mode: *mut u32) -> i32;
    fn SetConsoleMode(handle: *mut c_void, mode: u32) -> i32;
    fn GetConsoleScreenBufferInfo(handle: *mut c_void, info: *mut BufferInfo) -> i32;
}

fn main() {
    assert_eq!(
        std::env::args().skip(1).collect::<Vec<_>>(),
        ["fixture@http://127.0.0.1:9"]
    );
    assert!(io::stdin().is_terminal() && io::stdout().is_terminal());
    let dir = std::env::var_os("SSMCP_TEST_CONPTY_DIR").unwrap();
    let dir = Path::new(&dir);
    // Both handles are owned by the ConPTY-hosted process for its whole lifetime.
    let out = unsafe { GetStdHandle((-11i32) as u32) };
    let mut mode = 0;
    assert_ne!(unsafe { GetConsoleMode(out, &mut mode) }, 0);
    assert_ne!(unsafe { SetConsoleMode(out, mode | 4) }, 0);
    let size = || {
        let mut info = BufferInfo::default();
        assert_ne!(unsafe { GetConsoleScreenBufferInfo(out, &mut info) }, 0);
        (info.size.x, info.size.y)
    };
    let record = |stage: &str| {
        let (width, height) = size();
        let receipt = format!("{} {width} {height}\n", std::process::id());
        let pending = dir.join(format!("{stage}.tmp"));
        std::fs::write(&pending, receipt).unwrap();
        std::fs::rename(pending, dir.join(stage)).unwrap();
    };
    record("ready");
    let stdin = io::stdin();
    let mut lines = stdin.lock().lines();
    let initial_cols = size().0;
    for stage in ["first", "resized"] {
        if stage == "resized" {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(12);
            while size().0 == initial_cols {
                assert!(
                    std::time::Instant::now() < deadline,
                    "ConPTY never received a new column count"
                );
                std::thread::sleep(std::time::Duration::from_millis(35));
            }
            record("grid-changed");
        }
        assert_eq!(lines.next().unwrap().unwrap(), format!("keys-{stage}"));
        // This color is emitted by the child only after it received the expected input.
        print!("\x1b[2J\x1b[H\x1b[48;2;28;173;67m                        \x1b[0m");
        io::stdout().flush().unwrap();
        record(stage);
    }
    // Keep the real child alive until its parent closes the window.
    let _ = lines.next();
}
