use super::*;
use crate::client::{terminal, ui::Ui};

fn osc(url: &str, label: &str) -> String {
    format!("\x1b]8;;{url}\x1b\\{label}{CLOSE}")
}

#[test]
fn links_only_accept_complete_http_targets_with_bounded_storage() {
    for raw in [
        "file:///etc/passwd",
        "javascript:alert(1)",
        "https://user@host/path",
        "https://a/\0b",
        "https://a/a b",
        "https://a/\\b",
        "https://",
        "ssmcp-link:0",
    ] {
        assert!(target(raw).is_none(), "{raw:?}");
    }
    assert_eq!(
        &*target("HTTPS://Example.COM/a").unwrap(),
        "https://example.com/a"
    );
    assert!(
        target(&format!(
            "https://example.com/{}",
            "a".repeat(MAX_TARGET_BYTES)
        ))
        .is_none()
    );
    let mut table = Targets::default();
    for n in 0..1000 {
        table.insert(&format!("https://example.com/{n}"));
    }
    assert_eq!(table.values.len(), MAX_TARGETS);
    assert_eq!(table.insert("https://example.com/0"), Some(0));
    let mut table = Targets::default();
    for n in 0..1000 {
        table.insert(&format!("https://example.com/{n}/{}", "a".repeat(1900)));
    }
    assert!(table.bytes <= MAX_TABLE_BYTES);
    assert!(table.values.len() < MAX_TARGETS);
}

#[test]
fn links_bare_urls_trim_sentence_punctuation_and_preserve_balanced_paths() {
    let raw = "See (https://en.example/wiki/Function_(math)). 界 https://例子.测试/路径。 https://[::1]/x], https://e.test/?a=1&b=2! xhttps://wrong.test https://a.test/end.";
    let found: Vec<_> = bare(raw).into_iter().map(|range| &raw[range]).collect();
    assert_eq!(
        found,
        [
            "https://en.example/wiki/Function_(math)",
            "https://例子.测试/路径",
            "https://[::1]/x",
            "https://e.test/?a=1&b=2",
            "https://a.test/end"
        ]
    );
    let excessive = format!("https://example.com/{} ", "a".repeat(MAX_TARGET_BYTES + 20));
    assert!(
        bare(&excessive).is_empty(),
        "never accept a cut prefix of an overlong URL"
    );
}

#[test]
fn links_controls_cannot_inject_local_ids_or_make_control_payloads_clickable() {
    let raw = format!(
        "\x1b]52;c;https://secret.test\x07\x1bPhttps://dcs.test\x1b\\\u{9d}https://c1.test\u{9c}{}text{CLOSE}",
        marker(0)
    );
    let source = Source::new(&raw, true);
    assert_eq!(source.text, "text");
    assert!(source.targets.values.is_empty());
    assert!(source.spans.is_empty());
    let unclosed = Source::new("before\x1b]8;;https://unfinished.test", true);
    assert_eq!(unclosed.text, "before");
    assert!(unclosed.spans.is_empty());
    let replacement =
        format!("\x1b]8;;https://a.test\x1b\\one\x1b]8;;file:///tmp/no\x1b\\two{CLOSE}");
    let source = Source::new(&replacement, false);
    assert_eq!(source.text, "onetwo");
    assert_eq!(source.spans.len(), 1);
    assert_eq!(source.spans[0].range, 0..3);
}

#[test]
fn links_explicit_targets_survive_highlighting_and_override_bare_labels() {
    let raw = osc("https://real.test/path", "https://label.test");
    let colored = through(&raw, |text| format!("\x1b[38;2;4;0;24m{text}\x1b[0m"));
    let source = Source::new(&colored, true);
    assert_eq!(source.targets.values.len(), 1);
    assert_eq!(&*source.targets.values[0], "https://real.test/path");
    let rows = wrapped(&colored, 100, true);
    let (line, hits) = rows.resolve(&rows[0], 100);
    assert_eq!(hits.len(), 1);
    assert_eq!(&*hits[0].target, "https://real.test/path");
    assert!(!line.contains("\x1b]"));
    let mut parser = vt100::Parser::new(terminal::nonzero(1), terminal::nonzero(100), 0);
    parser.process(line.as_bytes());
    assert!(parser.screen().cell(0, 0).unwrap().underline());
}

