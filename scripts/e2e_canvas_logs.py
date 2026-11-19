#!/usr/bin/env python3
"""Exercise logical columns and diagnostic logs through a disposable real PTY.

Build ssmcp and examples/vt_snapshot, then pass --bin-dir and --out.
The preview fixture runs without a relay, external tools, or models. Its PTY
belongs to this test and never receives input through the user's desktop.
"""

import argparse
import json
from pathlib import Path
import tempfile

from e2e_client_ui import Preview, column, left_rows, logs_open, prompt_start, prompt_text, save_shot, text, view


LEFT = b"\x1b[D"
RIGHT = b"\x1b[C"
ALT_LEFT = b"\x1b[1;3D"
ALT_RIGHT = b"\x1b[1;3C"
ESCAPE = b"\x1b"
DIAGNOSTIC = "A quieter workspace"


def body(shot):
    end = prompt_start(shot)
    return "\n".join(shot["rows"][1:end if end is not None else -1])


def mouse(button, *, release=False, x=8, y=4):
    return f"\x1b[<{button};{x};{y}{'m' if release else 'M'}".encode()


def snapshot(preview):
    preview.pump(0.15)
    return preview.decoder.request(op="snapshot", cells=True)


def intact_cells(shot):
    rows, cols = shot["size"]
    if len(shot["cells"]) != rows:
        return False
    for cells in shot["cells"]:
        if len(cells) != cols:
            return False
        for index, cell in enumerate(cells):
            if cell["wide"] and (index + 1 == cols or not cells[index + 1]["continuation"]):
                return False
            if cell["continuation"] and (index == 0 or not cells[index - 1]["wide"]):
                return False
    row, col = shot["cursor"]
    return 0 <= row < rows and 0 <= col < cols


