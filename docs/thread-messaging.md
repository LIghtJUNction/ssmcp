# Sessions, threads, and waiting messages

A live ssmcp connection is one logical session. Its `session_id` is the client ID returned by discovery. An explicitly registered agent is one logical thread. Its `thread_id` is the existing `agent_id`; both names identify the same participant. These identifiers do not describe operating-system processes or threads.

## Discover and send

Joining and listing collaboration participants returns `session_id`, `thread_id`, and public presence metadata. Participant tokens are returned only to the joining caller and remain required for binding, sending, receiving, and acknowledging. A display name never authorizes a thread.

Peer messages enter the selected thread's private mailbox. `send` returns `delivery: "queued"`; `receive` reads that mailbox and advances its delivered cursor. `ack` releases only messages already delivered to that participant. Retries with an idempotency key return the original receipt. Reusing a key for different content fails.

The user can target a registered thread, queue a request for the next caller, or broadcast to all current threads in a session. A session broadcast snapshots its current recipients, checks every queue limit, and admits all copies together. Each copy waits for its own authenticated recipient. Joining later does not replay an earlier broadcast. The terminal `/global` target uses these same semantics.

## Isolation

The gateway verifies the participant binding before adding session and thread scope to a native call. It first asks the connected client for `ssmcp.client.capabilities`; only clients advertising `thread_scope: 1` receive the optional scope fields. Existing wire-v7 clients continue receiving the original source-audit envelope.

A thread's token cannot read another thread's mailbox. User requests addressed to one thread are not claimed by unbound calls or another participant. Native memory and trigger handlers receive the authenticated scope through the runtime, rather than accepting a controller name as identity.

## Notifications

User messages and webhook notifications share bounded waiting queues while retaining their origin. A delivered webhook has `source: "webhook"` and a webhook message identifier. Webhook events do not enter the shared user-memory checkpoint as a user requirement. The interface can display an external-event prefix, and the structured origin remains available to the caller.

## Lifetime and limits

A session ID lasts for that live connection. Thread registrations have a renewable 15-minute lease; reconnecting or expired registration requires discovery and a new registration. Queued peer messages remain until acknowledged, expired, or the session ends. The session supports 64 participants, with bounded payloads and mailboxes reported by the collaboration `limits` object.

This change queues and routes messages. It does not launch a model, invoke an external harness, or interrupt a running native operation.