#[test]
fn links_soft_wrap_uses_one_target_and_keeps_unicode_cell_coordinates() {
    let raw = format!(
        "界e\u{301} {} end",
        osc("https://example.com/路径?a=1", "标签界e\u{301}🙂")
    );
    for width in [1, 2, 3, 5, 9, 80] {
        let rows = wrapped(&raw, width, false);
        assert_eq!(rows.targets.values.len(), 1);
        let mut count = 0;
        for row in rows.iter() {
            let (line, hits) = rows.resolve(row, width);
            assert!(!line.contains("\x1b]"));
            assert!(
                unicode_width::UnicodeWidthStr::width(sanitize(&line, false, false).as_str())
                    <= width
            );
            for hit in hits {
                assert!(hit.start < hit.end && usize::from(hit.end) <= width);
                assert_eq!(hit.target, target("https://example.com/路径?a=1").unwrap());
                count += 1;
            }
        }
        assert!(count > 0, "{width}");
    }
    let long = format!("https://example.com/{}", "a".repeat(1500));
    let rows = wrapped(&long, 1, false);
    assert_eq!(rows.targets.values.len(), 1);
    assert_eq!(rows.targets.bytes, long.len());
    assert!(rows.iter().map(String::len).sum::<usize>() < long.len() * 80);
    assert!(rows.iter().all(|row| !row.contains("example.com")));
}

#[test]
fn links_markdown_labels_osc8_code_and_balanced_targets_have_full_destinations() {
    let ui = Ui {
        color: false,
        ..Ui::default()
    };
    let raw = format!(
        "[说明界](https://example.com/a_(b))\n{}\n`https://code.test/a`\nhttps://path.test/_literal_/x",
        osc("https://other.test/z", "OSC标签\nsecond")
    );
    for width in [1, 2, 3, 7, 16, 80] {
        let rows = terminal::markdown::linked_rows(&raw, width, &ui);
        let mut seen = std::collections::BTreeSet::new();
        for row in rows.iter() {
            let (line, hits) = rows.resolve(row, usize::from(width));
            assert!(!line.contains("\x1b]"));
            for hit in hits {
                assert!(hit.end <= width);
                seen.insert(hit.target.to_string());
            }
        }
        assert_eq!(
            seen,
            [
                "https://example.com/a_(b)",
                "https://other.test/z",
                "https://code.test/a",
                "https://path.test/_literal_/x"
            ]
            .into_iter()
            .map(String::from)
            .collect()
        );
    }
    assert!(
        terminal::markdown::rows(&raw, 80, &ui)
            .iter()
            .all(|row| !row.contains('\x1b'))
    );
}

fn shown(target: &str) -> ScreenHit {
    ScreenHit {
        row: 4,
        event: 7,
        hit: Hit {
            start: 3,
            end: 12,
            target: Arc::from(target),
            occurrence: 0,
        },
    }
}

#[test]
fn links_only_open_after_a_completed_single_click_on_the_presented_target() {
    let now = Instant::now();
    let mut display = Display::default();
    let mut same_link = shown("https://one.test/");
    same_link.hit.occurrence = 12;
    display.stage(vec![same_link]);
    assert!(
        !display.mouse(0, 4, 4, false, now),
        "pending frame is not interactive"
    );
    display.presented();
    assert!(display.mouse(0, 4, 4, false, now));
    assert!(display.mouse(0, 4, 4, true, now));
    assert!(display.take(now).is_none());
    display.stage(vec![shown("https://one.test/")]);
    assert_eq!(
        display.take(now + DOUBLE_CLICK).as_deref(),
        Some("https://one.test/")
    );
    assert!(display.take(now + DOUBLE_CLICK).is_none());
}

