# Defer a production switch while managed jobs are active

A normal release still publishes and deploys by default. An explicit main `workflow_dispatch` can set `deploy_server=false` to publish a verified immutable tag while holding production at its existing version. Build-only runs never enable deployment. This option does not weaken version, main-ancestry, recursive-submodule, archive, registry or runtime gates.

Use this only when the production switch has a concrete handoff constraint. Existing SSMCP clients may cancel native managed jobs when their relay connection closes; watchdog reconnect alone does not preserve the workload. Collect a read-only active-job snapshot and obtain the affected workload owners' checkpoint/normal-completion handoff. Do not stop their jobs to clear the gate.

When pushing a tag whose validated main commit has `[skip ci]`, first inspect the tag's workflow runs, then explicitly dispatch the Release workflow on main with the exact existing tag and `deploy_server=false`. Check that no concurrent automatic publication run is scheduled to deploy. GitHub documents skip instructions for push-triggered workflows: https://docs.github.com/en/actions/how-tos/manage-workflow-runs/skip-workflow-runs . Ordinary tag pushes retain their existing automatic deployment behavior.

After assets are published, keep production status explicitly pending in the report. Once the safe switch handoff is available, use the existing Deploy server workflow with the exact published stable tag and source commit from `release-manifest.json`. Its production environment, deployment concurrency, staging health checks, digest checks and rollback procedure remain in force. Verify the resulting deployment receipt and client reconnections separately from publication.

Local validation for this change uses the existing Python publication and deployment suites. Added cases cover explicit Boolean deferral, invalid inputs, default behavior, build-only denial, and deferred publication retaining exact-tag/lock validation. No Rust product source or dependency pin changes.