def run(binary, decoder, output, checks):
    def check(value, label):
        if not value:
            raise AssertionError(label)
        checks.append(label)

    with tempfile.TemporaryDirectory(prefix="ssmcp-canvas-logs-") as temporary:
        config = Path(temporary) / "config.toml"
        config.write_text('[ui]\nlayout="chat"\nsidebar=false\ncolor=false\n', encoding="utf-8")
        preview = Preview(binary, decoder, config, rows=30, cols=110)
        try:
            initial = preview.until(
                lambda shot: view(shot, "conversation") and "cargo test --lib" in body(shot),
                "conversation and compact tool activity",
            )
            check("Improve the client experience" in body(initial)
                  and "src/client/terminal/events.rs" in body(initial)
                  and "done" in body(initial)
                  and "Command completed successfully" in body(initial)
                  and "4–6/6" in body(initial)
                  and "expanded = false" in body(initial)
                  and "@@ -1 +1 @@" not in body(initial),
                  "conversation retains user messages and result summaries while tool and diff details start collapsed")
            check(DIAGNOSTIC not in text(initial),
                  "background diagnostics do not appear in the conversation")
            initial_count = column(initial)[2]
            check(initial_count == 8, "the main workflow starts with all eight logical columns")
            save_shot(output, "canvas-conversation", initial)

            draft = "canvas draft 界"
            preview.send(draft.encode())
            typed = preview.until(lambda shot: draft in prompt_text(shot), "Unicode shared draft")
            cursor = typed["cursor"]
            preview.send(LEFT)
            moved = preview.until(lambda shot: shot["cursor"] != cursor,
                                  "cursor moving left inside the draft")
            check(view(moved, "conversation") and draft in prompt_text(moved)
                  and moved["cursor"] == [cursor[0], cursor[1] - 2],
                  "ordinary Left inside the draft moves across a wide glyph without changing columns")
            preview.send(RIGHT)
            preview.until(lambda shot: view(shot, "conversation") and shot["cursor"] == cursor,
                          "cursor returning to the draft end")
            check(True, "ordinary Right inside the draft remains an editing action")

            preview.send(mouse(2))
            opened = preview.until(logs_open, "diagnostic log panel after right-button press")
            check(DIAGNOSTIC in body(opened), "diagnostic messages appear in the dedicated log panel")
            check(draft in prompt_text(opened) and opened["hide_cursor"],
                  "opening diagnostic logs preserves the shared draft and gives logs keyboard focus")
            save_shot(output, "canvas-diagnostic-logs", opened)
            preview.send(mouse(2, release=True))
            released = snapshot(preview)
            check(logs_open(released), "right-button release does not toggle the log panel again")
            preview.send(mouse(2))
            closed = preview.until(lambda shot: view(shot, "conversation") and not logs_open(shot),
                                   "second right-button press returning to the conversation")
            check(draft in prompt_text(closed) and closed["cursor"] == cursor,
                  "right-button toggling restores the conversation, draft and cursor")
            preview.send(mouse(2, release=True))
            check(not logs_open(snapshot(preview)), "release after closing leaves the original column open")

            preview.send(mouse(2))
            preview.until(logs_open, "logs reopened before editing")
            preview.send(mouse(2, release=True) + b"x")
            draft += "x"
            # Logs is a canvas column with the shared composer, not an overlay.
            # Editing stays here; only an explicit return restores its bookmark.
            editing = preview.until(
                lambda shot: logs_open(shot) and draft in prompt_text(shot)
                and not shot["hide_cursor"],
                "typing in the shared draft while remaining in the Logs column",
            )
            cursor = editing["cursor"]
            check(column(editing) == ("logs", 7, 8),
                  "typing in Logs keeps its column active and focuses the shared draft")
            save_shot(output, "canvas-logs-editor", editing)
            preview.send(ESCAPE)
            returned_from_logs = preview.until(
                lambda shot: view(shot, "conversation") and not logs_open(shot)
                and draft in prompt_text(shot) and not shot["hide_cursor"],
                "Escape returning from Logs to the original conversation",
            )
            check(returned_from_logs["cursor"] == cursor,
                  "Escape restores the bookmarked column with the edited draft and cursor")

            preview.send(RIGHT)
            audit = preview.until(lambda shot: view(shot, "code audit"),
                                  "Right at draft end navigating to code audit")
            check(draft in prompt_text(audit), "Right at the draft boundary changes columns without changing text")
            preview.send(ALT_LEFT)
            returned = preview.until(lambda shot: view(shot, "conversation"), "Alt+Left returning to conversation")
            check(draft in prompt_text(returned) and returned["cursor"] == cursor,
                  "horizontal round trip restores the original composer cursor")

            preview.send(LEFT + ALT_LEFT)
            agents = preview.until(lambda shot: view(shot, "agents") and "Builder" in body(shot),
                                   "Alt+Left reaching agents from inside a nonempty draft")
            check(draft in prompt_text(agents), "Alt+Left changes columns while preserving a nonempty draft")
            preview.send(b"\x1b[B")
            selected = preview.until(
                lambda shot: any("›" in row and "Builder" in row for row in shot["rows"]),
                "the Builder member becoming selected",
            )
            selected_body = body(selected)
            preview.send(ALT_LEFT)
            bounded_left = snapshot(preview)
            check(view(bounded_left, "agents") and column(bounded_left)[1] == 1
                  and body(bounded_left) == selected_body,
                  "horizontal navigation stops at the first column")
            preview.send(ALT_RIGHT)
            preview.until(lambda shot: view(shot, "conversation"), "leaving the selected agents column")
            preview.send(ALT_LEFT)
            retained = preview.until(
                lambda shot: view(shot, "agents")
                and any("›" in row and "Builder" in row for row in shot["rows"]),
                "the selected agent surviving a column round trip",
            )
            check(body(retained) == selected_body, "agent selection is retained independently in its logical column")

            preview.send(mouse(2))
            preview.until(logs_open, "diagnostic logs opened from the agents column")
            preview.send(mouse(2, release=True) + ESCAPE)
            restored = preview.until(
                lambda shot: view(shot, "agents") and not logs_open(shot),
                "Escape restoring the agents column",
            )
            check(body(restored) == selected_body and draft in prompt_text(restored),
                  "Escape closes logs and preserves the original column selection and shared draft")
            preview.send(b"\r")
            recipient = preview.until(lambda shot: view(shot, "conversation") and "To · Builder" in text(shot),
                                      "the selected member becoming the draft recipient")
            check(draft in prompt_text(recipient), "choosing a recipient preserves the shared draft")
            preview.send(ALT_RIGHT)
            preview.until(lambda shot: view(shot, "code audit"), "code audit with the chosen recipient")
            preview.resize(12, 60)
            short_audit = preview.until(
                lambda shot: shot["size"] == [12, 60] and view(shot, "code audit"),
                "short code audit viewport",
            )
            preview.send(b"\r")
            short_audit = preview.until(
                lambda shot: view(shot, "code audit") and "const client" in body(shot)
                and "Enter inspect" not in body(shot),
                "the selected audit source opened for inspection",
            )
            preview.send(b"\x1b[6~")
            scrolled = preview.until(lambda shot: body(shot) != body(short_audit),
                                     "code audit scrolling within its own column")
            scrolled_body = body(scrolled)
            preview.send(ALT_LEFT)
            preview.until(lambda shot: view(shot, "conversation"), "leaving the scrolled audit column")
            preview.send(ALT_RIGHT)
            back = preview.until(lambda shot: view(shot, "code audit"), "returning to the scrolled audit column")
            check(body(back) == scrolled_body and "To · Builder" in text(back)
                  and draft in prompt_text(back),
                  "audit viewport, shared recipient and draft survive horizontal navigation")
            check(intact_cells(back), "narrow columns keep wide glyphs and cursor coordinates within the viewport")
            save_shot(output, "canvas-narrow-audit", back)

            preview.resize(30, 110)
            preview.send(ALT_LEFT)
            grown = preview.until(lambda shot: shot["size"] == [30, 110]
                                  and view(shot, "conversation") and draft in prompt_text(shot),
                                  "conversation after restoring terminal dimensions")
            check("To · Builder" in text(grown) and DIAGNOSTIC not in body(grown),
                  "resizing restores the conversation with its recipient and diagnostic separation")
            save_shot(output, "canvas-restored", grown)

            # The following local commands replace only this fixture's own draft.
            preview.send(b"\x01\x0b")
            preview.until(lambda shot: draft not in prompt_text(shot), "cleared fixture draft")
            preview.send(b"/new queue\r")
            created = preview.until(lambda shot: view(shot, "queue") and "example-request" in body(shot),
                                    "new queue column inserted after conversation")
            check("To · Builder" in text(created) and column(created)[2] == initial_count + 1,
                  "new columns extend the canvas and share the selected recipient")
            preview.send(ALT_RIGHT)
            preview.until(lambda shot: view(shot, "code audit"),
                          "the original audit column remaining after the inserted queue")
            preview.send(ALT_LEFT)
            preview.until(lambda shot: view(shot, "queue"), "return to the inserted queue column")
            preview.send(b"/new chat\r")
            duplicate = preview.until(lambda shot: view(shot, "conversation"), "a second conversation column")
            check(column(duplicate)[2] == initial_count + 2, "another column of an existing view type is independent")
            preview.send(b"/close\r")
            preview.until(lambda shot: view(shot, "queue"), "closing the duplicate conversation returns to its neighbor")
            preview.send(b"/close\r")
            preview.until(lambda shot: view(shot, "conversation"), "closing the inserted queue restores the original neighbor")
            preview.send(ALT_RIGHT)
            original = preview.until(lambda shot: view(shot, "code audit"), "original audit after removing inserted columns")
            check("To · Builder" in text(original) and column(original)[2] == initial_count
                  and preview.process.poll() is None,
                  "/new and /close extend and shrink the canvas without ending the session")
            preview.send(ALT_RIGHT * 8)
            final = preview.until(lambda shot: view(shot, "tasks"), "last logical column")
            preview.send(ALT_RIGHT)
            bounded_right = snapshot(preview)
            check(view(bounded_right, "tasks") and column(bounded_right)[1] == column(bounded_right)[2]
                  and body(bounded_right) == body(final),
                  "horizontal navigation stops at the last column without wrapping")
        except BaseException:
            save_shot(output, "canvas-failure", snapshot(preview))
            raise
        finally:
            restored = preview.close()
        check(restored and preview.process.returncode == 0,
              "preview restores original terminal attributes and exits cleanly")
        check(b"\x1b[?1000l\x1b[?1006l" in preview.transcript,
              "preview disables mouse reporting on exit")


