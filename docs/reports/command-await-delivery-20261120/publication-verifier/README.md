# Explicit-path publication verifier

This is a path-argument successor to the recovered a238 verifier (28,307 bytes; SHA256 a23827c06034964c6ef8aa3299613f7d50cafbd6ddfb2bafeb7b50d814889394). The recovered original is unchanged.

## Usage

```sh
python3 verify_published_candidate.py CANDIDATE_SHA FORMAL_RELEASE_RUN_ID \
  --checkout /absolute/path/to/source-checkout \
  --report-dir /absolute/existing-parent/publication-verification
```

Both path options are required. `--checkout` must be the root of an existing Git working tree and contain the exact candidate commit object. It may have a different HEAD: source reads continue to use the supplied candidate. The four candidate submodule commit/tree objects must also be available for the existing downstream checks.

`--report-dir` is resolved explicitly. An existing directory is accepted; one final directory component may be created if its parent exists. Missing ancestors are not created. Reports retain the name `verification-<first 12 candidate characters>.json`. Ensure the directory is writable.

The two positional parameters remain candidate SHA and formal release run ID. Repository, version v0.10.21, tag-push-only event binding, 19-job and 16-asset checks, official crate checksum, verified-crate/development/registry graph contracts, four gitlinks, and complete `src/**` source-byte checks are unchanged. A workflow-dispatch publication run is still rejected; a preflight run is not a formal publication run.

Only syntax and local argument/path boundaries are exercised for this successor. The network publication verifier is not run, and the original 8 schema tests / 23 tamper cases are not rerun. This does not establish publication success. Rebind the final candidate SHA and formal run after any source change.
