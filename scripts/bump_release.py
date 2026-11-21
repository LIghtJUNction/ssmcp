#!/usr/bin/env python3
"""Prepare one immutable application version, then dispatch Release once per Actions run."""

import argparse
import base64
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import tomllib

import release


BOT_NAME = "github-actions[bot]"
BOT_EMAIL = "41898282+github-actions[bot]@users.noreply.github.com"
RETRY_HELP = (
    "No Release POST is sent on a Bump workflow rerun. Inspect the first attempt's "
    "dispatch artifact and continue its recorded Release run. If the first attempt "
    "definitely never sent a POST, start a new explicit Bump workflow dispatch "
    "with level=none for the prepared version; do not edit receipt state. "
    "If delivery is uncertain, check Actions before making any new dispatch."
)


def git(root, *args, env=None):
    return subprocess.check_output(
        ["git", *args], cwd=root, env=env, text=True, encoding="utf-8"
    ).strip()


def ancestor(root, older, newer):
    result = subprocess.run(
        ["git", "merge-base", "--is-ancestor", older, newer], cwd=root, check=False
    )
    if result.returncode not in (0, 1):
        raise subprocess.CalledProcessError(result.returncode, result.args)
    return result.returncode == 0


def require_baseline(root, baseline):
    if not release.SHA.fullmatch(baseline) or git(root, "rev-parse", "HEAD") != baseline:
        raise ValueError("checkout must equal the original workflow event SHA")
    if git(root, "status", "--porcelain", "--untracked-files=normal"):
        raise ValueError("the bump checkout must be clean")


def fetch_main(root):
    git(root, "fetch", "--no-tags", "origin",
        "refs/heads/main:refs/remotes/origin/main")
    return git(root, "rev-parse", "refs/remotes/origin/main")


def tag_commit(root, tag):
    # Only an absent ref is accepted as missing; an unreadable/broken ref fails.
    result = subprocess.run(
        ["git", "show-ref", "--verify", "--quiet", "refs/tags/" + tag],
        cwd=root, check=False
    )
    if result.returncode == 1:
        return None
    if result.returncode:
        raise subprocess.CalledProcessError(result.returncode, result.args)
    return git(root, "rev-parse", f"refs/tags/{tag}^{{commit}}")


def version_commit(root, baseline, tag):
    git(root, "add", "--", "Cargo.toml", "Cargo.lock")
    tree = git(root, "write-tree")
    # Commit identity is deliberately derived from the frozen baseline, not the
    # runner clock or run_attempt. A retry creates the identical Git object.
    date = git(root, "show", "-s", "--format=%ct", baseline) + " +0000"
    env = {
        **os.environ,
        "GIT_AUTHOR_NAME": BOT_NAME, "GIT_AUTHOR_EMAIL": BOT_EMAIL,
        "GIT_COMMITTER_NAME": BOT_NAME, "GIT_COMMITTER_EMAIL": BOT_EMAIL,
        "GIT_AUTHOR_DATE": date, "GIT_COMMITTER_DATE": date,
    }
    sha = git(root, "-c", "commit.gpgSign=false", "-c", "i18n.commitEncoding=UTF-8",
              "commit-tree", tree,
              "-p", baseline, "-m", f"chore: release {tag}", env=env)
    # The index and worktree already contain precisely the new version bytes.
    git(root, "reset", "--soft", sha)
    return sha


