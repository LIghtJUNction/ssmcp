# Final native P1 acceptance controller

This controller runs the SSMCP–zcalld native listener acceptance suite only after both supplied build receipts report success for the exact requested commits. It uses the suite from the verified SSMCP build archives and copies the verified executables into a private run directory.

## Evidence boundaries

- The main archive and all four pinned component archives must match their build-recorded SHA-256 hashes and embedded Git commit IDs. The extracted files, byte counts and executable bits must match the build source manifest.
- Git reads remove inherited repository overrides and disable replacement objects, transport access and lazy fetch. Nested gitlinks and special archive entries fail closed.
- Execution uses owned read-only source and executable copies, a private network/process namespace, a cleared environment and one writable evidence directory.
- Success requires the producer exit code, fresh report, at least 94 recorded assertions, the four exact fake-provider requests, the expected execution/history receipts and two complete 30-row terminal captures.
- Each saved terminal capture must show `P1_NATIVE_RESULT` in both its row text and rendered cell text. Parsing, validation and SHA-256 calculation use the same bytes.
- The final receipt records copied-input and original-input checks, timeout/cleanup errors and the exact source and binary cohort. It does not authenticate the author of a build receipt; the trusted build producers remain part of the evidence boundary.

## Preparation validation

The controller at SHA-256 `f1a6a0eff76ad3efb7e3427918721f46427a923eb6b0737fbe640a6c0aff0783` has nine pure source/archive/copy contracts and a separately reviewed saved-cell marker contract. These checks execute no SSMCP or zcalld product and no model.

A separate export-only check on the recorded `fd8ae13b96b2f93753d3028a2c6074028b804d24` build archives matched all 1,466 files and four dependency commits. That build's Rust gate failed; this export check does not change that result or qualify it for final P1 execution.

## Running

Run `python3 -B test_final_p1_contracts.py` for the pure controller contracts.

Use `python3 -B run_final_p1.py --help` for the final-run arguments. Supply full commit IDs, actual passed build receipts, the native build source manifest and a new output directory outside private masked mounts such as `/tmp`. Do not substitute a historical successful report for a different build cohort.

Final product acceptance and release publication are tracked separately from this controller preparation.
