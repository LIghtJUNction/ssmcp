# 0.10.25 publication verifier — UNBOUND preparation

This is an independently stored successor to the [published 0.10.21 path-argument verifier](https://github.com/LIghtJUNction/ssmcp/blob/90a83b0b74d7553d9c34470c96f5a3233d984b74/docs/reports/command-await-delivery-20261120/publication-verifier/verify_published_candidate.py). Its only code change is the version literal from `0.10.21` to `0.10.25`. The original file and its published copies remain unchanged. The complete code difference is in `changes-from-v21.diff`.

**Status: UNBOUND.** No final candidate SHA or formal release run ID is bound to this preparation, and the publication verifier has not been executed. Preparing or publishing these files does not establish a successful build, test, release, installation or deployment.

## Binding and use after formal publication

The source/release owner must supply the exact final candidate commit and its successful tag-push release run. Recheck the candidate workflow and manifest contracts before use; any later source change requires a fresh binding. Do not substitute a preflight run, a workflow-dispatch run, or results from an earlier candidate.

```sh
python3 verify_published_candidate.py FINAL_CANDIDATE_SHA FORMAL_RELEASE_RUN_ID \
  --checkout /absolute/path/to/source-checkout \
  --report-dir /absolute/existing-parent/publication-verification-v25
```

The placeholders are intentionally not executable values. Use Python 3.11 or newer with assertions enabled: do not pass `-O` or `-OO`, and do not set `PYTHONOPTIMIZE`. The existing verifier uses Python assertions for several release checks. A working authenticated `gh` command, Git, and network access to GitHub and crates.io are needed for actual execution.

Both path options remain required. The checkout must be an existing Git working-tree root containing the exact candidate commit and the four candidate submodule commit/tree objects. Its current HEAD may differ: source reads use the supplied commit. The report directory must exist or have an existing parent; only its final component may be created. The output filename remains `verification-<first 12 candidate characters>.json`.

## Preserved verification contracts

| Check | Required relationship |
| --- | --- |
| Formal workflow | Exact candidate `head_sha`, `.github/workflows/release.yml`, `event=push`, `head_branch=v0.10.25`, completed and successful |
| Workflow jobs | Exactly 19 named jobs: 18 successful and `deploy_server` skipped |
| Git tag | `v0.10.25` resolves to the exact candidate commit |
| GitHub release | Published, not draft or prerelease, with exactly 16 expected uploaded assets |
| Release manifest | Version, commit, eight artifact records, hashes, sizes, checksum sidecars and four gitlink commit/tree identities match |
| Official crate | crates.io checksum, unyanked state, package version, clean VCS commit, verified-crate receipt and complete `src/**` file set/bytes match |
| Dependency records | Candidate development lock/graph and the separately normalized registry manifest/lock/graph retain their existing checks |

The historical `5d087d…` schema-provenance comment remains unchanged. It identifies the source of the receipt schema, not the candidate to verify.

## Exact download and execution scope

The verifier downloads the release manifest, `SHA256SUMS`, six archive checksum sidecars, registry metadata and the official crate. It compares binary archive and installer sizes/hashes through GitHub asset metadata; installer hashes also match candidate source bytes. It does **not** independently download every binary archive or installer. Cargo feature selection and transitive Git trees remain bound to the formal workflow receipt; this helper does not rerun Cargo metadata.

This preparation checks the one-line source difference and Python syntax without importing or executing the verifier. It does not rerun the earlier argument/path cases, eight schema tests or 23 tamper cases. It does not install a binary or change production services. `preparation-manifest.json` records these limits and file identities; it is preparation evidence, not a publication result.

## File identities

| File | Bytes | SHA-256 |
| --- | ---: | --- |
| Original 0.10.21 verifier | 30,229 | `27804d3605f2d0eecf8434100a193a89829dd7324b6150da5036cafc8bbd0755` |
| This 0.10.25 verifier | 30,229 | `9f75533684446080b4ae6ec82f17d245b174541a23a29a2c3399536680c95109` |
| Complete one-line diff | 460 | `dae5a5cbe9f7de33d23a6480c6e8cb0d3bef6788071f74e6c1eba786583d54f7` |