def prepare(level, baseline, root=None, *, custom_version=None,
            dry_run=False, publish=True):
    root = Path(root or release.ROOT)
    require_baseline(root, baseline)
    old = release.version(root)
    release.validate_package("v" + old, root)
    release.source_submodules(root)
    if level in ("none", "current"):
        if custom_version:
            raise ValueError("custom_version is only valid with the custom level")
        new = old
    else:
        new = release.bumped_version(old, level, custom_version)
    result = {
        "schema": "ssmcp-bump-preparation-v1", "baseline_sha": baseline,
        "baseline_version": old, "version": new, "tag": "v" + new,
        "dry_run": dry_run, "publish_requested": publish,
        "version_change": new != old, "version_commit": False,
        "sha": None, "tag_prepared": False,
    }
    if dry_run:
        # Exercise the same two-file replacement validation without writing it.
        if new != old:
            release.bump(level, root, custom_version=custom_version, dry_run=True)
        # In particular, do not fetch, stage, create a commit, move any ref or tag.
        return {**result, "status": "preview"}

    remote_main = fetch_main(root)
    if not ancestor(root, baseline, remote_main):
        raise ValueError("the original workflow baseline is no longer on main")
    if new != old:
        release.bump(level, root, custom_version=custom_version)
        sha = version_commit(root, baseline, result["tag"])
    else:
        sha = baseline

    if publish:
        git(root, "fetch", "origin", "--tags")
        existing = tag_commit(root, result["tag"])
        if existing is not None and existing != sha:
            raise ValueError("release tag already points at another commit")

    reused = ancestor(root, sha, remote_main)
    if not reused:
        if remote_main != baseline:
            raise ValueError("main advanced beyond the frozen baseline; no main push was made")
        # A normal fast-forward push also rejects a concurrent unrelated advance.
        # Never reset, force-push or merge a newer main into this version candidate.
        git(root, "push", "origin", f"{sha}:refs/heads/main")
        remote_main = fetch_main(root)
        if not ancestor(root, sha, remote_main):
            raise ValueError("prepared version commit is not on main after push")

    if publish:
        release.create_tag(result["tag"], sha, root)
        remote_main = git(root, "rev-parse", "refs/remotes/origin/main")
    require_baseline(root, sha)
    return {
        **result, "status": "reused" if reused else "prepared",
        "sha": sha, "remote_main_observed": remote_main,
        "version_commit": new != old, "tag_prepared": publish,
    }


def verify_dispatch_source(root, baseline, sha, tag):
    require_baseline(root, baseline)
    if not release.SHA.fullmatch(sha) or not release.TAG.fullmatch(tag):
        raise ValueError("dispatch requires an exact prepared commit and version tag")
    remote_main = fetch_main(root)
    git(root, "fetch", "origin", "--tags")
    if (tag_commit(root, tag) != sha
            or not ancestor(root, baseline, sha)
            or not ancestor(root, sha, remote_main)):
        raise ValueError("prepared tag, baseline and main ancestry do not agree")
    package = tomllib.loads(git(root, "show", sha + ":Cargo.toml"))["package"]
    if package["name"] != "ssmcp" or tag != "v" + package["version"]:
        raise ValueError("prepared tag does not match the committed application version")


def utc_now():
    return datetime.now(timezone.utc).isoformat(timespec="microseconds")


def write_json(path, value):
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n", encoding="utf-8")


def run_details(stdout, repository):
    # Keep the raw bytes in the capture; normalize line endings only for parsing.
    headers, separator, body = stdout.decode("utf-8").replace("\r\n", "\n").partition("\n\n")
    if not separator or not re.fullmatch(r"HTTP/[0-9.]+ 200(?: .*)?", headers.split("\n")[0]):
        raise ValueError("dispatch did not return HTTP 200 with a run identity")
    result = json.loads(body)
    run_id = result.get("workflow_run_id")
    if (type(run_id) is not int or run_id <= 0
            or result.get("run_url") != f"https://api.github.com/repos/{repository}/actions/runs/{run_id}"
            or result.get("html_url") != f"https://github.com/{repository}/actions/runs/{run_id}"):
        raise ValueError("dispatch returned no exact repository/run identity")
    return {key: result[key] for key in ("workflow_run_id", "run_url", "html_url")}