#[test]
fn links_drag_double_click_wheel_replaced_target_and_resize_cancel_opening() {
    let now = Instant::now();
    for action in 0..6 {
        let mut display = Display::default();
        display.stage(vec![shown("https://one.test/")]);
        display.presented();
        display.mouse(0, 4, 4, false, now);
        match action {
            0 => {
                display.mouse(32, 5, 4, false, now);
                display.mouse(32, 4, 4, false, now);
            }
            1 => {
                display.mouse(0, 4, 4, true, now);
                display.mouse(0, 4, 4, false, now + Duration::from_millis(10));
            }
            2 => {
                display.mouse(64, 4, 4, false, now);
            }
            3 => {
                display.stage(vec![shown("https://two.test/")]);
                display.presented();
            }
            4 => display.invalidate(),
            5 => {
                display.mouse(0, 5, 4, true, now);
            }
            _ => unreachable!(),
        }
        display.mouse(0, 4, 4, true, now + Duration::from_millis(20));
        assert!(
            display.take(now + Duration::from_secs(1)).is_none(),
            "case {action}"
        );
    }
}

#[test]
fn links_double_click_on_adjacent_cells_still_cancels_the_same_link() {
    let now = Instant::now();
    let mut display = Display::default();
    display.stage(vec![shown("https://one.test/")]);
    display.presented();
    display.mouse(0, 4, 4, false, now);
    display.mouse(0, 4, 4, true, now);
    display.mouse(0, 5, 4, false, now + Duration::from_millis(10));
    display.mouse(0, 5, 4, true, now + Duration::from_millis(20));
    assert!(display.take(now + Duration::from_secs(1)).is_none());
}

#[test]
fn links_grapheme_prefix_positions_and_wrap_boundaries() {
    let mut actual = Vec::new();
    let mut expected = Vec::new();
    for prefix in ["👨‍👩‍👧‍👦", "❤️", "🇨🇳", "🧑🏽‍💻"] {
        let raw = format!("{prefix} {}", osc("https://emoji.test/", "target"));
        let rows = wrapped(&raw, 64, false);
        let (_, hits) = rows.resolve(&rows[0], 64);
        actual.push((prefix, hits[0].start));
        expected.push((prefix, prefix.width() as u16 + 1));
        println!(
            "grapheme={prefix:?} actual_start={} expected_start={}",
            hits[0].start,
            prefix.width() + 1
        );
        let rows = wrapped(&format!("{prefix}x"), 2, false);
        assert_eq!(sanitize(&rows[0], false, false), prefix);
        assert_eq!(sanitize(&rows[1], false, false), "x");
    }
    assert_eq!(
        actual, expected,
        "click cells must follow complete grapheme widths"
    );
}

#[test]
fn links_grapheme_row_normalization_preserves_styles_and_discards_other_controls() {
    let ui = Ui::default();
    let mut rows = vec![
        "\x1b[31;4m👨‍👩‍👧‍👦x\x1b[0m y".into(),
        "\x1b]52;c;https://hidden.test\x07\x1b[2J❤️Z".into(),
        "🇨🇳\x1b[38;2;4;0;24mABCD".into(),
    ];
    fill_rows(&mut rows, 6, &ui);
    assert_eq!(sanitize(&rows[0], false, false), "👨‍👩‍👧‍👦x y ");
    assert_eq!(sanitize(&rows[1], false, false), "❤️Z   ");
    assert_eq!(sanitize(&rows[2], false, false), "🇨🇳ABCD");
    assert!(rows[0].contains("[0;4"));
    for row in rows {
        let mut text = row.as_str();
        while !text.is_empty() {
            let (len, item) = token(text);
            assert!(matches!(item, Token::Text(_) | Token::Sgr(_)), "{row:?}");
            text = &text[len..];
        }
    }
    for width in [1, 2, 3, 4, 7] {
        let rows = terminal::markdown::linked_rows("👨‍👩‍👧‍👦 [x](https://emoji.test/)", width, &ui);
        for row in rows.iter() {
            let (line, _) = rows.resolve(row, usize::from(width));
            assert!(sanitize(&line, false, false).width() <= usize::from(width));
            assert!(
                !line.contains('👨') || line.contains("👨‍👩‍👧‍👦"),
                "cluster split: {line:?}"
            );
        }
    }
}

