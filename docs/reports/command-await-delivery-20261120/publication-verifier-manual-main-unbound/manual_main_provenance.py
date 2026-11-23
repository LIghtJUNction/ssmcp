"""Read-only provenance checks for the separately pinned publication recipe.

This is an ordinary reviewed GitHub Actions chain, not an attestation against a
malicious runner, build script, or compromised action. Job logs are obtained by
the job IDs in one exact attempt. An ambiguous or missing identity is an error;
there is no whole-log fallback that searches for a later favorable value.
"""

from collections import Counter
from dataclasses import dataclass
import datetime as dt
import hashlib
import io
import json
from pathlib import PurePosixPath
import re
import stat
import zipfile


REPOSITORY = "LIghtJUNction/ssmcp"
CHECKOUT = "actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1"
UPLOAD = "actions/upload-artifact@cf430e030ddbb5b0abf93d22962f4752f3646cd9"
DOWNLOAD = "actions/download-artifact@9000827ccba6bdab643e8b6fd33ac0654aef8333"
PYTHON = "actions/setup-python@6d4fd22b20465ccc0f7354476588e8df7c9ca0e0"
RUST = "dtolnay/rust-toolchain@39008827e6b4319a349e9a42dfc90579dd25f1ca"
TARGETS = (
    "x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu",
    "x86_64-apple-darwin", "aarch64-apple-darwin",
    "x86_64-pc-windows-msvc", "aarch64-pc-windows-msvc",
)
DEFERRED_REASON = (
    "Preserve active relay sessions until a separate non-disruptive server switch is verified.")
TIMESTAMP = re.compile(r"^(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d{1,9})?Z) (.*)$")
GIT_LOG = re.compile(
    r'^\[command\](?:/usr/bin/git|/usr/local/bin/git|/opt/homebrew/bin/git|'
    r'"C:\\Program Files\\Git\\bin\\git\.exe") log -1 --format=%H$')


def need(condition, message):
    if not condition:
        raise ValueError("execution provenance: " + message)


def sha256(data):
    return hashlib.sha256(data).hexdigest()


def utc(value):
    need(isinstance(value, str) and re.fullmatch(
        r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d{1,9})?Z", value),
        "missing or malformed UTC timestamp")
    return dt.datetime.fromisoformat(value[:-1] + "+00:00")


def unique_json(text):
    def pairs(items):
        result = {}
        for key, value in items:
            need(key not in result, "duplicate JSON key")
            result[key] = value
        return result
    return json.loads(text, object_pairs_hook=pairs)


@dataclass(frozen=True)
class Line:
    number: int
    text: str
    time: object


