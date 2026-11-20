# Native messaging source checkpoint

This independent next-iteration source checkpoint adds explicitly selected native recipients, a bounded authenticated mailbox, broker routing, negotiated wire transport and the terminal sender API. It does not change the current v0.10.14 release candidate.

Recipients use immutable connection IDs and, when available, verified registered agent IDs. A device account inbox is explicitly labeled as delivery to that account on its next eligible call; an observed model label is never treated as an authenticated agent. Cross-client routing checks the sender's live account access to the source and destination, resolves the exact destination identity, and preserves local-user versus agent origin.

Request IDs deduplicate sends across destinations within the bounded receipt history. Mailbox admission is atomic and bounded. Pending messages are not silently evicted. Inclusion in a response and recipient acknowledgement are separate states; neither proves reading or execution. Revoked or disconnected scopes expire without rerouting. Mailboxes and receipts are scoped to the live broker process and connection; this checkpoint does not promise persistence across a server restart.

Only authenticated devices negotiating native_messages_v1 receive the new local protocol. Existing strict wire-v7 peers keep their earlier Ready/frame behavior. Public messages helpers queue plain text: @paths are not implicitly read or transferred.

Validation executed: cargo fmt --all, git diff --check and node --check src/code_mode.js. Seven mailbox regression tests have been written for identity/client/agent isolation, concurrent deduplication, exact acknowledgement, budgets, expiry, capacity and typed origin, but no Cargo compilation or Rust test has run for this checkpoint.

Pending: public HTTP tool/catalog and automatic finalizer integration, terminal recipient picker and draft persistence, compiled/real authenticated transport and UI acceptance, release documentation and versioning. The UI and HTTP work is assigned to the existing collaborating agents in separate source trees.