def run_diff_login(binary, decoder, output, checks):
    def check(value, label):
        if not value:
            raise AssertionError(label)
        checks.append(label)

    def split_visible(shot):
        return any("Old" in row and "New" in row and "│" in row
                   for row in body(shot).splitlines())

    def changes_visible(shot):
        return ("expanded = true" in body(shot) and "expanded = false" in body(shot)
                and "@@ -1 +1 @@" in body(shot))

    with tempfile.TemporaryDirectory(prefix="ssmcp-canvas-diff-login-") as temporary:
        config = Path(temporary) / "config.toml"
        config.write_text('[ui]\nlayout="chat"\nsidebar=false\ncolor=true\n', encoding="utf-8")
        preview = Preview(binary, decoder, config, rows=30, cols=110)
        try:
            initial = preview.until(lambda shot: view(shot, "conversation")
                                    and "src/client/terminal/events.rs" in body(shot),
                                    "collapsed diff tool")
            diff_row = next(index for index, row in enumerate(initial["rows"])
                            if "src/client/terminal/events.rs" in row)
            preview.send(mouse(0, x=8, y=diff_row + 1)
                         + mouse(0, release=True, x=8, y=diff_row + 1))
            opened = preview.until(changes_visible, "expanded diff changes")
            check(not split_visible(opened),
                  "opening a collapsed diff shows all old and new content in the default single layout")
            prompt_y = prompt_start(opened) + 2
            preview.send(mouse(0, x=4, y=prompt_y)
                         + mouse(0, release=True, x=4, y=prompt_y))
            preview.send(b"/diff split\r")
            split = preview.until(lambda shot: split_visible(shot) and changes_visible(shot),
                                  "wide split diff")
            check(any("expanded = true" in row and "expanded = false" in row
                      for row in body(split).splitlines()) and intact_cells(split),
                  "/diff split aligns old and new changes on the same bounded row")
            save_shot(output, "canvas-diff-split", split)

            preview.resize(30, 60)
            narrow = preview.until(lambda shot: shot["size"] == [30, 60]
                                   and changes_visible(shot) and not split_visible(shot),
                                   "single-column fallback on a narrow screen")
            check(intact_cells(narrow),
                  "split diff falls back to complete single-column changes below its minimum width")
            save_shot(output, "canvas-diff-narrow", narrow)
            preview.send(b"/diff\r")
            preferred = preview.until(lambda shot: "Diff layout · split" in text(shot),
                                      "the retained split preference")
            check(changes_visible(preferred) and not split_visible(preferred),
                  "narrow fallback preserves the selected split preference")
            preview.resize(30, 110)
            restored = preview.until(lambda shot: shot["size"] == [30, 110] and split_visible(shot),
                                     "split layout returning after a resize")
            check(changes_visible(restored) and intact_cells(restored),
                  "restoring terminal width restores the selected split diff")

            preview.send(b"/diff single\r")
            single = preview.until(lambda shot: changes_visible(shot) and not split_visible(shot)
                                   and "Diff layout · single" in text(shot), "explicit single diff")
            check(intact_cells(single), "/diff single keeps both versions on a wide screen")
            save_shot(output, "canvas-diff-single", single)
            preview.resize(30, 60)
            single_narrow = preview.until(lambda shot: shot["size"] == [30, 60]
                                          and changes_visible(shot) and not split_visible(shot),
                                          "explicit single diff on a narrow screen")
            check(intact_cells(single_narrow), "single diff stays complete and bounded after narrowing")
            save_shot(output, "canvas-diff-single-narrow", single_narrow)
            preview.resize(30, 110)
            preview.until(lambda shot: shot["size"] == [30, 110] and changes_visible(shot),
                          "the wide conversation before login checks")

            preview.send(b"/new chat\r")
            duplicate = preview.until(lambda shot: view(shot, "conversation")
                                      and column(shot)[2] == column(initial)[2] + 1,
                                      "an independent duplicate conversation")
            expected_column = column(duplicate)
            expected_body = body(duplicate)
            expected_recipient_row = left_rows(duplicate)[prompt_start(duplicate)]
            check("expanded = false" in expected_body and "@@ -1 +1 @@" not in expected_body,
                  "a new conversation column starts with its own collapsed tool cards")
            for command in ["/login", "/login open", "/login copy", "/login link"]:
                preview.send(b"\x01\x0b" + command.encode())
                typed = preview.until(lambda shot: command in prompt_text(shot),
                                      "the login command draft")
                cursor = typed["cursor"]
                preview.send(b"\r")
                rejected = preview.until(
                    lambda shot: "Preview does not start GitHub login" in text(shot),
                    "the explicit preview login rejection",
                )
                check(command in prompt_text(rejected) and rejected["cursor"] == cursor
                      and not rejected["hide_cursor"] and column(rejected) == expected_column
                      and body(rejected) == expected_body
                      and left_rows(rejected)[prompt_start(rejected)] == expected_recipient_row,
                      f"{command} is explicitly rejected in preview without changing draft, cursor, recipient or canvas")
            save_shot(output, "canvas-login-preview", rejected)
            preview.send(ALT_LEFT)
            previous = preview.until(lambda shot: view(shot, "conversation") and changes_visible(shot),
                                     "the original column's expanded diff")
            check("/login link" in prompt_text(previous),
                  "returning to the original conversation restores expansion and preserves the rejected command draft")
            preview.send(ALT_RIGHT)
            returned = preview.until(lambda shot: column(shot) == expected_column
                                     and body(shot) == expected_body, "the independent duplicate column")
            check("/login link" in prompt_text(returned),
                  "the duplicate conversation retains its own collapsed state after the round trip")
        except BaseException:
            save_shot(output, "canvas-diff-login-failure", snapshot(preview))
            raise
        finally:
            restored_terminal = preview.close()
        check(restored_terminal and preview.process.returncode == 0,
              "diff and login preview restores terminal attributes and exits cleanly")


