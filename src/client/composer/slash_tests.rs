use super::*;

fn draft(text: &str) -> Composer {
    let mut composer = Composer::default();
    assert!(composer.set_text(text));
    composer
}

fn select(composer: &mut Composer, name: &str) {
    composer.suggestion_index = composer
        .suggestions()
        .iter()
        .position(|item| item.name == name)
        .unwrap_or_else(|| panic!("missing suggestion {name} for {:?}", composer.text()));
}

#[test]
fn secondary_tab_preserves_arguments_separators_and_utf8() {
    let mut composer = draft("  /plugin\tlo\t名称 \"path with spaces/界\"");
    composer.cursor = "  /plugin\tlo".len();
    select(&mut composer, "/plugin load");
    assert!(composer.accept_suggestion().unwrap());
    assert_eq!(
        composer.text(),
        "  /plugin\tload\t名称 \"path with spaces/界\""
    );
    assert_eq!(composer.cursor(), "  /plugin\tload\t".len());
    assert!(composer.text().is_char_boundary(composer.cursor()));

    let mut composer = draft("/group remold  exact-agent-id");
    composer.cursor = "/group rem".len();
    select(&mut composer, "/group remove");
    composer.accept_suggestion().unwrap();
    assert_eq!(composer.text(), "/group remove  exact-agent-id");
    assert_eq!(composer.cursor(), "/group remove  ".len());
}

#[test]
fn primary_tab_preserves_existing_arguments_and_enter_shortcut_scope() {
    let mut composer = draft("  /plu load demo /tmp/plugin.so");
    composer.cursor = "  /plu".len();
    select(&mut composer, "/plugin");
    composer.accept_suggestion().unwrap();
    assert_eq!(composer.text(), "  /plugin load demo /tmp/plugin.so");
    assert_eq!(composer.cursor(), "  /plugin ".len());
    assert!(!composer.primary_command_suggestion());
    assert!(draft(" /aud").primary_command_suggestion());
    assert!(!draft("/new ").primary_command_suggestion());
    assert!(!draft("/new a").primary_command_suggestion());
}

#[test]
fn free_form_arguments_and_noncommands_do_not_offer_secondary_verbs() {
    for text in [
        "please /group",
        "/unknown ",
        "/group add exact",
        "/plugin load demo",
        "/goal set some text",
        "/filter cargo test",
        "/version extra",
        "/group\nsend",
        "/group\r",
    ] {
        assert!(draft(text).suggestions().is_empty(), "{text:?}");
    }
    let text = "/group   add";
    assert!(slash::query(text, "/group ".len()).is_none());
    assert!(slash::query("/group 界", "/group ".len() + 1).is_none());
    assert!(slash::query("/group ", 100).is_none());
    let (completed, cursor) = slash::apply("/group   ", 7, "/group pick").unwrap();
    assert_eq!(completed, "/group pick  ");
    assert_eq!(cursor, completed.len());
}

#[test]
fn subcommand_hints_cover_literal_parser_branches_without_invented_values() {
    for (prefix, name) in [
        ("/tasks ca", "/tasks cancel"),
        ("/goal se", "/goal set"),
        ("/chatgpt co", "/chatgpt copy"),
        ("/plugin --", "/plugin --help"),
        ("/wait ca", "/wait cancel"),
        ("/diff sp", "/diff split"),
        ("/allow re", "/allow remove"),
        ("/login li", "/login link"),
        ("/new con", "/new conversation"),
        ("/filter --e", "/filter --errors"),
        ("/theme cod", "/theme codex-dark"),
        ("/group ret", "/group retry"),
        ("/paste-retry di", "/paste-retry discard"),
        ("/invite wo", "/invite worker"),
        ("/pi to", "/pi tools"),
        ("/cc to", "/cc tools"),
        ("/codex to", "/codex tools"),
    ] {
        let mut composer = draft(prefix);
        let item = composer.selected_suggestion().unwrap();
        assert_eq!(item.name, name);
        assert!(!item.description.is_empty());
        assert!(!item.reference);
        composer.accept_suggestion().unwrap();
        assert_eq!(composer.text(), format!("{name} "));
    }
    for text in [
        "/allow list",
        "/worker list",
        "/invite coordinator",
        "/new logs",
    ] {
        assert!(draft(text).suggestions().is_empty(), "{text}");
    }
    for item in draft("/theme ").suggestions() {
        crate::client::ui::Theme::parse(item.name.split_once(' ').unwrap().1).unwrap();
    }
}

