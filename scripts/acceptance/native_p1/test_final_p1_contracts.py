#!/usr/bin/env python3
"""Pure controller contracts. These do not execute SSMCP, zcalld or a model."""
from __future__ import annotations

import copy
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
from types import SimpleNamespace
import unittest


MODULE = Path(__file__).with_name("run_final_p1.py")
SPEC = importlib.util.spec_from_file_location("final_p1", MODULE)
p1 = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(p1)


def git(repo, *args):
    return subprocess.check_output(["/usr/bin/git", "-C", str(repo), *args], stderr=subprocess.PIPE).decode().strip()


def init(repo):
    repo.mkdir(parents=True)
    git(repo, "init", "-q")
    git(repo, "config", "user.name", "P1 Contract Fixture")
    git(repo, "config", "user.email", "fixture@example.invalid")


class SourceContracts(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="p1-pure-contract-")
        self.root = Path(self.temp.name)
        self.repo = self.root / "repo"
        init(self.repo)
        (self.repo / "src").mkdir()
        (self.repo / "src/lib.rs").write_text("// synthetic source fixture\n")
        (self.repo / "run.sh").write_text("#!/bin/sh\nexit 0\n")
        (self.repo / "run.sh").chmod(0o755)
        self.components = {}
        for name in ("zharness", "zplugin", "ztuio", "zuse"):
            repo = self.root / name
            init(repo)
            (repo / "component.txt").write_text(name + "\n")
            git(repo, "add", ".")
            git(repo, "commit", "-qm", "Synthetic fixture")
            revision = git(repo, "rev-parse", "HEAD")
            self.components["crates/" + name] = (repo, revision)
            git(self.repo, "update-index", "--add", "--cacheinfo", "160000", revision, "crates/" + name)
        git(self.repo, "add", "src", "run.sh")
        git(self.repo, "commit", "-qm", "Synthetic root fixture")
        self.revision = git(self.repo, "rev-parse", "HEAD")
        self.archives = self.root / "archives"
        self.archives.mkdir()
        self.reference = self.root / "reference"
        self.reference.mkdir()
        main = self.make_archive(self.repo, self.revision, "main", self.reference)
        links = {}
        for name, (repo, revision) in self.components.items():
            destination = self.reference / name
            destination.mkdir(parents=True, exist_ok=True)
            links[name] = self.make_archive(repo, revision, name.rsplit("/", 1)[-1], destination)
        manifest = {}
        for path in self.reference.rglob("*"):
            item = {"type": "directory" if path.is_dir() else "file", "mode": path.stat().st_mode & 0o777}
            if path.is_file():
                item.update(bytes=path.stat().st_size, sha256=p1.sha(path))
            manifest[path.relative_to(self.reference).as_posix()] = item
        self.manifest_path = self.root / "source-manifest.json"
        self.manifest_path.write_text(json.dumps(manifest))
        self.gate = {"source_archives": {"main": main, "gitlinks": links},
                     "source_manifest": {"path": str(self.manifest_path), "sha256": p1.sha(self.manifest_path),
                                         "entries": len(manifest)}}
        self.args = SimpleNamespace(repository=self.repo, source_revision=self.revision)

    def tearDown(self):
        self.temp.cleanup()

    def make_archive(self, repo, revision, name, destination):
        path = self.archives / (name + ".tar")
        with path.open("wb") as stream:
            subprocess.run(["/usr/bin/git", "-C", str(repo), "archive", "--format=tar", revision],
                           check=True, stdout=stream, stderr=subprocess.PIPE)
        with tarfile.open(path) as stream:
            stream.extractall(destination, filter="data")
        return {"archive": str(path), "repository": str(repo), "commit": revision, "sha256": p1.sha(path)}

    def freeze(self, gate=None):
        output = self.root / "output"
        output.mkdir()
        originals = {}
        source, manifest = p1.freeze_source(self.args, output, gate or self.gate, originals)
        return source, manifest, originals

    def test_actual_git_archives_match_recorded_build_tree(self):
        source, manifest, originals = self.freeze()
        self.assertEqual(p1.actual_build_manifest(source), manifest["build_entries"])
        self.assertEqual(len(originals), 6)
        self.assertEqual(set(manifest["gitlinks"]), set(self.components))
        self.assertEqual(manifest["source_archives"]["main"]["sha256"], self.gate["source_archives"]["main"]["sha256"])

    def test_archive_byte_change_is_rejected(self):
        with Path(self.gate["source_archives"]["main"]["archive"]).open("ab") as stream:
            stream.write(b"not the recorded archive")
        with self.assertRaisesRegex(ValueError, "differs from its passed build receipt"):
            self.freeze()

    def test_archive_commit_label_change_is_rejected(self):
        gate = copy.deepcopy(self.gate)
        gate["source_archives"]["main"]["commit"] = "0" * 40
        with self.assertRaisesRegex(ValueError, "another commit"):
            self.freeze(gate)

    def test_archive_with_updated_hash_but_wrong_embedded_commit_is_rejected(self):
        entry = self.gate["source_archives"]["main"]
        other = self.gate["source_archives"]["gitlinks"]["crates/zuse"]
        shutil.copyfile(other["archive"], entry["archive"])
        entry["sha256"] = p1.sha(entry["archive"])
        with self.assertRaisesRegex(ValueError, "embeds another commit"):
            self.freeze()

    def mutate_manifest(self, edit):
        manifest = json.loads(self.manifest_path.read_text())
        edit(manifest)
        self.manifest_path.write_text(json.dumps(manifest))
        self.gate["source_manifest"].update(sha256=p1.sha(self.manifest_path), entries=len(manifest))

    def test_build_manifest_file_hash_mismatch_is_rejected(self):
        self.mutate_manifest(lambda m: m["src/lib.rs"].update(sha256="0" * 64))
        with self.assertRaisesRegex(ValueError, "not the exact tree"):
            self.freeze()

    def test_missing_recorded_file_is_rejected(self):
        self.mutate_manifest(lambda m: m.pop("src/lib.rs"))
        with self.assertRaisesRegex(ValueError, "not the exact tree"):
            self.freeze()

    def test_executable_bit_mismatch_is_rejected(self):
        self.mutate_manifest(lambda m: m["run.sh"].update(mode=0o644))
        with self.assertRaisesRegex(ValueError, "not the exact tree"):
            self.freeze()

    def test_git_environment_override_does_not_select_other_repo(self):
        old = os.environ.get("GIT_DIR")
        os.environ["GIT_DIR"] = str(self.root / "not-a-repository")
        try:
            source, _, _ = self.freeze()
            self.assertTrue((source / "src/lib.rs").is_file())
        finally:
            if old is None:
                os.environ.pop("GIT_DIR", None)
            else:
                os.environ["GIT_DIR"] = old

    def test_artifacts_are_copies_not_shared_paths(self):
        origin = self.root / "synthetic-binary"
        origin.write_bytes(b"non executable fixture bytes")
        out = self.root / "frozen-output"
        out.mkdir()
        digest = p1.sha(origin)
        artifacts, directory = p1.freeze_artifacts(out, {"ssmcp": {"path": str(origin), "sha256": digest}}, {})
        origin.write_bytes(b"replacement")
        self.assertEqual(p1.sha(artifacts["ssmcp"]["path"]), digest)
        self.assertEqual(Path(artifacts["ssmcp"]["path"]), directory / "ssmcp")


if __name__ == "__main__":
    unittest.main(verbosity=2)
