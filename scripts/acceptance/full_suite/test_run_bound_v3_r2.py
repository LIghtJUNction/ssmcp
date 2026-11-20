#!/usr/bin/env python3
"""Controller contract tests only: no product executable, suite or Cargo is run."""
import copy
import importlib.util
import hashlib
import io
import json
import pathlib
import signal
import subprocess
import tarfile
import tempfile
import types
import unittest
from unittest import mock

HERE = pathlib.Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("acceptance_controller", HERE / "run_bound_v3.py")
runner = importlib.util.module_from_spec(spec)
spec.loader.exec_module(runner)

class Contracts(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="ssmcp-controller-contract-")
        self.root = pathlib.Path(self.tmp.name)
        self.repo = self.root / "repository"
        self.repo.mkdir()
        env = runner.clean_environment()
        def git(*args):
            return subprocess.check_output(
                ["/usr/bin/git", "-C", str(self.repo), "-c", "user.name=Contract Fixture",
                 "-c", "user.email=contract@example.invalid", *args], env=env, stderr=subprocess.PIPE)
        git("init", "--quiet")
        (self.repo / "payload.txt").write_text("immutable unit fixture\n")
        git("add", "payload.txt")
        git("commit", "--quiet", "-m", "unit fixture")
        self.revision = git("rev-parse", "HEAD").decode().strip()
        self.source = self.root / "source"
        self.source.mkdir()
        with tarfile.open(fileobj=io.BytesIO(git("archive", "--format=tar", self.revision))) as archive:
            archive.extractall(self.source, filter="data")
        self.bins = self.root / "bins"
        self.bins.mkdir()
        for name in ("ssmcp", "ssmcpd", "ssmcp-frontend", "vt_snapshot", "ssmcp-tests", "old-server"):
            p = self.bins / name
            p.write_bytes(("NOT AN EXECUTED PRODUCT: " + name + "\n").encode())
            p.chmod(0o555)
        (self.bins / "examples").mkdir()
        (self.bins / "examples/vt_snapshot").symlink_to(self.bins / "vt_snapshot")
        self.gates = self.root / "unit-gates.json"
        self.gate = {
            "status": "passed", "source_revision": self.revision,
            "checks": [{"name": "synthetic_parser_contract_only", "exit_code": 0}],
            "binaries": {name: {"path": str(self.bins / name), "sha256": runner.digest(self.bins / name)}
                         for name in ("ssmcp", "ssmcpd", "ssmcp-frontend", "vt_snapshot")},
            "library_binary": str(self.bins / "ssmcp-tests"),
            "library_sha256": runner.digest(self.bins / "ssmcp-tests"),
        }
        manifest = self.root / "tested-source-manifest.json"
        entries = runner.tree_manifest(self.source)
        manifest.write_text(json.dumps(entries))
        self.gate["source_manifest"] = {
            "path": str(manifest), "entries": len(entries),
            "sha256": runner.digest(manifest)}
        self.gates.write_text(json.dumps(self.gate))
        self.args = types.SimpleNamespace(
            source_path=self.source, source_revision=self.revision, git_dir=self.repo / ".git",
            bin_dir=self.bins, decoder=self.bins / "vt_snapshot",
            fixture_binary=self.bins / "ssmcp-tests", old_server=self.bins / "old-server",
            gates=self.gates, expected_version="0.10.21", source_version="0.10.21",
            out=self.root / "owned-output", suite_timeout=1,
        )
    def tearDown(self):
        self.tmp.cleanup()

    def test_interactions_integer_requires_exact_validated_passed_checks(self):
        self.assertEqual(runner.check_count({"checks": 2, "passed_checks": ["one", "two"]}, "interactions"), 2)
        for bad in ({"checks": 2}, {"checks": 2, "passed_checks": ["one"]},
                    {"checks": True, "passed_checks": ["one"]},
                    {"checks": 1, "passed_checks": [None]},
                    {"checks": 1, "passed_checks": [{"name": "bad", "passed": False}]}):
            with self.subTest(report=bad), self.assertRaises(RuntimeError):
                runner.check_count(bad, "interactions")

    def test_list_reports_reject_malformed_entries_or_wrong_totals(self):
        self.assertEqual(runner.check_count({"checks": [{"name": "ok", "passed": True}]}, "account_compat"), 1)
        for bad in ({"checks": [None]}, {"checks": [{}]}, {"checks": [""]},
                    {"checks": [{"name": "bad", "passed": False}]},
                    {"checks": ["ok"], "count": 2}, {"checks": 1}):
            with self.subTest(report=bad), self.assertRaises(RuntimeError):
                runner.check_count(bad, "composer")

    def test_source_archive_binding_rejects_changed_or_extra_files(self):
        receipt = runner.verify_source_revision(self.args, runner.tree_manifest(self.source))
        self.assertEqual(receipt["source_revision"], self.revision)
        (self.source / "payload.txt").write_text("changed\n")
        with self.assertRaises(RuntimeError):
            runner.verify_source_revision(self.args, runner.tree_manifest(self.source))
        (self.source / "payload.txt").write_text("immutable unit fixture\n")
        (self.source / "extra.py").write_text("not tracked\n")
        with self.assertRaises(RuntimeError):
            runner.verify_source_revision(self.args, runner.tree_manifest(self.source))

    def test_gate_requires_passed_exact_source_all_artifacts_and_strict_zero_exits(self):
        receipt = runner.validate_gates(self.args, runner.tree_manifest(self.source))
        self.assertEqual(len(receipt["artifacts"]), 5)
        mutations = [
            lambda g: g.update(status="failed"),
            lambda g: g.update(source_revision="f" * 40),
            lambda g: g.update(checks=[]),
            lambda g: g.update(checks=[{"exit_code": False}]),
            lambda g: g.update(checks=[{"exit_code": 1}]),
            lambda g: g["binaries"].pop("ssmcp-frontend"),
            lambda g: g.update(library_sha256="0" * 64),
            lambda g: g["binaries"]["vt_snapshot"].update(sha256="0" * 64),
        ]
        for mutate in mutations:
            gate = copy.deepcopy(self.gate)
            mutate(gate)
            self.gates.write_text(json.dumps(gate))
            with self.subTest(gate=gate), self.assertRaises(RuntimeError):
                runner.validate_gates(self.args, runner.tree_manifest(self.source))

    def test_report_reader_rejects_symlinks_and_non_object_json(self):
        evidence = self.root / "evidence"
        evidence.mkdir()
        original = self.root / "old.json"
        original.write_text('{"status":"passed","checks":["old"]}')
        report = evidence / "report.json"
        report.symlink_to(original)
        with self.assertRaises(RuntimeError):
            runner.read_json_file(report, evidence)
        report.unlink()
        report.write_text("[]")
        with self.assertRaises(RuntimeError):
            runner.read_json_file(report, evidence)

    def test_vt_evidence_requires_complete_decoder_schema(self):
        frame = {"ok": True, "size": [1, 2], "rows": ["  "], "cursor": [0, 0],
                 "alternate_screen": True, "hide_cursor": False}
        self.assertTrue(runner.vt_snapshot(frame))
        for key in ("ok", "cursor", "hide_cursor", "alternate_screen"):
            value = dict(frame)
            value.pop(key)
            self.assertFalse(runner.vt_snapshot(value))
        self.assertFalse(runner.vt_snapshot({**frame, "size": [2, 2]}))

    def _baseline(self):
        source = runner.tree_manifest(self.source)
        return {"source_files": source, "files": {
                    "candidate_gates": {"sha256": runner.digest(self.gates)},
                    "old_server": {"sha256": runner.digest(self.args.old_server)}},
                "input_set_sha256": "a" * 64, "legacy_code_mode": {}}

    def _run_mocked_main(self, outcome):
        baseline = self._baseline()
        gate = {"receipt": {"path": str(self.gates), "sha256": runner.digest(self.gates)}}
        old = {sig: signal.getsignal(sig) for sig in (signal.SIGINT, signal.SIGTERM)}
        try:
            with mock.patch.object(runner, "parse_args", return_value=self.args), \
                 mock.patch.object(runner, "validate_inputs"), \
                 mock.patch.object(runner, "validate_gates", return_value=gate), \
                 mock.patch.object(runner, "verify_gated_baseline"), \
                 mock.patch.object(runner, "static_source_checks", return_value=("unit-engine", {})), \
                 mock.patch.object(runner, "git_read", return_value=runner.OLD_BLOB_ID.encode()), \
                 mock.patch.object(runner, "input_snapshot", return_value=baseline), \
                 mock.patch.object(runner, "execute_suite", side_effect=outcome) as execute:
                code = runner.main()
                self.assertEqual(execute.call_count, 1)
            return code, json.loads((self.args.out / "acceptance.json").read_text())
        finally:
            for sig, handler in old.items():
                signal.signal(sig, handler)

    def test_ordinary_failed_suite_stops_and_marks_all_later_rows_not_run(self):
        code, receipt = self._run_mocked_main(
            lambda args, name, engine, baseline: {"name": name, "status": "failed", "error": "synthetic failure"})
        self.assertEqual(code, 1)
        self.assertEqual(receipt["checks"][0]["status"], "failed")
        self.assertEqual(len(receipt["checks"]), 18)
        self.assertTrue(all(row["status"] == "not_run" for row in receipt["checks"][1:]))

    def test_escaped_suite_exception_remains_an_accountable_failed_row(self):
        code, receipt = self._run_mocked_main(RuntimeError("synthetic suite construction failure"))
        self.assertEqual(code, 1)
        self.assertEqual(receipt["checks"][0]["status"], "failed")
        self.assertTrue(all(row["status"] == "not_run" for row in receipt["checks"][1:]))

    def test_preflight_failure_is_recorded_before_any_suite(self):
        old = {sig: signal.getsignal(sig) for sig in (signal.SIGINT, signal.SIGTERM)}
        try:
            with mock.patch.object(runner, "parse_args", return_value=self.args), \
                 mock.patch.object(runner, "validate_inputs", side_effect=RuntimeError("synthetic missing input")), \
                 mock.patch.object(runner, "execute_suite") as execute:
                self.assertEqual(runner.main(), 1)
                execute.assert_not_called()
            receipt = json.loads((self.args.out / "acceptance.json").read_text())
            self.assertEqual(receipt["status"], "failed")
            self.assertTrue(all(row["status"] == "not_run" for row in receipt["checks"]))
        finally:
            for sig, handler in old.items():
                signal.signal(sig, handler)

    def test_abnormal_wait_still_reaps_only_the_owned_child(self):
        scripts = self.source / "scripts"
        scripts.mkdir()
        (scripts / "e2e_composer.py").write_text("# not executed\n")
        self.args.out.mkdir()
        process = mock.Mock()
        process.returncode = None
        process.poll.side_effect = lambda: process.returncode
        process.wait.side_effect = RuntimeError("synthetic wait fault")
        reaped = []
        def stop(owned):
            self.assertIs(owned, process)
            reaped.append(owned)
            owned.returncode = -15
        with mock.patch.object(runner, "sandbox_command", return_value=(["never-executed"], [])), \
             mock.patch.object(runner, "input_snapshot", return_value={}), \
             mock.patch.object(runner, "input_check", return_value={"unchanged": True}), \
             mock.patch.object(runner.subprocess, "Popen", return_value=process), \
             mock.patch.object(runner, "stop_owned_process", side_effect=stop), \
             mock.patch.object(runner, "evidence_inventory", return_value=([], [])):
            row = runner.execute_suite(self.args, "composer", "unit-engine", {})
        self.assertEqual(row["status"], "failed")
        self.assertTrue(row["owned_child_reaped"])
        self.assertTrue(reaped)
        self.assertIn("synthetic wait fault", row["error"])


    def test_materialized_gitlink_uses_explicit_pinned_object_store_not_moving_head(self):
        child = self.root / "child-repository"
        child.mkdir()
        def git(repo, *args):
            return subprocess.check_output(
                ["/usr/bin/git", "-C", str(repo), "-c", "user.name=Contract Fixture",
                 "-c", "user.email=contract@example.invalid", *args],
                env=runner.clean_environment(), stderr=subprocess.PIPE)
        git(child, "init", "--quiet")
        (child / "child.txt").write_text("pinned child\n")
        git(child, "add", "child.txt")
        git(child, "commit", "--quiet", "-m", "pinned child")
        child_revision = git(child, "rev-parse", "HEAD").decode().strip()
        (self.repo / ".gitmodules").write_text(
            '[submodule "part"]\n\tpath = crates/part\n\turl = https://example.invalid/part.git\n')
        git(self.repo, "add", ".gitmodules")
        git(self.repo, "update-index", "--add", "--cacheinfo", "160000," + child_revision + ",crates/part")
        git(self.repo, "commit", "--quiet", "-m", "fixed child pin")
        self.args.source_revision = git(self.repo, "rev-parse", "HEAD").decode().strip()
        with tarfile.open(fileobj=io.BytesIO(git(self.repo, "archive", self.args.source_revision))) as archive:
            archive.extractall(self.source, filter="data")
        target = self.source / "crates/part"
        target.mkdir(parents=True, exist_ok=True)
        with tarfile.open(fileobj=io.BytesIO(git(child, "archive", child_revision))) as archive:
            archive.extractall(target, filter="data")
        with self.assertRaises(RuntimeError):
            runner.verify_source_revision(self.args, runner.tree_manifest(self.source))
        self.args.submodule_git_dirs = {"crates/part": child / ".git"}
        (child / "child.txt").write_text("different moving HEAD\n")
        git(child, "commit", "--quiet", "-am", "advance unrelated HEAD")
        receipt = runner.verify_source_revision(self.args, runner.tree_manifest(self.source))
        self.assertEqual(receipt["archives"][1]["revision"], child_revision)
        (target / "child.txt").write_text("wrong child\n")
        with self.assertRaises(RuntimeError):
            runner.verify_source_revision(self.args, runner.tree_manifest(self.source))

    def test_claimed_provenance_and_cleanup_must_match_actual_inputs(self):
        digest = "a" * 64
        baseline = {"files": {name: {"sha256": digest} for name in
                    ("ssmcp", "ssmcpd", "ssmcp-frontend", "decoder", "fixture_binary", "old_server")},
                    "source_files": {"scripts/e2e_message_references.py": {"sha256": digest}}}
        valid = {"status": "passed", "checks": ["one"], "source_revision": self.revision,
                 "binaries": {"ssmcp": digest}, "sources": {"e2e_message_references.py": digest},
                 "cleanup": {"all_exited": True, "owned_processes": [{"exit_code": 0}]}}
        self.assertEqual(runner.verify_report(valid, "message_references", self.args, "unit-engine", baseline), 1)
        for field, value in (("source_revision", "b" * 40), ("binaries", {"ssmcp": "b" * 64}),
                             ("sources", {"e2e_message_references.py": "b" * 64}),
                             ("cleanup", {"all_exited": False}), ("inputs_unchanged", False),
                             ("models_started", True)):
            with self.subTest(field=field), self.assertRaises(RuntimeError):
                runner.verify_report({**valid, field: value}, "message_references",
                                     self.args, "unit-engine", baseline)


    def test_gate_source_manifest_hash_and_built_bytes_are_required(self):
        original = copy.deepcopy(self.gate)
        self.gate["source_manifest"]["sha256"] = "b" * 64
        self.gates.write_text(json.dumps(self.gate))
        with self.assertRaises(RuntimeError):
            runner.validate_gates(self.args, runner.tree_manifest(self.source))
        self.gate = original
        manifest_path = pathlib.Path(self.gate["source_manifest"]["path"])
        entries = json.loads(manifest_path.read_text())
        entries["payload.txt"]["sha256"] = "b" * 64
        manifest_path.write_text(json.dumps(entries))
        self.gate["source_manifest"]["sha256"] = runner.digest(manifest_path)
        self.gates.write_text(json.dumps(self.gate))
        with self.assertRaises(RuntimeError):
            runner.validate_gates(self.args, runner.tree_manifest(self.source))

    def test_gate_manifest_cannot_drop_materialized_dependency_files(self):
        manifest_path = pathlib.Path(self.gate["source_manifest"]["path"])
        entries = json.loads(manifest_path.read_text())
        entries["crates/dependency/pinned.txt"] = dict(entries["payload.txt"])
        manifest_path.write_text(json.dumps(entries))
        self.gate["source_manifest"].update(sha256=runner.digest(manifest_path), entries=len(entries))
        self.gates.write_text(json.dumps(self.gate))
        with self.assertRaisesRegex(RuntimeError, "tested manifest"):
            runner.validate_gates(self.args, runner.tree_manifest(self.source))

    def test_git_provenance_disables_inherited_configuration_and_lazy_fetch(self):
        with mock.patch.dict(runner.os.environ, {"GIT_DIR": "/untrusted", "GIT_CONFIG_COUNT": "1",
                                                "GIT_NO_LAZY_FETCH": "0"}):
            environment = runner.clean_environment()
        self.assertNotIn("GIT_DIR", environment)
        self.assertNotIn("GIT_CONFIG_COUNT", environment)
        self.assertEqual(environment["GIT_NO_REPLACE_OBJECTS"], "1")
        self.assertEqual(environment["GIT_NO_LAZY_FETCH"], "1")