#[test]
fn worker_count_completion_preserves_the_real_parser_syntax() {
    use crate::client::worker_control::{Command, parse};
    for (sign, amount) in [("+", 3), ("-", -3)] {
        let mut composer = draft("/worker ");
        select(&mut composer, &format!("/worker {sign}"));
        composer.accept_suggestion().unwrap();
        assert_eq!(composer.text(), format!("/worker {sign}"));
        assert!(composer.insert("3"));
        assert_eq!(
            parse(composer.text().strip_prefix("/worker ").unwrap()),
            Ok(Command::Delta(amount))
        );
    }
    let mut composer = draft("/worker +128");
    composer.cursor = "/worker +".len();
    select(&mut composer, "/worker +");
    composer.accept_suggestion().unwrap();
    assert_eq!(composer.text(), "/worker +128");
    assert_eq!(composer.cursor(), "/worker +".len());
    assert_eq!(parse("+128"), Ok(Command::Delta(128)));
    let mut composer = draft("/worker l");
    composer.accept_suggestion().unwrap();
    assert_eq!(
        parse(composer.text().strip_prefix("/worker ").unwrap()),
        Ok(Command::List)
    );
}

#[test]
fn a_completion_that_exceeds_the_draft_limit_is_atomic() {
    let text = format!(
        "/group r {}",
        "x".repeat(MAX_DRAFT_BYTES - "/group r ".len())
    );
    let mut composer = draft(&text);
    composer.cursor = "/group r".len();
    composer.remember("saved request");
    select(&mut composer, "/group remove");
    let before = (
        composer.text().to_owned(),
        composer.cursor(),
        composer.revision,
        composer.history.clone(),
    );
    assert!(composer.accept_suggestion().is_err());
    assert_eq!(
        (
            composer.text().to_owned(),
            composer.cursor(),
            composer.revision,
            composer.history.clone()
        ),
        before
    );
}

#[test]
fn registered_tool_hints_cache_by_input_and_catalog_generation() {
    let mut composer = draft("/cc L");
    composer.refresh_harness_completions_with(true, |provider, prefix, known| {
        assert_eq!((provider, prefix, known), ("cc", "L", None));
        Ok(Some((
            7,
            vec![(
                "claude.LSP".into(),
                "Inspect the installed LSP schema".into(),
            )],
        )))
    });
    assert_eq!(
        composer.selected_suggestion().unwrap().name,
        "/cc claude.LSP"
    );
    composer.refresh_harness_completions_with(true, |_, _, known| {
        assert_eq!(known, Some(7));
        Ok(None)
    });
    assert_eq!(
        composer.selected_suggestion().unwrap().name,
        "/cc claude.LSP"
    );
    composer.accept_suggestion().unwrap();
    assert_eq!(composer.text(), "/cc claude.LSP ");
    assert!(composer.suggestions().is_empty());

    composer.set_text("/pi ");
    assert!(
        composer
            .suggestions()
            .iter()
            .all(|item| !item.name.contains("LSP"))
    );
    composer.refresh_harness_completions_with(true, |provider, prefix, known| {
        assert_eq!((provider, prefix, known), ("pi", "", None));
        Ok(Some((8, vec![("pi.rpc".into(), "Inspect RPC".into())])))
    });
    select(&mut composer, "/pi pi.rpc");
    composer.refresh_harness_completions_with(true, |_, _, _| {
        Ok(Some((
            9,
            vec![
                ("pi.events".into(), "Inspect events".into()),
                ("pi.rpc".into(), "Inspect RPC".into()),
            ],
        )))
    });
    assert_eq!(composer.selected_suggestion().unwrap().name, "/pi pi.rpc");
    composer.refresh_harness_completions_with(true, |_, _, _| Err("provider disconnected".into()));
    assert_eq!(composer.suggestions().len(), 1);
    assert_eq!(composer.suggestions()[0].name, "/pi tools");
}

#[test]
fn disabled_and_dismissed_menus_do_not_query_native_catalogs() {
    let mut composer = draft("/codex ");
    composer.refresh_harness_completions_with(false, |_, _, _| {
        panic!("preview completion must not touch the live catalog")
    });
    composer.suggestions_dismissed = true;
    composer.refresh_harness_completions_with(true, |_, _, _| {
        panic!("dismissed completion must not touch the live catalog")
    });
    assert!(composer.suggestions().is_empty());
    composer.set_text("/group ");
    composer.refresh_harness_completions_with(true, |_, _, _| {
        panic!("local command completion must not touch the live catalog")
    });
}

#[test]
fn narrow_secondary_menus_stay_inside_the_editor_and_keep_a_valid_cursor() {
    let composer = draft("/group re");
    let ui = Ui {
        color: false,
        ..Ui::default()
    };
    for width in [1, 2, 8, 24, 60, 132] {
        for height in [1, 2, 4, 6] {
            let rows = composer.rows(width, height, &ui, true);
            assert!(rows.lines.len() <= usize::from(height));
            assert!(
                rows.lines
                    .iter()
                    .all(|line| line.width() <= usize::from(width))
            );
            assert!(rows.cursor_row < height);
            assert!(rows.cursor_col < width);
        }
    }
    let rows = composer.rows(60, 4, &ui, true);
    assert!(rows.lines.iter().any(|line| line.contains("/group remove")));
    assert!(rows.lines.iter().any(|line| line.contains("/group retry")));
}