class JobLog:
    def __init__(self, data, job, uploads, downloads):
        need(isinstance(data, bytes) and 0 < len(data) <= 128 * 1024 * 1024,
             "empty or oversized job log")
        text = data.decode("utf-8")
        if text.startswith("\ufeff"):
            text = text[1:]
        # Only the first BOM, one outer timestamp and one CR in CRLF are removed.
        # Identity records never undergo ANSI removal or arbitrary whitespace repair.
        self.lines = []
        for number, physical in enumerate(text.split("\n"), 1):
            if physical.endswith("\r"):
                physical = physical[:-1]
            match = TIMESTAMP.fullmatch(physical)
            self.lines.append(Line(number, match[2] if match else physical,
                                   utc(match[1]) if match else None))
        self.job = job
        self.steps = {}
        for step in job["steps"]:
            number = step.get("number")
            need(type(number) is int and number > 0 and number not in self.steps,
                 "duplicate or invalid API step number")
            need(step.get("status") == "completed"
                 and step.get("conclusion") in ("success", "skipped"),
                 "incomplete or failed API step")
            self.steps[number] = step
        need(list(self.steps) == sorted(self.steps), "API step order differs")
        self.step(1, "Set up job")
        self.step(2, "Run " + CHECKOUT)
        self.headers = [i for i, line in enumerate(self.lines)
                        if line.text.startswith("##[group]Run ")]
        need(self.headers and self.lines[self.headers[0]].text == "##[group]Run " + CHECKOUT,
             "first executing log action is not the pinned checkout")
        self.action_counts = {CHECKOUT: 1, UPLOAD: uploads, DOWNLOAD: downloads}
        for action, count in self.action_counts.items():
            family = action.split("@", 1)[0]
            positions = [i for i in self.headers
                         if self.lines[i].text.startswith("##[group]Run " + family + "@")]
            need(len(positions) == count and all(
                self.lines[i].text == "##[group]Run " + action for i in positions),
                "pinned action count or revision differs: " + family)
        self.windows = []

    def step(self, number, name):
        step = self.steps.get(number)
        need(step is not None and step.get("name") == name
             and step.get("conclusion") == "success", "API step identity differs: " + name)
        need(utc(step["started_at"]) <= utc(step["completed_at"]),
             "API step times are reversed")
        return step

    def positions(self, header):
        return [i for i in self.headers if self.lines[i].text == "##[group]Run " + header]

    def timed(self, line, step):
        need(line.time is not None
             and utc(step["started_at"]) <= line.time
             < utc(step["completed_at"]) + dt.timedelta(seconds=1),
             "identity line falls outside its API step")

    def window(self, number, name, header, *, occurrence=0, total=1,
               next_header=None, next_number=None, next_name=None):
        step = self.step(number, name)
        starts = self.positions(header)
        need(len(starts) == total and 0 <= occurrence < total,
             "missing or ambiguous action header: " + header)
        start = starts[occurrence]
        following = [i for i in self.headers if i > start]
        cleanup = [i for i in range(start + 1, len(self.lines))
                   if self.lines[i].text == "Post job cleanup."]
        boundary = min(following[:1] + cleanup[:1], default=len(self.lines))
        next_steps = [s for n, s in self.steps.items() if n > number
                      and s["conclusion"] != "skipped"
                      and not s["name"].startswith("Post ") and s["name"] != "Complete job"]
        if next_header is None:
            need(boundary < len(self.lines)
                 and self.lines[boundary].text == "Post job cleanup." and not next_steps,
                 "final artifact upload is not followed solely by trusted cleanup")
            need(not following, "a later Run header follows the final artifact upload")
        else:
            next_step = self.step(next_number, next_name)
            need(next_steps and next_steps[0]["number"] == next_number,
                 "next API execution step differs")
            expected_total = self.action_counts.get(next_header, 1)
            need(len(self.positions(next_header)) == expected_total,
                 "missing or repeated next-step boundary")
            need(boundary < len(self.lines)
                 and self.lines[boundary].text == "##[group]Run " + next_header,
                 "next actual Run boundary differs from the reviewed recipe")
            self.timed(self.lines[boundary], next_step)
        self.timed(self.lines[start], step)
        window = Window(self, step, start, boundary)
        self.windows.append({"step_number": number, "step_name": name,
                             "first_line": start + 1, "last_line": boundary,
                             "boundary_basis": "unique recipe headers plus API step order and timestamps"})
        return window


class Window:
    def __init__(self, log, step, start, end):
        self.log, self.step = log, step
        self.lines = log.lines[start:end]

    def inputs(self, multiline_path=False):
        need(len(self.lines) > 2 and self.lines[1].text == "with:",
             "action has no initial with block")
        self.log.timed(self.lines[1], self.step)
        fields = {}
        path_continued = False
        blank_after_path = False
        for line in self.lines[2:]:
            if line.text in ("env:", "##[endgroup]"):
                self.log.timed(line, self.step)
                break
            match = re.fullmatch(r"  ([A-Za-z0-9_-]+): (.*)", line.text)
            if match:
                self.log.timed(line, self.step)
                key, value = match.groups()
                need(key not in fields, "duplicate action input")
                fields[key] = value
                continue
            # Actual runner multi-line inputs have bare, untimestamped continuation
            # lines. Only this exact reviewed two-path value is supported here.
            if (multiline_path and fields.get("path") == "target/package/*.crate"
                    and not path_continued and line.time is None
                    and line.text == "target/package/ssmcp-verified-crate.json"):
                fields["path"] += "\n" + line.text
                path_continued = True
                continue
            if (multiline_path and path_continued and not blank_after_path
                    and line.time is None and line.text == ""):
                blank_after_path = True
                continue
            need(False, "unsupported or ambiguous action input continuation")
        else:
            need(False, "unterminated action input block")
        return fields

    def matching(self, pattern):
        result = []
        compiled = re.compile(pattern)
        for line in self.lines:
            match = compiled.fullmatch(line.text)
            if match:
                self.log.timed(line, self.step)
                result.append((line, match))
        return result

    def one(self, pattern):
        rows = self.matching(pattern)
        need(len(rows) == 1, "missing or duplicated identity record in mapped step")
        return rows[0][1]

    def json(self):
        rows = [line for line in self.lines
                if line.text.startswith("{") and line.text.endswith("}")]
        need(len(rows) == 1, "step JSON is missing or ambiguous")
        self.log.timed(rows[0], self.step)
        value = unique_json(rows[0].text)
        need(isinstance(value, dict), "step JSON must be an object")
        return value