def test_gated_baseline_binds_inputs_and_originals(self):
    binding = runner.validate_gates(self.args, runner.tree_manifest(self.source))
    with mock.patch.object(runner, "legacy_blob", return_value={"sha256": "0" * 64}):
        baseline = runner.input_snapshot(self.args)
    runner.verify_gated_baseline(binding, baseline)
    for name in ("ssmcp", "decoder_alias", "gate_origin_ssmcpd",
                 "gate_source_manifest", "candidate_gates"):
        with self.subTest(input=name):
            changed = json.loads(json.dumps(baseline))
            changed["files"][name]["sha256"] = "0" * 64
            with self.assertRaisesRegex(RuntimeError, "changed during gate verification"):
                runner.verify_gated_baseline(binding, changed)


def test_main_rejects_prebaseline_change_before_any_suite(self):
    binding = runner.validate_gates(self.args, runner.tree_manifest(self.source))
    targets = (self.bins / "ssmcp", pathlib.Path(self.gate["source_manifest"]["path"]))
    for index, target in enumerate(targets):
        with self.subTest(input=target.name):
            self.args.out = self.root / ("baseline-race-" + str(index))
            original, mode = target.read_bytes(), target.stat().st_mode & 0o777
            old = {sig: signal.getsignal(sig) for sig in (signal.SIGINT, signal.SIGTERM)}
            def change_after_verification(_args):
                target.chmod(mode | 0o200)
                target.write_bytes(original + b" ")
                return "unit-engine", {}
            try:
                with mock.patch.object(runner, "parse_args", return_value=self.args), \
                     mock.patch.object(runner, "validate_inputs"), \
                     mock.patch.object(runner, "validate_gates", return_value=binding), \
                     mock.patch.object(runner, "static_source_checks", side_effect=change_after_verification), \
                     mock.patch.object(runner, "git_read", return_value=runner.OLD_BLOB_ID.encode()), \
                     mock.patch.object(runner, "legacy_blob", return_value={"sha256": "0" * 64}), \
                     mock.patch.object(runner, "execute_suite") as execute:
                    self.assertEqual(runner.main(), 1)
                    execute.assert_not_called()
                receipt = json.loads((self.args.out / "acceptance.json").read_text())
                self.assertEqual(receipt["status"], "failed")
                self.assertIn("changed during gate verification", receipt["error"])
                self.assertTrue(all(row["status"] == "not_run" for row in receipt["checks"]))
            finally:
                target.chmod(mode | 0o200)
                target.write_bytes(original)
                target.chmod(mode)
                for sig, handler in old.items():
                    signal.signal(sig, handler)