def dispatch(tag, sha, baseline, run_id, run_attempt, report_dir, *,
             root=None, repository=None):
    root = Path(root or release.ROOT)
    repository = repository or os.environ["GITHUB_REPOSITORY"]
    if (not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repository)
            or type(run_id) is not int or run_id <= 0
            or type(run_attempt) is not int or run_attempt <= 0):
        raise ValueError("invalid Actions repository/run identity")
    report_dir = Path(report_dir)
    # Never replace an earlier local receipt, including an ambiguous POST result.
    report_dir.mkdir(parents=True, exist_ok=False)
    result = {
        "schema": "ssmcp-bump-dispatch-v1", "source": "github-actions",
        "repository": repository, "bump_run_id": run_id, "bump_run_attempt": run_attempt,
        "baseline_sha": baseline, "prepared_sha": sha, "tag": tag,
    }
    if run_attempt != 1:
        write_json(report_dir / "result.json",
                   {**result, "status": "rerun_blocked", "message": RETRY_HELP})
        raise ValueError(RETRY_HELP)
    try:
        verify_dispatch_source(root, baseline, sha, tag)
    except (ValueError, subprocess.CalledProcessError) as failure:
        write_json(report_dir / "result.json",
                   {**result, "status": "not_dispatched", "error": str(failure)})
        raise
    argv = [
        "gh", "api", "--method", "POST", "--hostname", "github.com",
        "--header", "X-GitHub-Api-Version: 2026-03-10", "--input", "-", "--include",
        f"repos/{repository}/actions/workflows/release.yml/dispatches",
    ]
    stdin = (json.dumps({
        "ref": "main",
        "inputs": {"tag": tag, "build_only": False, "deploy_server": False},
    }) + "\n").encode("utf-8")
    started = utc_now()
    error = None
    try:
        response = subprocess.run(argv, input=stdin, capture_output=True, timeout=120, check=False)
        stdout, stderr, exit_code = response.stdout, response.stderr, response.returncode
    except subprocess.TimeoutExpired as failure:
        stdout, stderr, exit_code = failure.stdout or b"", failure.stderr or b"", None
        error = "dispatch timed out; delivery is unknown"
    except OSError as failure:
        stdout, stderr, exit_code = b"", b"", None
        error = f"dispatch command could not start: {failure}"
    capture = {
        "schema": "ssmcp-gh-api-capture-v1", "argv": argv,
        "stdin_base64": base64.b64encode(stdin).decode("ascii"),
        "stdout_base64": base64.b64encode(stdout).decode("ascii"),
        "stderr_base64": base64.b64encode(stderr).decode("ascii"),
        "exit_code": exit_code, "started_utc": started, "finished_utc": utc_now(),
    }
    capture_path = report_dir / "dispatch-capture.json"
    write_json(capture_path, capture)
    (report_dir / "stdout.http").write_bytes(stdout)
    (report_dir / "stderr.txt").write_bytes(stderr)
    result["capture_sha256"] = hashlib.sha256(capture_path.read_bytes()).hexdigest()
    if not error and exit_code != 0:
        error = f"dispatch command exited {exit_code}; delivery was not confirmed"
    if not error:
        try:
            result.update(run_details(stdout, repository))
        except (ValueError, UnicodeError, AttributeError) as failure:
            error = str(failure)
    if error:
        result.update(status="dispatch_unconfirmed", error=error, message=RETRY_HELP)
        write_json(report_dir / "result.json", result)
        raise ValueError(error + ". " + RETRY_HELP)
    result["status"] = "dispatched"
    write_json(report_dir / "result.json", result)
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    prepare_parser = commands.add_parser("prepare")
    prepare_parser.add_argument("--baseline", required=True)
    prepare_parser.add_argument("--level", choices=["none", "current", "patch", "minor", "major", "custom"],
                                default="none")
    prepare_parser.add_argument("--custom-version")
    prepare_parser.add_argument("--dry-run", action="store_true")
    prepare_parser.add_argument("--no-publish", dest="publish", action="store_false")
    prepare_parser.add_argument("--report", type=Path, required=True)
    dispatch_parser = commands.add_parser("dispatch")
    dispatch_parser.add_argument("--tag", required=True)
    dispatch_parser.add_argument("--sha", required=True)
    dispatch_parser.add_argument("--baseline", required=True)
    dispatch_parser.add_argument("--run-id", type=int, required=True)
    dispatch_parser.add_argument("--run-attempt", type=int, required=True)
    dispatch_parser.add_argument("--report-dir", type=Path, required=True)
    args = parser.parse_args()
    if args.command == "prepare":
        result = prepare(args.level, args.baseline, custom_version=args.custom_version,
                         dry_run=args.dry_run, publish=args.publish)
        args.report.parent.mkdir(parents=True, exist_ok=True)
        write_json(args.report, result)
        if output := os.environ.get("GITHUB_OUTPUT"):
            with open(output, "a", encoding="utf-8") as stream:
                stream.write(f"tag={result['tag']}\nsha={result['sha'] or ''}\n")
    else:
        result = dispatch(args.tag, args.sha, args.baseline, args.run_id, args.run_attempt,
                          args.report_dir)
    print(json.dumps(result))
    if summary := os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(summary, "a", encoding="utf-8") as stream:
            stream.write("### Bump workflow receipt\n\n"
                         + "~~~json\n" + json.dumps(result, indent=2) + "\n~~~\n")


if __name__ == "__main__":
    main()