def action_shape(name):
    if name == "validate":
        return 0, 0, PYTHON
    if name.startswith("Build "):
        return (3 if "windows" in name else 1), 0, RUST
    if name.startswith("Verify installed server "):
        return 0, 1, DOWNLOAD
    if name == "finalize":
        return 1, 1, RUST
    if name == "publish_crates":
        return 0, 1, RUST
    if name == "publish_release":
        return 0, 2, RUST
    if name.startswith("checks / Client ("):
        return 1, 0, PYTHON
    if name == "checks / linux (ubuntu-24.04)":
        return 2, 0, RUST
    if name == "checks / linux (ubuntu-24.04-arm)":
        return 1, 0, RUST
    need(False, "unknown required job name")


def verify_checkout(log, candidate, tag, next_action):
    window = log.window(2, "Run " + CHECKOUT, CHECKOUT,
                        next_header=next_action, next_number=3, next_name="Run " + next_action)
    inputs = window.inputs()
    need(inputs.get("repository") == REPOSITORY
         and inputs.get("ref") == (tag if log.job["name"] == "validate" else candidate)
         and inputs.get("persist-credentials") == "false" and inputs.get("clean") == "true",
         "checkout inputs do not bind the expected source")
    if not log.job["name"].startswith("Verify installed server "):
        need(inputs.get("submodules") == "recursive", "recursive source checkout is missing")
    if log.job["name"] in ("validate", "finalize", "publish_crates", "publish_release"):
        need(inputs.get("fetch-depth") == "0", "publication checkout is not full-history")
    commands = [i for i, line in enumerate(log.lines) if GIT_LOG.fullmatch(line.text)]
    need(len(commands) == 1, "canonical checkout git log is absent or repeated")
    index = commands[0]
    need(index + 1 < len(log.lines)
         and log.lines[index] in window.lines and log.lines[index + 1] in window.lines,
         "checkout commit was printed outside the initial checkout")
    log.timed(log.lines[index], window.step)
    log.timed(log.lines[index + 1], window.step)
    need(log.lines[index + 1].text == candidate,
         "the first checkout identity differs; later favorable values are not considered")
    return {"source_commit": candidate, "requested_ref": inputs["ref"],
            "git_log_line": log.lines[index].number,
            "identity_line": log.lines[index + 1].number}


def verify_validation(log, candidate, version):
    window = log.window(
        4, "Validate the tag, versions, main ancestry and pinned submodules",
        "python scripts/release.py validate --event", next_header="python - <<'PY'",
        next_number=5, next_name="Select deployment after validated publication")
    value = window.json()
    need(value == {"tag": "v" + version, "version": version, "sha": candidate,
                   "submodule_count": 4, "prerelease": "false", "publish": "true",
                   "create_tag": "false", "build_only": "false"},
         "validate output differs from the exact release candidate or publication mode")
    policy = log.window(
        5, "Select deployment after validated publication", "python - <<'PY'",
        next_header="python -m unittest discover -s scripts -p 'test_release.py' -v",
        next_number=6, next_name="Test publication safeguards").json()
    need(policy == {"publish": "true", "deploy_server": "false", "version": version,
                    "deployment_deferred": DEFERRED_REASON},
         "effective durable deployment deferral is absent")
    return {"validate": value, "deployment_policy": policy,
            "policy_output_does_not_replace_original_dispatch_inputs": True}