Contracts.test_gated_baseline_binds_inputs_and_originals = test_gated_baseline_binds_inputs_and_originals
Contracts.test_main_rejects_prebaseline_change_before_any_suite = test_main_rejects_prebaseline_change_before_any_suite



def test_actual_source_keys_from_three_fixed_producers(self):
    cases = {
        "presentations": ["scripts/e2e_presentations.py", "scripts/e2e.py",
            "scripts/e2e_dashboard.py", "scripts/e2e_composer.py", "scripts/e2e_input_history.py",
            "src/client/presentation.rs", "src/client/interface/messages.rs",
            "src/client/interface.rs", "src/code_mode.rs"],
        "context_memory": ["scripts/e2e_context_memory.py", "scripts/e2e_input_history.py",
            "scripts/e2e_composer.py", "scripts/e2e_dashboard.py", "scripts/e2e.py",
            "src/client/context_memory.rs", "src/client/interface/history.rs",
            "src/code_mode.rs", "src/http.rs", "src/runtime.rs"],
        "relay_composer": ["scripts/e2e_relay_composer.py", "scripts/e2e_composer.py",
            "scripts/e2e_dashboard.py", "scripts/e2e.py", "scripts/e2e_account_relay.py",
            "src/http/relay_composer_fixture.rs"],
    }
    binaries = {name: {"sha256": hashlib.sha256(("binary:" + name).encode()).hexdigest()}
                for name in ("ssmcp", "ssmcpd", "ssmcp-frontend", "decoder",
                             "fixture_binary", "old_server")}
    for name, paths in cases.items():
        with self.subTest(producer=name):
            sources = {path: {"sha256": hashlib.sha256(path.encode()).hexdigest()} for path in paths}
            baseline = {"files": binaries, "source_files": sources}
            reported_binaries = {"ssmcp": binaries["ssmcp"]["sha256"],
                                 "vt_snapshot": binaries["decoder"]["sha256"]}
            if name == "relay_composer":
                reported_binaries["router_test_fixture"] = binaries["fixture_binary"]["sha256"]
            else:
                reported_binaries["ssmcpd"] = binaries["ssmcpd"]["sha256"]
            report = {"status": "passed", "checks": ["synthetic producer hash contract"],
                      "sources": {path: entry["sha256"] for path, entry in sources.items()},
                      "binaries": reported_binaries}
            self.assertEqual(runner.verify_report(report, name, self.args, "unit-engine", baseline), 1)
            rust = next(path for path in paths if path.startswith("src/"))
            changed = json.loads(json.dumps(report))
            changed["sources"][rust] = "0" * 64
            with self.assertRaisesRegex(RuntimeError, "mismatched hash"):
                runner.verify_report(changed, name, self.args, "unit-engine", baseline)