def run_tasks(binary, decoder, output, checks):
    def check(value, label):
        if not value:
            raise AssertionError(label)
        checks.append(label)

    def edit(preview, shot, value):
        y = prompt_start(shot) + 2
        preview.send(mouse(0, x=4, y=y) + mouse(0, release=True, x=4, y=y))
        preview.send(b"\x01\x0b" + value.encode())
        return preview.until(lambda current: value in prompt_text(current), "the local command draft")

    def command(preview, shot, value):
        edit(preview, shot, value)
        preview.send(b"\r")

    def task_list(shot):
        return (view(shot, "tasks") and "fictional task data" in body(shot)
                and "preview-task-build" in body(shot) and "preview-task-test" in body(shot)
                and "Enter details" in body(shot))

    with tempfile.TemporaryDirectory(prefix="ssmcp-task-columns-") as temporary:
        config = Path(temporary) / "config.toml"
        config.write_text('[ui]\nlayout="chat"\nsidebar=false\ncolor=true\n', encoding="utf-8")
        preview = Preview(binary, decoder, config, rows=30, cols=110)
        try:
            initial = preview.until(lambda shot: view(shot, "conversation"), "the eight-column canvas")
            check(column(initial) == ("conversation", 2, 8),
                  "the default canvas has eight columns with conversation second")
            preview.send(ALT_LEFT + b"\x1b[B\r")
            recipient = preview.until(lambda shot: view(shot, "conversation")
                                      and "To · Builder" in text(shot), "the shared task-panel recipient")
            command(preview, recipient, "/tasks")
            tasks = preview.until(task_list, "the actual Tasks panel")
            check(column(tasks) == ("tasks", 8, 8)
                  and "cargo check --workspace" in body(tasks)
                  and "cargo test --lib" in body(tasks)
                  and "running" in body(tasks) and "completed" in body(tasks)
                  and "/preview/fictional-project" in body(tasks)
                  and "To · Builder" in text(tasks),
                  "/tasks opens the canonical Tasks column with explicitly fictional preview job metadata")
            save_shot(output, "canvas-tasks-list", tasks)

            draft = "tasks draft 界"
            typed = edit(preview, tasks, draft)
            cursor = typed["cursor"]
            preview.send(b"\x1b[Z\x1b[B")
            selected = preview.until(
                lambda shot: any("› preview-task-test" in row for row in shot["rows"]),
                "the second task selected by its stable job id",
            )
            check(draft in prompt_text(selected) and "To · Builder" in text(selected),
                  "task selection preserves the shared Unicode draft and recipient")
            preview.send(b"\r")
            detail = preview.until(lambda shot: view(shot, "tasks")
                                   and "Job: preview-task-test" in body(shot), "the selected task details")
            check("State: completed" in body(detail) and "Exit: 0" in body(detail)
                  and "Command: cargo test --lib" in body(detail),
                  "Enter opens the selected task's status and command without launching work")
            save_shot(output, "canvas-tasks-detail", detail)
            preview.resize(12, 60)
            narrow = preview.until(lambda shot: shot["size"] == [12, 60]
                                   and view(shot, "tasks") and "Job: preview-task-test" in body(shot),
                                   "narrow task details")
            preview.send(b"\x1b[6~")
            scrolled = preview.until(lambda shot: view(shot, "tasks") and body(shot) != body(narrow),
                                     "task metadata scrolling")
            check(intact_cells(scrolled), "task details scroll inside a bounded narrow viewport")
            save_shot(output, "canvas-tasks-narrow", scrolled)
            preview.send(LEFT)
            logs = preview.until(logs_open, "Tasks Left reaching the adjacent Logs column")
            check(column(logs)[1] + 1 == column(scrolled)[1]
                  and draft in prompt_text(logs) and "To · Builder" in text(logs),
                  "Tasks Left reaches Logs without changing the draft or recipient")
            preview.send(RIGHT)
            restored = preview.until(lambda shot: view(shot, "tasks"), "Logs Right returning to Tasks")
            check(body(restored) == body(scrolled) and draft in prompt_text(restored),
                  "Logs Right restores the task selection, detail and scroll position")
            preview.send(ESCAPE)
            preview.resize(30, 110)
            listed = preview.until(
                lambda shot: shot["size"] == [30, 110] and task_list(shot)
                and any("› preview-task-test" in row for row in shot["rows"]),
                "the selected task retained in the full-width list",
            )
            check(draft in prompt_text(listed) and "To · Builder" in text(listed),
                  "Escape leaves task details while keeping the selected job and draft")

            preview.send(ALT_LEFT * 5)
            audit = preview.until(lambda shot: view(shot, "code audit"), "the Audit return bookmark origin")
            original_column, original_body = column(audit), body(audit)
            preview.send(mouse(2) + mouse(2, release=True))
            preview.until(logs_open, "Logs opened from Audit")
            preview.send(RIGHT)
            preview.until(task_list, "the adjacent Tasks column during the Logs round trip")
            preview.send(LEFT)
            preview.until(logs_open, "Logs revisited without replacing the return bookmark")
            preview.send(ESCAPE)
            returned = preview.until(lambda shot: column(shot) == original_column, "Escape returning to Audit")
            check(body(returned) == original_body and draft in prompt_text(returned)
                  and "To · Builder" in text(returned),
                  "Audit to Logs to Tasks to Logs then Escape returns to the original bookmarked column")
            preview.send(ALT_LEFT)
            chat = preview.until(lambda shot: view(shot, "conversation"), "the shared composer after task navigation")
            check(draft in prompt_text(chat) and chat["cursor"] == cursor,
                  "task and log navigation preserves the original wide-glyph composer cursor")

            # Only this disposable fixture draft is replaced by the following local commands.
            command(preview, chat, "/tasks")
            current = preview.until(task_list, "Tasks before closing its column")
            command(preview, current, "/close")
            closed_tasks = preview.until(lambda shot: logs_open(shot) and column(shot)[2] == 7,
                                         "closing Tasks without ending its jobs")
            command(preview, closed_tasks, "/tasks")
            reopened_tasks = preview.until(lambda shot: task_list(shot) and column(shot) == ("tasks", 8, 8),
                                           "the canonical Tasks column recreated next to Logs")
            check("To · Builder" in text(reopened_tasks),
                  "closing and reopening Tasks restores the canonical pair and preserves the recipient")
            preview.send(LEFT)
            current_logs = preview.until(logs_open, "Logs before closing its column")
            command(preview, current_logs, "/close")
            closed_logs = preview.until(lambda shot: view(shot, "history") and column(shot)[2] == 7,
                                        "closing the actual Logs column")
            command(preview, closed_logs, "/tasks")
            repaired = preview.until(lambda shot: task_list(shot) and column(shot) == ("tasks", 8, 8),
                                     "Tasks restoring its missing Logs neighbor")
            preview.send(LEFT)
            repaired_logs = preview.until(lambda shot: logs_open(shot) and column(shot) == ("logs", 7, 8),
                                          "the rebuilt adjacent Logs column")
            check("To · Builder" in text(repaired_logs),
                  "dedicated Tasks restores a closed Logs neighbor without deleting jobs or the recipient")

            command(preview, repaired_logs, "/new chat")
            inserted = preview.until(lambda shot: column(shot) == ("conversation", 9, 9),
                                     "a new chat column inserted after the whole Logs and Tasks pair")
            preview.send(ALT_LEFT)
            preview.until(lambda shot: task_list(shot) and column(shot)[1] == 8,
                          "Tasks remaining immediately left of the inserted chat")
            preview.send(ALT_LEFT)
            adjacent_logs = preview.until(lambda shot: logs_open(shot) and column(shot)[1] == 7,
                                          "Logs and Tasks remaining adjacent after insertion")
            check(column(adjacent_logs)[2] == 9 and "To · Builder" in text(adjacent_logs),
                  "/new from Logs inserts after Tasks and never splits the canonical pair")
            check(preview.process.poll() is None, "closing system columns leaves the preview session alive")
        except BaseException:
            save_shot(output, "canvas-tasks-failure", snapshot(preview))
            raise
        finally:
            restored_terminal = preview.close()
        check(restored_terminal and preview.process.returncode == 0,
              "task-column preview restores original terminal attributes and exits cleanly")


