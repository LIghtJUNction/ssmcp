# Full acceptance suite controller

`run_bound_v3.py` runs the 18 isolated SSMCP acceptance suites against a passed exact-source build receipt. It verifies the build source manifest, materialized Git tree and pinned component objects, executable hashes, producer provenance and complete saved terminal frames. A failed or timed-out suite stops the run and marks later suites `not_run`.

The paired contract test needs only the controller in the same directory:

```sh
python3 -B test_run_bound_v3_r2.py
```

These 19 controller contracts use temporary repositories, synthetic files and owned cleanup fixtures. They do not execute an SSMCP/zcalld product or a model. The checked controller SHA-256 is `3bbe4a7e74ff9bf7305cbf4aedb49ae19a28a4bcfeed49958e9c5a1d15ffb94b`; the test SHA-256 is `67467048112e5344a49fedcd544f096c8d4deaefb86dcc17fdd90caf84b59570`. The original preparation receipts retain their execution paths and are under `docs/verification/2026-11-20/`.

Use `python3 -B run_bound_v3.py --help` for the actual-suite arguments. Successful controller contracts are preparation evidence, not successful product acceptance. A final product run requires the matching passed build receipt and frozen artifacts.