def test_router_fixture_alias_requires_the_gated_library_hash(self):
    sources = {"scripts/e2e_relay_composer.py": {"sha256": "a" * 64}}
    binaries = {name: {"sha256": hashlib.sha256(("binary:" + name).encode()).hexdigest()}
                for name in ("ssmcp", "ssmcpd", "ssmcp-frontend", "decoder",
                             "fixture_binary", "old_server")}
    baseline = {"files": binaries, "source_files": sources}
    report = {"status": "passed", "checks": ["synthetic router fixture contract"],
              "sources": {"e2e_relay_composer.py": "a" * 64},
              "binaries": {"router_test_fixture": binaries["fixture_binary"]["sha256"]}}
    self.assertEqual(runner.verify_report(report, "relay_composer", self.args, "unit-engine", baseline), 1)
    report["binaries"]["router_test_fixture"] = binaries["ssmcp"]["sha256"]
    with self.assertRaisesRegex(RuntimeError, "router_test_fixture"):
        runner.verify_report(report, "relay_composer", self.args, "unit-engine", baseline)


Contracts.test_actual_source_keys_from_three_fixed_producers = test_actual_source_keys_from_three_fixed_producers
Contracts.test_router_fixture_alias_requires_the_gated_library_hash = test_router_fixture_alias_requires_the_gated_library_hash


if __name__ == "__main__":
    unittest.main(verbosity=2)
