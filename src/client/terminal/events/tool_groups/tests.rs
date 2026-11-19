use super::*;
use crate::{
    backend::Backend,
    output::OutputStore,
    runtime::Runtime,
    wire::{CodeContext, ControlAccount, ControlAgent, ControlAttribution},
};
use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::Instant,
};

static NEXT: AtomicU64 = AtomicU64::new(1);

struct Fixture {
    runtime: Arc<Runtime>,
    root: PathBuf,
}

impl Fixture {
    async fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "ssmcp-tool-groups-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&root).unwrap();
        let output = OutputStore::at_for_test(root.join("output")).unwrap();
        let backend = Backend::with_output(Path::new("/bin/sh"), root.clone(), output)
            .await
            .unwrap();
        let runtime = Runtime::new(backend, root.clone()).unwrap();
        Self { runtime, root }
    }

    fn scope(&self, request: u64, subject: &str, run: char, agent: Option<char>) -> Arc<CallScope> {
        self.runtime
            .call_scope(
                request,
                Some(&ControlAttribution {
                    account: ControlAccount {
                        provider: "github".into(),
                        subject: subject.into(),
                        login: "same-label".into(),
                    },
                    owner: None,
                    agent: agent.map(|id| ControlAgent {
                        id: format!("agent_{}", id.to_string().repeat(32)),
                        name: "same-agent-label".into(),
                    }),
                }),
                Some(&CodeContext {
                    run_id: run.to_string().repeat(32),
                    code: "native source fixture".into(),
                    timestamp_ms: 1,
                    session_id: None,
                    thread_id: None,
                }),
            )
            .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn ui() -> Ui {
    Ui {
        color: false,
        ..Ui::default()
    }
}

fn tool(view: &mut EventView, scope: Option<&Arc<CallScope>>, id: usize, lines: usize) {
    let job = format!("group-{id}");
    view.ingest_scoped_event(
        "tools",
        &json!({
            "type":"exec.started", "job_id":job,
        }),
        format!("[RUN] tools #{job} command-{id} 界🙂 started\n").as_bytes(),
        scope.cloned(),
    );
    let output = (0..lines)
        .map(|line| format!("result-{id}-{line:03} 界🙂\n"))
        .collect::<String>();
    view.ingest_scoped_event(
        "tools",
        &json!({
            "type":"exec.stdout", "job_id":job,
        }),
        format!("[STDOUT] tools #{job}\n{output}").as_bytes(),
        scope.cloned(),
    );
}

fn view(scope: &Arc<CallScope>, count: usize) -> EventView {
    let mut view = EventView::new();
    view.set_scope(EventScope::Conversation);
    for index in 0..count {
        tool(&mut view, Some(scope), index, 24);
    }
    view
}

#[tokio::test]
async fn tool_containers_use_verified_identity_and_never_display_names() {
    let fixture = Fixture::new().await;
    let other_connection = Fixture::new().await;
    let a = fixture.scope(1, "123", 'a', None);
    let same_run = fixture.scope(2, "123", 'a', None);
    let other_run = fixture.scope(3, "123", 'b', None);
    let other_account = fixture.scope(4, "456", 'a', None);
    let other = other_connection.scope(1, "123", 'a', None);
    assert!(same_sender(&a, &same_run));
    assert!(!same_sender(&a, &other_run));
    assert!(!same_sender(&a, &other_account));
    assert!(!same_sender(&a, &other));
    let agent = fixture.scope(5, "123", 'a', Some('a'));
    let same_agent = fixture.scope(6, "123", 'b', Some('a'));
    let other_agent = fixture.scope(7, "123", 'a', Some('b'));
    assert!(same_sender(&agent, &same_agent));
    assert!(!same_sender(&agent, &other_agent));
    assert!(!same_sender(&agent, &a));
    let mut v = EventView::new();
    v.set_scope(EventScope::Conversation);
    for (index, source) in [Some(&a), Some(&same_run), Some(&other_account), None, None]
        .into_iter()
        .enumerate()
    {
        tool(&mut v, source, index, 1);
    }
    let groups = v.tool_groups();
    assert_eq!(groups.len(), 1);
    assert_eq!((groups[0].start, groups[0].end), (0, 2));
    assert_eq!(
        v.visible_ids().len(),
        5,
        "grouping keeps every child selectable"
    );
}

#[tokio::test]
async fn tool_containers_bound_height_and_cell_width_with_real_hit_rows() {
    let fixture = Fixture::new().await;
    let scope = fixture.scope(1, "123", 'a', None);
    let mut v = view(&scope, 30);
    for width in [1, 2, 3, 9, 31, 68, 100] {
        for height in [1, 2, 3, 8, 24] {
            let rows = v.render(width, height, &ui(), true);
            assert_eq!(rows.len(), usize::from(height));
            assert!(rows.iter().all(|row| UnicodeWidthStr::width(unstyled(row).as_str()) <= usize::from(width)), "{width}x{height}: {rows:?}");
            assert!(v.total_rows <= MAX_BODY_ROWS + 2);
            assert_eq!(v.tool_hit_rows.len(), usize::from(height));
            for (index, hit) in v.tool_hit_rows.iter().enumerate() {
                if let Some(hit) = hit {
                    assert_eq!(v.hit_rows[index], hit.child);
                    if let Some(id) = hit.child {
                        assert!(v.events.iter().any(|event| event.id == id));
                    }
                }
            }
        }
    }
    let rows = v.render(80, 20, &ui(), true);
    let border = rows.iter().position(|row| row.starts_with('╭')).unwrap();
    let selected = v.selected;
    assert!(!v.mouse_select(border as u16));
    assert_eq!(v.selected, selected, "borders are not disguised child rows");
}

#[tokio::test]
async fn tool_containers_chain_inner_then_group_and_release_at_both_edges() {
    let fixture = Fixture::new().await;
    let scope = fixture.scope(1, "123", 'a', None);
    let mut v = view(&scope, 12);
    v.selected = Some(v.events[3].id);
    v.open();
    v.render(100, 20, &ui(), true);
    let head = v.tool_groups()[0].head;
    let child = v.events[3].id;
    let outer = v.tool_positions[&head].offset();
    let row = v
        .tool_hit_rows
        .iter()
        .position(|hit| hit.is_some_and(|hit| hit.child == Some(child) && hit.output))
        .unwrap();
    assert_eq!(v.events[3].panel.offset(), 0);
    assert!(v.scroll_layer_at(row as u16, 2));
    assert_eq!(v.events[3].panel.offset(), 2);
    assert_eq!(v.tool_positions[&head].offset(), outer);
    assert!(v.scroll_selected_layer(isize::MAX));
    let inner_end = v.events[3].panel.offset();
    assert!(inner_end > 2);
    assert_eq!(v.tool_positions[&head].offset(), outer);
    assert!(v.scroll_selected_layer(isize::MAX));
    assert!(v.tool_positions[&head].following());
    assert_eq!(v.events[3].panel.offset(), inner_end);
    assert!(
        !v.scroll_selected_layer(3),
        "both ends pass the next wheel/page to the conversation"
    );
    assert!(v.scroll_selected_layer(isize::MIN));
    assert_eq!(v.events[3].panel.offset(), 0);
    assert!(v.scroll_selected_layer(isize::MIN));
    assert_eq!(v.tool_positions[&head].offset(), 0);
    assert!(!v.scroll_selected_layer(-3));
}

#[tokio::test]
async fn tool_containers_preserve_child_output_and_column_positions() {
    let fixture = Fixture::new().await;
    let scope = fixture.scope(1, "123", 'a', None);
    let mut v = view(&scope, 10);
    v.selected = Some(v.events[2].id);
    v.open();
    v.render(100, 20, &ui(), true);
    assert!(v.scroll_selected_layer(7));
    let first_inner = v.events[2].panel.offset();
    let saved = v.position();
    let first_outer = v.tool_positions[&v.events[0].id].offset();
    v.selected = Some(v.events[8].id);
    v.open();
    v.render(100, 20, &ui(), true);
    assert!(v.scroll_selected_layer(4));
    assert_eq!(v.events[2].panel.offset(), first_inner);
    v.restore_position(&saved);
    v.render(100, 20, &ui(), true);
    assert_eq!(v.selected, saved.selected);
    assert_eq!(v.events[2].panel.offset(), first_inner);
    assert!(!v.events[8].chat_expanded);
    assert_eq!(v.tool_positions[&v.events[0].id].offset(), first_outer);
    v.filter("result-2-", 0);
    assert!(
        !v.scroll_layer_at(2, 3),
        "filter invalidates old hits immediately"
    );
    assert!(v.snapshot()["tool_groups"].as_array().unwrap().is_empty());
    v.render(80, 12, &ui(), true);
    v.filter("", 0);
    v.restore_position(&saved);
    v.render(100, 20, &ui(), true);
    assert_eq!(v.tool_positions[&v.events[0].id].offset(), first_outer);
}

#[tokio::test]
async fn tool_containers_append_and_trim_keep_an_existing_anchor_by_event_id() {
    let fixture = Fixture::new().await;
    let scope = fixture.scope(1, "123", 'a', None);
    let mut v = view(&scope, MAX_EVENTS);
    v.render(80, 20, &ui(), true);
    let head = v.events[0].id;
    assert!(v.scroll_tool_group(head, None, -50));
    let anchor = v.tool_positions[&head].anchor.unwrap();
    let old_offset = v.tool_positions[&head].offset();
    tool(&mut v, Some(&scope), MAX_EVENTS, 2);
    v.render(80, 20, &ui(), true);
    let head = v.events[0].id;
    let state = &v.tool_positions[&head];
    assert_eq!(state.anchor.unwrap().id, anchor.id);
    assert_eq!(state.offset(), old_offset - 1);
    assert!(!state.following());
    assert_eq!(state.members.len(), MAX_EVENTS);
    // Evict the selected anchor while retaining later members of the same run.
    for index in MAX_EVENTS + 1..MAX_EVENTS + old_offset + 2 {
        tool(&mut v, Some(&scope), index, 1);
    }
    v.render(80, 20, &ui(), true);
    assert_eq!(v.tool_positions[&v.events[0].id].offset(), 0);
    assert_eq!(v.tool_positions.len(), 1);
    assert!(
        v.tool_positions
            .values()
            .map(|state| state.members.len())
            .sum::<usize>()
            <= MAX_EVENTS
    );
}

#[tokio::test]
async fn tool_containers_keyboard_reveals_hidden_children_and_latest_resumes_outer_follow() {
    let fixture = Fixture::new().await;
    let scope = fixture.scope(1, "123", 'a', None);
    let mut v = view(&scope, 20);
    v.render(80, 16, &ui(), true);
    v.select(isize::MIN);
    v.render(80, 16, &ui(), true);
    assert_eq!(v.selected, Some(v.events[0].id));
    assert!(v.hit_rows.contains(&v.selected));
    assert_eq!(v.tool_positions[&v.events[0].id].offset(), 0);
    v.select(15);
    v.open();
    v.render(80, 16, &ui(), true);
    assert!(v.hit_rows.contains(&v.selected));
    assert!(v.events[15].chat_expanded);
    assert!(v.back());
    assert!(!v.events[15].chat_expanded);
    v.latest();
    v.render(80, 16, &ui(), true);
    assert!(v.follow);
    assert!(v.tool_positions[&v.events[0].id].following());
    assert_eq!(v.selected, Some(v.events[19].id));
}

#[tokio::test]
async fn tool_containers_new_output_does_not_hide_the_child_being_read() {
    let fixture = Fixture::new().await;
    let scope = fixture.scope(1, "123", 'a', None);
    let mut v = view(&scope, 2);
    v.selected = Some(v.events[0].id);
    v.open();
    v.render(80, 20, &ui(), true);
    assert!(v.scroll_selected_layer(3));
    let selected = v.selected;
    let first = v.tool_positions[&v.events[0].id].offset();
    for index in 2..15 {
        tool(&mut v, Some(&scope), index, 2);
    }
    v.render(80, 20, &ui(), true);
    assert_eq!(v.selected, selected);
    assert_eq!(v.tool_positions[&v.events[0].id].offset(), first);
    assert!(v.hit_rows.contains(&selected));
    assert_eq!(v.events[0].panel.offset(), 3);
}

#[tokio::test]
async fn tool_containers_do_not_double_project_read_summaries_or_keep_stale_group_hits() {
    let fixture = Fixture::new().await;
    let scope = fixture.scope(1, "123", 'a', None);
    let mut v = EventView::new();
    v.set_scope(EventScope::Conversation);
    for index in 0..5 {
        v.ingest_scoped_event(
            "workspace",
            &json!({"type":"file.read","path":format!("src/file-{index}.rs")}),
            format!("[READ] src/file-{index}.rs\nretained body {index}\n").as_bytes(),
            Some(scope.clone()),
        );
    }
    assert_eq!(v.tool_groups().len(), 1);
    assert!(v.read_groups().is_empty());
    v.render(100, 16, &ui(), true);
    assert_eq!(v.visible_ids().len(), 5);
    v.set_scope(EventScope::All);
    assert!(!v.scroll_layer_at(1, 3));
    assert!(v.tool_groups().is_empty());
    v.clear();
    assert!(v.tool_positions.is_empty());
    assert!(v.tool_hit_rows.is_empty());
}

fn frame(state: &mut crate::client::terminal::State) -> (String, Vec<u8>) {
    let frame = state.chat_frame(Instant::now());
    let ansi = frame.diff(None);
    let mut parser = vt100::Parser::new(
        crate::client::terminal::nonzero(frame.geometry.rows),
        crate::client::terminal::nonzero(frame.geometry.cols),
        0,
    );
    parser.process(&ansi);
    (parser.screen().contents(), ansi)
}

fn key(state: &mut crate::client::terminal::State, bytes: &[u8]) {
    assert!(state.chat_keyboard(bytes, Instant::now(), false).is_empty());
}

#[tokio::test]
async fn tool_containers_real_key_parser_routes_wheel_and_pages_without_touching_draft() {
    let fixture = Fixture::new().await;
    let scope = fixture.scope(1, "123", 'a', None);
    for width in [120, 49, 24] {
        let mut state = crate::client::terminal::preview_state(28, width, &Ui::default());
        state.events.clear();
        state.notice = None;
        for index in 0..12 {
            state.log_message("user", &format!("Earlier message {index}"), b"");
        }
        for index in 0..16 {
            tool(&mut state.events, Some(&scope), index, 50);
        }
        state.conversation.composer.set_text("Keep draft 界🙂");
        let draft = state.conversation.snapshot()["draft"].clone();
        let (screen, _) = frame(&mut state);
        let y = screen
            .lines()
            .position(|row| row.contains("command-15"))
            .unwrap();
        let split = state.geometry.left / 2;
        key(&mut state, format!("\x1b[<0;2;{}M", y + 1).as_bytes());
        let (screen, _) = frame(&mut state);
        assert_eq!(
            state.events.snapshot()["events"]
                .as_array()
                .unwrap()
                .last()
                .unwrap()["expanded"],
            true
        );
        let output_y = screen
            .lines()
            .position(|row| row.contains("Command"))
            .unwrap();
        let before = state.events.snapshot();
        key(
            &mut state,
            format!("\x1b[<65;2;{}M", output_y + 1).as_bytes(),
        );
        frame(&mut state);
        let inner = state.events.snapshot();
        assert_eq!(inner["scroll"], before["scroll"]);
        assert_eq!(
            inner["tool_groups"][0]["scroll"],
            before["tool_groups"][0]["scroll"]
        );
        assert!(
            inner["events"].as_array().unwrap().last().unwrap()["panel"]["scroll"]
                .as_u64()
                .unwrap()
                > 0
        );
        key(
            &mut state,
            format!("\x1b[<64;{};{}M", split + 1, output_y + 1).as_bytes(),
        );
        frame(&mut state);
        let outer = state.events.snapshot();
        assert!(outer["scroll"].as_u64().unwrap() < inner["scroll"].as_u64().unwrap());
        assert_eq!(outer["tool_groups"], inner["tool_groups"]);
        key(&mut state, b"\x1b[6~");
        frame(&mut state);
        assert_eq!(state.conversation.snapshot()["draft"], draft);
        key(&mut state, b"\x1b[18~");
        assert!(state.selection);
        let selected = state.events.snapshot();
        key(
            &mut state,
            format!("\x1b[<64;2;{}M", output_y + 1).as_bytes(),
        );
        assert_eq!(
            state.events.snapshot(),
            selected,
            "native copy mode owns mouse input"
        );
        key(&mut state, b"\x1b");
        state.chat_keyboard(
            b"",
            Instant::now() + std::time::Duration::from_millis(100),
            true,
        );
        assert!(!state.selection);
        assert_eq!(state.conversation.snapshot()["draft"], draft);
        if let Some(path) = std::env::var_os("SSMCP_TOOL_CONTAINERS_PREVIEW_DIR") {
            let path = PathBuf::from(path);
            std::fs::create_dir_all(&path).unwrap();
            let (plain, ansi) = frame(&mut state);
            std::fs::write(path.join(format!("container-{width}.ansi")), ansi).unwrap();
            std::fs::write(path.join(format!("container-{width}.txt")), plain).unwrap();
        }
    }
}

#[tokio::test]
async fn tool_containers_language_switch_rebuilds_colorless_rows() {
    use crate::client::language::Language;
    let fixture = Fixture::new().await;
    let scope = fixture.scope(1, "123", 'a', None);
    let mut v = view(&scope, 3);
    v.selected = Some(v.events[0].id);
    v.open();
    let mut config = ui();
    for width in [1, 2, 3, 9, 31, 68, 100] {
        for language in [Language::English, Language::Chinese, Language::English] {
            config.language = language;
            let rows = v.render(width, 20, &config, true);
            assert!(rows.iter().all(|row| !row.contains('\x1b')));
            assert!(rows.iter().all(
                |row| unicode_width::UnicodeWidthStr::width(row.as_str()) <= usize::from(width)
            ));
            let cache = v.events[0].cache.as_ref().unwrap();
            assert!(cache.1.contains(language.code()));
            if width >= 31 {
                let body = cache.2.join("\n");
                assert!(body.starts_with(language.text("Command", "命令")), "{body}");
                let frame = rows.join("\n");
                assert!(frame.contains(language.text("Tools", "工具")), "{frame}");
                assert!(frame.contains(language.text(" rows", " 行")), "{frame}");
            }
        }
    }
}