def package_output(log, target):
    output = log.window(
        6, "Smoke test and package executables",
        'python scripts/release.py package --target "$TARGET"',
        next_header='python scripts/test_installers.py "$TARGET"', next_number=7,
        next_name="Verify native installation, update and failed-download handling").json()
    name = "ssmcp-" + target + (".zip" if "windows" in target else ".tar.gz")
    need(set(output) == {"archive", "sha256", "bytes"} and output["archive"] == name
         and type(output["bytes"]) is int and output["bytes"] > 0
         and re.fullmatch(r"[0-9a-f]{64}", output["sha256"]), "package output differs")
    return output


def artifact_matches(artifact, *, name, size, digest, run):
    need(type(artifact.get("id")) is int and artifact["id"] > 0
         and artifact.get("name") == name and artifact.get("size_in_bytes") == size
         and type(size) is int and size > 0 and artifact.get("digest") == "sha256:" + digest
         and artifact.get("expired") is False
         and artifact.get("workflow_run", {}).get("id") == run["id"]
         and artifact.get("workflow_run", {}).get("head_sha") == run["head_sha"],
         "artifact API metadata does not match this run and log identity")


def upload_output(log, *, number, name, occurrence, total, artifact, run, crate=False):
    step_name = ("Retain the verified application crate for publishing" if crate else "Run " + UPLOAD)
    window = log.window(number, step_name, UPLOAD, occurrence=occurrence, total=total)
    inputs = window.inputs(multiline_path=crate)
    expected = {"name": name,
                "path": ("target/package/*.crate\ntarget/package/ssmcp-verified-crate.json"
                         if crate else "dist/*"),
                "compression-level": "0", "if-no-files-found": "error", "retention-days": "14",
                "overwrite": "false", "include-hidden-files": "false", "archive": "true"}
    need(inputs == expected, "artifact upload inputs differ from the reviewed recipe")
    digest = window.one(r"SHA256 digest of uploaded artifact is ([0-9a-f]{64})")[1]
    finalized = window.one(r"Artifact (\S+) successfully finalized\. Artifact ID ([1-9][0-9]*)")
    result = window.one(
        r"Artifact (\S+) has been successfully uploaded! Final size is ([1-9][0-9]*) bytes\. Artifact ID is ([1-9][0-9]*)")
    url = window.one(r"Artifact download URL: (https://github\.com/[^ ]+)")[1]
    artifact_id, size = int(result[3]), int(result[2])
    need(finalized[1] == result[1] == name and int(finalized[2]) == artifact_id == artifact["id"]
         and url == f"https://github.com/{REPOSITORY}/actions/runs/{run['id']}/artifacts/{artifact_id}",
         "upload ID/name/URL records disagree")
    artifact_matches(artifact, name=name, size=size, digest=digest, run=run)
    return {"job_id": log.job["id"], "step_number": number, "artifact_id": artifact_id,
            "name": name, "bytes": size, "sha256": digest}