#[test]
fn links_markdown_keeps_literal_controls_but_discards_hidden_string_payloads() {
    let ui = Ui::default();
    let input = format!(
        "```text\n\x1b[2J\u{202e}literal\n```\n\x1b]52;c;https://hidden.test/\x07{}",
        osc("https://visible.test/", "visible")
    );
    let rows = terminal::markdown::linked_rows(&input, 80, &ui);
    let mut plain = String::new();
    let mut targets = Vec::new();
    for row in rows.iter() {
        let (row, hits) = rows.resolve(row, 80);
        plain.push_str(&sanitize(&row, false, false));
        targets.extend(hits.into_iter().map(|hit| hit.target));
    }
    assert!(plain.contains("\\x1b[2J\\u{202e}literal"));
    assert!(!plain.contains("hidden.test"));
    assert!(!targets.is_empty());
    assert!(
        targets
            .iter()
            .all(|target| target.as_ref() == "https://visible.test/")
    );
}

#[test]
fn links_markdown_redacts_complete_visible_fields_across_osc_boundaries() {
    let ui = Ui::default();
    for between in [
        "\x1b]8;;https://example.test/\x1b\\",
        "\x1b]0;ignored-title\x07",
    ] {
        let source =
            format!("Authorization: Bearer {between}opaque_value_123{CLOSE}\nordinary text");
        let rows = terminal::markdown::linked_rows(&source, 100, &ui);
        let mut text = String::new();
        for row in rows.iter() {
            let (row, hits) = rows.resolve(row, 100);
            text.push_str(&sanitize(&row, false, false));
            assert!(
                hits.is_empty(),
                "redacted source cannot reuse original metadata"
            );
        }
        assert!(!text.contains("opaque_value_123"));
        assert!(text.contains("redacted") && text.contains("ordinary text"));
    }
}

fn frame(state: &mut terminal::State) -> Vec<u8> {
    let bytes = state.chat_frame(Instant::now()).diff(None);
    state.links.presented();
    bytes
}

#[test]
fn links_real_key_parser_preserves_draft_and_anchored_background_append() {
    let mut state = terminal::State::new(24, 70, &Ui::default());
    state.preview = true;
    state.log_message("agent", "[label界](https://example.test/path)", b"");
    for n in 0..25 {
        state.log_message("user", &format!("filler {n}"), b"");
    }
    frame(&mut state); // Measure rows before scrolling, as the live UI does.
    state.events.scroll_by(-1000);
    state.conversation.composer.set_text("draft stays");
    let bytes = frame(&mut state);
    assert!(String::from_utf8_lossy(&bytes).contains("\x1b[?1002h"));
    let location = state.links.shown.first().unwrap().clone();
    let mouse = |release| {
        format!(
            "\x1b[<0;{};{}{}",
            location.hit.start + 1,
            location.row + 1,
            if release { 'm' } else { 'M' }
        )
    };
    assert!(
        state
            .chat_keyboard(mouse(false).as_bytes(), Instant::now(), false)
            .is_empty()
    );
    state.log_message("user", "new background message", b"");
    frame(&mut state);
    assert!(
        state
            .chat_keyboard(mouse(true).as_bytes(), Instant::now(), false)
            .is_empty()
    );
    assert_eq!(
        state.links.take(Instant::now() + DOUBLE_CLICK).as_deref(),
        Some("https://example.test/path")
    );
    assert_eq!(state.conversation.composer.text(), "draft stays");
    state.chat_keyboard(mouse(false).as_bytes(), Instant::now(), false);
    state.chat_keyboard(mouse(true).as_bytes(), Instant::now(), false);
    state.chat_keyboard(b"x", Instant::now(), false);
    assert!(
        state.links.take(Instant::now() + DOUBLE_CLICK).is_none(),
        "typing cancels a pending opener"
    );
    state.chat_keyboard(mouse(false).as_bytes(), Instant::now(), false);
    state.chat_keyboard(b"\x1b[18~", Instant::now(), false);
    frame(&mut state);
    state.chat_keyboard(mouse(true).as_bytes(), Instant::now(), false);
    assert!(state.links.take(Instant::now() + DOUBLE_CLICK).is_none());
    assert!(state.selection);
}
