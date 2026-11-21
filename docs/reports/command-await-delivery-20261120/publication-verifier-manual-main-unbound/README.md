# Manual-main publication verifier — UNBOUND preparation

This is a separate verifier for publication dispatched from **main**. It distinguishes the workflow-definition commit **P** from the product/tag commit **S**. The prepared version is **0.10.25**; final S, P, run ID, dispatch capture and native execution receipt are all unbound. This preparation is not a publication pass or permission to dispatch.

The old v21 and v25 push-only helpers are unchanged. Keep the three active files together: **verify_published_manual_main.py**, **dispatch_receipt_adapter.py**, and **manual_main_provenance.py**. They require Python 3.11 or newer, assertions enabled, Git, authenticated GitHub CLI and the candidate/submodule objects. The verifier performs remote reads and writes evidence into an explicit local report directory. It does not create tags, trigger workflows, publish, install binaries or change services.

## Independent bindings and future usage

**S** must be the final accepted product commit resolved by the v0.10.25 tag. **P** must be the exact main commit reported for the original workflow-dispatch run. The verifier requires S to be an ancestor of P. Both commits must contain the same six pinned recipe files: release.yml, ci.yml, deploy-server.yml, scripts/release.py, scripts/install.sh and scripts/install.ps1. Their byte/SHA-256 pins come from [Q, 647bce2ec9cf74beab43adea831c2e0712d28ea8](https://github.com/LIghtJUNction/ssmcp/commit/647bce2ec9cf74beab43adea831c2e0712d28ea8). Q is historical recipe provenance, not an automatically selected release source. Changed recipes require review; pins are never learned from a run.

The original dispatch capture and native execution receipt require independently recorded SHA-256 values. The expected native job ID and exact remote capture path must come from independent readback of the actual execution. They cannot be learned from the capture being checked. Two self-authored JSON files and matching hashes do not authenticate a request. The parser checks the complete nontruncated native stdout envelope, its cumulative byte cursor, both EOF flags and its exact capture-file binding. The remote capture path is not copied into the public report.

The supported collector records actual argv/stdin/stdout/stderr from **gh api --hostname github.com --include --method POST**, API version **2026-03-10**, with captured stdin. The request must explicitly select main, the exact tag, and false for both build_only and deploy_server. Boolean false and the exact string "false" retain their original types. The actual HTTP 200 response must include workflow_run_id, run_url and html_url. Missing values, zero, empty strings, normalized summaries and old 204 responses do not substitute. The verifier only parses this evidence; it never issues the POST.

Use the following template only after independently binding the actual publication:

~~~sh
python3 ./verify_published_manual_main.py "$FINAL_S" "$RELEASE_RUN_ID" \
  --workflow-sha "$WORKFLOW_P" \
  --checkout "$SOURCE_CHECKOUT" \
  --report-dir "$VERIFICATION_REPORT_DIR" \
  --dispatch-receipt "$LOCAL_ORIGINAL_CAPTURE" \
  --dispatch-receipt-sha256 "$ORIGINAL_CAPTURE_SHA256" \
  --native-execution-receipt "$LOCAL_ORIGINAL_NATIVE_RECEIPT" \
  --native-execution-receipt-sha256 "$ORIGINAL_NATIVE_RECEIPT_SHA256" \
  --native-execution-job-id "$INDEPENDENT_NATIVE_JOB_ID" \
  --native-capture-path "$INDEPENDENT_REMOTE_CAPTURE_PATH"
~~~

The checkout must be an existing Git working-tree root containing S, P and four submodule commit/tree objects. The report directory may exist or have only its final component created; its parent must exist. Do not use python -O or PYTHONOPTIMIZE. Only the original attempt, run_attempt=1, is supported; reruns, partial reruns and preflight/build-only runs fail. P is labelled as independently read run/attempt API head_sha, not as an observed github.workflow_sha context value.

## Implemented checks

The original requirements remain: **19 exact job names, 18 successes and deployment skipped; 16 exact Release assets; four gitlinks; the complete regular source-file set and bytes under src; official crates.io checksum and package identity; and separate development and normalized registry manifests, locks and dependency graphs**. The eleven inherited non-entry, non-API functions remain AST-identical. The API reader now explicitly pins github.com. All source and ancestry reads use Git's no-replace-objects option.

The new identity chain binds the raw request to its returned run, the exact run/attempt and workflow ID/path, and the resolved reusable workflow references at P. Referencing deploy-server.yml does not mean it executed. The validate-step JSON must identify S and the publication mode. The policy JSON must identify the durable v25 deferral, while the actual deployment job must be skipped. Effective policy output cannot replace proof of the original request's explicit false input.

For all 18 successful jobs, logs are downloaded by job IDs from the fixed-attempt API. Checks cover the first pinned checkout, exact requested ref, one canonical checkout git-log command and its immediately following S. Selected windows are located by exact recipe headers, API step identity/order, timestamps and the next actual action boundary. Endgroup is not treated as action completion, and internal composite-action Run headers are not blindly paired with API steps. Missing, repeated or ambiguous identities fail; later favorable output cannot repair an earlier wrong value.

Each Build package JSON and final upload ID/name/size/digest/URL is bound to GitHub artifact metadata. The same IDs must appear in runtime/finalize/publication download windows. Parallel computed download hashes are compared as a multiset, never assigned to IDs by output order. Extra diagnostic artifacts are allowed, while ssmcp-* must select exactly six distinct target artifacts. The modern verified-crates artifact must contain both the official crate and ssmcp-verified-crate.json; the old one-member v20 format is not accepted.

## Actual download scope and platform formats

The helper downloads the manifest, SHA256SUMS, six public sidecars, six public binary archives, the official crate, 18 job logs and seven outer Actions artifact ZIPs. It checks each ZIP's actual size/digest, then reads its exact regular members in memory without extracting arbitrary paths. Outer ZIP hashes remain separate from inner archive and receipt hashes. Actual job API snapshots are saved as labelled canonical JSON, not described as HTTP wire bytes.

Each public binary archive must be byte-identical to its CI artifact member and agree with its package JSON and manifest. The finalize crate must equal the official registry archive, and its receipt must equal the manifest's verified-crate object. Installer checks retain the original GitHub asset size/digest and exact candidate-source comparison; this successor does **not** claim to download installer asset bytes.

Windows sidecars require exact **CRLF**, and Unix sidecars require exact **LF**. This corrects the old helper's LF-only assumption. The pinned [producer at release.py line 547](https://github.com/LIghtJUNction/ssmcp/blob/647bce2ec9cf74beab43adea831c2e0712d28ea8/scripts/release.py#L547) uses native text output; checksums reads the sidecars without rewriting them, and release uploads their bytes unchanged. Independent published v20 Windows/Linux reads confirmed those formats. Full original bytes and hashes remain checked; there is no whitespace stripping.

## Evidence and remaining limits

| Claim | Evidence | Status |
|---|---|---|
| All three files parse as Python | AST parsing of actual saved bytes | Static syntax only |
| Inherited crate/graph/source helper functions are unchanged | AST comparison with the 30,229-byte v25 baseline, SHA 9f75533684446080b4ae6ec82f17d245b174541a23a29a2c3399536680c95109 | Eleven functions identical |
| The new route has real request/log/artifact checks | Three source files and complete diff | Implemented, not executed |
| Log, multiline-input and collector formats have observed examples | Historical v20/v23 API/log data and read-only original GET capture | Format evidence only |
| Final v25 publication is verified | Requires real final S/P/run, POST receipt, successful modern workflow and published bytes | UNBOUND; no pass claimed |

No helper import, CLI invocation, parser execution, offline test, Cargo, PTY, CI trigger or publication action was performed for this preparation. Historical eight tests and 23 tamper cases were not rerun and do not become successor results. Historical v20 success and v23 failure do not establish modern Q/U or final-candidate acceptance.

The trust boundary is a reviewed recipe, pinned actions and the GitHub runner. It addresses missing or later reprinted identity evidence, but is not signed attestation against a malicious runner, action or build script able to corrupt the same machine. Missing logs, expired artifacts, changed recipes, unsupported captures or ambiguous windows fail. The live run identity and attempt are re-read before reporting success.

The development Git lock and the crate's registry lock remain separate inputs. Feature selection and transitive Git-tree evidence remain bound to the successful formal workflow; this verifier does not rerun Cargo. Final product acceptance and selection of S remain the source owner's and reviewer's responsibility.

## Review closeout

The design adds a distinct manual-main route without using run.head_sha as the product SHA. Required terms and inputs are explicit. Experimental evidence is absent because execution is not authorized in this phase. Evaluation is limited to syntax, AST preservation and static review. Future real dispatch and publication evidence must supply the unbound data; there is no unconditional provenance stub standing in for a check.

The preparation manifest records active-file hashes and null final bindings. The complete diff contains main-file changes plus both added modules. The [capture schema](dispatch-capture.schema.json) describes the original collector envelope; the adapter source enforces the additional native receipt and independent binding checks. The unchanged [v25 baseline](../publication-verifier-v25-unbound/verify_published_candidate.py) is retained in its existing directory. Private recovery receipts, unapplied wiring patches and checkpoints are excluded from this public bundle. The active main was reconstructed from the exact v25 baseline, then revised in a separate recovery directory. Public metadata removes private execution identifiers without changing the three active source files or the complete diff.