def download_output(log, *, number, occurrence, total, artifacts, run,
                    name=None, path, next_header, next_number, next_name):
    window = log.window(number, "Run " + DOWNLOAD, DOWNLOAD, occurrence=occurrence, total=total,
                        next_header=next_header, next_number=next_number, next_name=next_name)
    inputs = window.inputs()
    expected = {"path": path, "merge-multiple": "false" if name else "true",
                "repository": REPOSITORY, "run-id": str(run["id"]),
                "skip-decompress": "false", "digest-mismatch": "error"}
    expected["name" if name else "pattern"] = name if name else "ssmcp-*"
    need(inputs == expected, "artifact download inputs differ from this exact run")
    rows = window.matching(
        r"- (\S+) \(ID: ([1-9][0-9]*), Size: ([1-9][0-9]*), Expected Digest: sha256:([0-9a-f]{64})\)")
    need(len(rows) == len(artifacts), "download selection count differs")
    selected = {}
    for _, match in rows:
        artifact_name, artifact_id, size, digest = match.groups()
        need(artifact_name not in selected and artifact_name in artifacts,
             "download selection has duplicate or unexpected names")
        artifact = artifacts[artifact_name]
        need(int(artifact_id) == artifact["id"], "download is from another producer artifact")
        artifact_matches(artifact, name=artifact_name, size=int(size), digest=digest, run=run)
        selected[artifact_name] = {"id": int(artifact_id), "sha256": digest}
    computed = [match[1] for _, match in window.matching(
        r"SHA256 digest of downloaded artifact is ([0-9a-f]{64})")]
    need(Counter(computed) == Counter(row["sha256"] for row in selected.values()),
         "downloaded digest multiset differs; parallel output cannot be paired by order")
    need(int(window.one(r"Total of ([1-9][0-9]*) artifact\(s\) downloaded")[1]) == len(artifacts),
         "download final count differs")
    window.one(r"Download artifact has finished successfully")
    need(len(window.matching(r"Artifact download completed successfully\.")) == len(artifacts),
         "individual artifact download completions are missing")
    return {"job_id": log.job["id"], "step_number": number, "artifacts": selected}


def save_bytes(directory, name, data):
    need(re.fullmatch(r"[A-Za-z0-9_.-]+", name), "invalid local evidence filename")
    path = directory / name
    if path.exists():
        need(path.is_file() and not path.is_symlink() and path.read_bytes() == data,
             "existing evidence differs; refusing to overwrite it")
    else:
        with path.open("xb") as stream:
            stream.write(data)
    return {"file": name, "bytes": len(data), "sha256": sha256(data)}


def list_artifacts(api_json, run):
    result = []
    total = None
    for page in range(1, 11):
        value = api_json(f"repos/{REPOSITORY}/actions/runs/{run['id']}/artifacts?per_page=100&page={page}")
        need(type(value.get("total_count")) is int and 0 <= value["total_count"] <= 1000,
             "unexpected artifact collection size")
        if total is None:
            total = value["total_count"]
        need(value["total_count"] == total and isinstance(value.get("artifacts"), list),
             "artifact collection changed during pagination")
        result.extend(value["artifacts"])
        if len(result) >= total:
            break
        need(len(value["artifacts"]) == 100, "artifact pagination is incomplete")
    need(len(result) == total and len({a["id"] for a in result}) == len(result),
         "artifact enumeration is incomplete or duplicated")
    return result


def read_zip(data, expected):
    """Inspect exact regular members in memory; never extract a ZIP path to disk."""
    members = {}
    with zipfile.ZipFile(io.BytesIO(data)) as archive:
        entries = archive.infolist()
        need(len(entries) == len(expected), "artifact ZIP has an unexpected member count")
        for entry in entries:
            name = entry.filename
            path = PurePosixPath(name)
            kind = stat.S_IFMT(entry.external_attr >> 16)
            need(name == path.as_posix() and not path.is_absolute()
                 and ".." not in path.parts and "\\" not in name
                 and name in expected and name not in members
                 and not entry.is_dir() and kind in (0, stat.S_IFREG)
                 and not entry.flag_bits & 1,
                 "artifact ZIP contains an unsafe, duplicate or unexpected member")
            need(0 < entry.file_size <= 512 * 1024 * 1024, "artifact member size is invalid")
            value = archive.read(entry)
            need(len(value) == entry.file_size, "artifact member size differs")
            members[name] = value
    need(set(members) == set(expected), "artifact ZIP regular file set differs")
    return members