def run_exit_aliases(binary, decoder, output, checks):
    def check(value, label):
        if not value:
            raise AssertionError(label)
        checks.append(label)

    for command in ["/exit", "/quit"]:
        with tempfile.TemporaryDirectory(prefix="ssmcp-preview-exit-") as temporary:
            config = Path(temporary) / "config.toml"
            config.write_text('[ui]\nlayout="chat"\nsidebar=false\ncolor=false\n', encoding="utf-8")
            preview = Preview(binary, decoder, config, rows=30, cols=110)
            try:
                initial = preview.until(lambda shot: view(shot, "conversation"), "the exit-test preview")
                if command == "/exit":
                    preview.send(b"/exit extra\r")
                    invalid = preview.until(
                        lambda shot: "arguments" in shot["rows"][-1]
                        and ("without" in shot["rows"][-1] or "no arguments" in shot["rows"][-1]),
                        "the explicit invalid-exit argument error",
                    )
                    check(preview.process.poll() is None and column(invalid) == column(initial)
                          and "/exit extra" in prompt_text(invalid),
                          "/exit extra reports an error without exiting or changing the canvas and draft")
                    save_shot(output, "canvas-exit-invalid", invalid)
                    preview.send(b"\x01\x0b")
                preview.send(command.encode() + b"\r")
                # Require the command itself to end the process before cleanup can send any signal.
                preview.process.wait(timeout=5)
                preview.pump(0.15)
                exited = preview.decoder.request(op="snapshot", cells=True)
                check(preview.process.returncode == 0, f"{command} exits the preview normally with code zero")
                check(not exited["alternate_screen"] and not exited["bracketed_paste"]
                      and not exited["hide_cursor"],
                      f"{command} restores the primary screen, bracketed-paste state and visible cursor")
                check(b"\x1b[?1000l" in preview.transcript and b"\x1b[?1006l" in preview.transcript,
                      f"{command} disables mouse event reporting before exit")
                save_shot(output, "canvas-" + command[1:] + "-restored", exited)
            except BaseException:
                save_shot(output, "canvas-" + command[1:] + "-failure", snapshot(preview))
                raise
            finally:
                restored = preview.close()
            check(restored, f"{command} restores the original PTY termios without a signal-driven exit")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", required=True, type=Path)
    parser.add_argument("--out", required=True, type=Path)
    parser.add_argument("--report", type=Path)
    args = parser.parse_args()
    args.out.mkdir(parents=True, exist_ok=True)
    report = args.report or args.out / "report.json"
    report.parent.mkdir(parents=True, exist_ok=True)
    checks = []
    result = {"suite": "canvas-logs-tasks", "status": "failed", "checks": checks}
    try:
        run((args.bin_dir / "ssmcp").resolve(),
            (args.bin_dir / "examples" / "vt_snapshot").resolve(), args.out, checks)
        run_diff_login((args.bin_dir / "ssmcp").resolve(),
                       (args.bin_dir / "examples" / "vt_snapshot").resolve(), args.out, checks)
        run_tasks((args.bin_dir / "ssmcp").resolve(),
                  (args.bin_dir / "examples" / "vt_snapshot").resolve(), args.out, checks)
        run_exit_aliases((args.bin_dir / "ssmcp").resolve(),
                         (args.bin_dir / "examples" / "vt_snapshot").resolve(), args.out, checks)
        result["status"] = "passed"
    except BaseException as error:
        result["error"] = str(error)
        raise
    finally:
        result["check_count"] = len(checks)
        report.write_text(json.dumps(result, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    print(json.dumps({"status": "passed", "checks": len(checks), "report": str(report)}))


if __name__ == "__main__":
    main()