def verify_execution(run, jobs, candidate, version, manifest, assets, official_crate,
                     *, api_json, api_bytes, download, evidence_dir):
    """Bind a single successful attempt through its real uploads and consumers."""
    need(run["event"] == "workflow_dispatch" and run["head_branch"] == "main"
         and run["run_attempt"] == 1 and run["conclusion"] == "success",
         "not the original successful manual-main attempt")
    need(jobs["deploy_server"]["conclusion"] == "skipped", "deployment did not remain skipped")
    evidence_dir.mkdir(exist_ok=True)
    need(evidence_dir.is_dir() and not evidence_dir.is_symlink(), "invalid evidence directory")
    logs, log_receipts = {}, {}
    for name, job in sorted(jobs.items()):
        if name == "deploy_server":
            continue
        need(job["run_id"] == run["id"] and job["run_attempt"] == 1
             and job["head_sha"] == run["head_sha"] and job["conclusion"] == "success",
             "job does not belong to this successful attempt")
        data = api_bytes(f"repos/{REPOSITORY}/actions/jobs/{job['id']}/logs")
        receipt = save_bytes(evidence_dir, f"job-{job['id']}.log", data)
        receipt["acquisition"] = "authenticated GET /actions/jobs/{job_id}/logs response body"
        api_snapshot = json.dumps(job, sort_keys=True, ensure_ascii=False, indent=2).encode() + b"\n"
        receipt["api_job"] = save_bytes(evidence_dir, f"job-{job['id']}-api-decoded.json", api_snapshot)
        receipt["api_job"]["encoding"] = "canonical JSON of the fixed-attempt API record, not HTTP wire bytes"
        uploads, downloads, next_action = action_shape(name)
        log = JobLog(data, job, uploads, downloads)
        receipt["checkout"] = verify_checkout(log, candidate, "v" + version, next_action)
        logs[name], log_receipts[name] = log, receipt
    need(len(logs) == 18, "required checkout count differs")
    validation = verify_validation(logs["validate"], candidate, version)
    all_artifacts = list_artifacts(api_json, run)
    snapshot = json.dumps(all_artifacts, sort_keys=True, ensure_ascii=False, indent=2).encode() + b"\n"
    artifact_snapshot = save_bytes(evidence_dir, "artifact-api-decoded.json", snapshot)
    artifact_snapshot["encoding"] = "canonical JSON of decoded API records, not HTTP wire bytes"
    expected_names = {"ssmcp-" + target for target in TARGETS}
    relevant = [a for a in all_artifacts if a["name"].startswith("ssmcp-")
                or a["name"] == "verified-crates"]
    artifacts = {a["name"]: a for a in relevant}
    need(set(artifacts) == expected_names | {"verified-crates"}
         and len(relevant) == len(artifacts), "release artifact names are missing or duplicated")
    uploaded, packages = {}, {}
    for target in TARGETS:
        name = "ssmcp-" + target
        log = logs["Build " + target]
        packages[name] = package_output(log, target)
        count = 3 if "windows" in target else 1
        uploaded[name] = upload_output(log, number=12, name=name, occurrence=count - 1,
                                       total=count, artifact=artifacts[name], run=run)
        if "windows" in target:
            # The two diagnostic upload headers must precede the final package;
            # their API identities are checked without treating them as release assets.
            for number, step_name, occurrence in (
                (10, "Retain real Windows browser dependency evidence", 0),
                (11, "Retain Windows browser bootstrap evidence", 1)):
                step = log.step(number, step_name)
                log.timed(log.lines[log.positions(UPLOAD)[occurrence]], step)
    uploaded["verified-crates"] = upload_output(
        logs["finalize"], number=9, name="verified-crates", occurrence=0, total=1,
        artifact=artifacts["verified-crates"], run=run, crate=True)
    consumed = []
    release_artifacts = {name: artifacts[name] for name in expected_names}
    for target in TARGETS[:2]:
        name = "ssmcp-" + target
        consumed.append(download_output(
            logs["Verify installed server " + target], number=3, occurrence=0, total=1,
            artifacts={name: artifacts[name]}, run=run, name=name, path="dist",
            next_header="sudo apt-get update", next_number=4,
            next_name="Install system runtime dependencies"))
    consumed.append(download_output(
        logs["finalize"], number=5, occurrence=0, total=1, artifacts=release_artifacts,
        run=run, path="dist", next_header='if [ -n "$RELEASE_TAG" ]; then', next_number=6,
        next_name="Recheck the source, submodules and existing release tag"))
    consumed.append(download_output(
        logs["publish_crates"], number=5, occurrence=0, total=1,
        artifacts={"verified-crates": artifacts["verified-crates"]}, run=run,
        name="verified-crates", path="target/package",
        next_header='python scripts/release.py validate --tag "$RELEASE_TAG"', next_number=6,
        next_name="Recheck release tag identity"))
    consumed.append(download_output(
        logs["publish_release"], number=5, occurrence=0, total=2, artifacts=release_artifacts,
        run=run, path="dist", next_header=DOWNLOAD, next_number=6, next_name="Run " + DOWNLOAD))
    consumed.append(download_output(
        logs["publish_release"], number=6, occurrence=1, total=2,
        artifacts={"verified-crates": artifacts["verified-crates"]}, run=run,
        name="verified-crates", path="target/package",
        next_header='python scripts/release.py validate --tag "$RELEASE_TAG"', next_number=7,
        next_name="Recheck tag identity and every target archive"))
    records = {record["name"]: record for record in manifest["artifacts"]}
    bridges = {}
    for name, artifact in sorted(artifacts.items()):
        data = api_bytes(f"repos/{REPOSITORY}/actions/artifacts/{artifact['id']}/zip")
        need(len(data) == artifact["size_in_bytes"]
             and "sha256:" + sha256(data) == artifact["digest"],
             "downloaded outer artifact ZIP differs from GitHub metadata")
        receipt = save_bytes(evidence_dir, f"artifact-{artifact['id']}.zip", data)
        if name == "verified-crates":
            crate_name = "ssmcp-" + version + ".crate"
            contents = read_zip(data, {crate_name, "ssmcp-verified-crate.json"})
            need(contents[crate_name] == official_crate,
                 "finalize crate bytes differ from the official registry archive")
            crate_receipt = unique_json(contents["ssmcp-verified-crate.json"].decode("utf-8"))
            need(crate_receipt == manifest["verified_crate"],
                 "finalize verified-crate receipt differs from the release manifest")
        else:
            package = packages[name]
            archive_name = package["archive"]
            contents = read_zip(data, {archive_name, archive_name + ".sha256"})
            archive = contents[archive_name]
            archive_sha = sha256(archive)
            need(len(archive) == package["bytes"] == records[archive_name]["bytes"]
                 and archive_sha == package["sha256"] == records[archive_name]["sha256"],
                 "inner release archive differs from package JSON or manifest")
            # package() writes through the native platform text stream.
            newline = "\r\n" if archive_name.endswith("-pc-windows-msvc.zip") else "\n"
            need(contents[archive_name + ".sha256"]
                 == f"{archive_sha}  {archive_name}{newline}".encode(),
                 "inner release sidecar differs")
            public = download(assets[archive_name]["browser_download_url"])
            need(public == archive and len(public) == assets[archive_name]["size"]
                 and assets[archive_name].get("digest") == "sha256:" + sha256(public),
                 "public release archive bytes differ from the CI-produced archive")
            receipt["public_archive"] = {"name": archive_name, "bytes": len(public),
                                         "sha256": sha256(public), "byte_identical": True}
        receipt["members"] = {key: {"bytes": len(value), "sha256": sha256(value)}
                              for key, value in sorted(contents.items())}
        receipt["artifact_id"] = artifact["id"]
        bridges[name] = receipt
    for name, log in logs.items():
        log_receipts[name]["mapped_windows"] = sorted(log.windows, key=lambda w: w["first_line"])
    return {"verified": True, "run_id": run["id"], "run_attempt": 1,
            "workflow_api_head_sha": run["head_sha"], "candidate_source_sha": candidate,
            "checkout_count": len(logs), "validation": validation,
            "job_logs": log_receipts, "artifact_api": artifact_snapshot,
            "producer_uploads": uploaded, "consumer_downloads": consumed,
            "downloaded_artifact_bytes": bridges,
            "trust_boundary": "Reviewed recipe, pinned actions and GitHub runner; not malicious-builder attestation"}
