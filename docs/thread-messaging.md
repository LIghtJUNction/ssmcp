# Sessions, threads, and waiting messages

A live ssmcp connection is one logical session. Its `session_id` is the client ID returned by discovery. An explicitly registered agent is one logical thread. Its `thread_id` is the existing `agent_id`; both names identify the same participant. These identifiers do not describe operating-system processes or threads.

## Discover and send

Joining and listing collaboration participants returns `session_id`, `thread_id`, and public presence metadata. Participant tokens are returned only to the joining caller and remain required for binding, sending, receiving, and acknowledging. A display name never authorizes a thread.

Peer messages enter the selected thread's private mailbox. `send` returns `delivery: "queued"`; `receive` reads that mailbox and advances its delivered cursor. `ack` releases only messages already delivered to that participant. Retries with an idempotency key return the original receipt. Reusing a key for different content fails.

The user can target a registered thread, queue a request for the next caller, or broadcast to all current threads in a session. A session broadcast snapshots its current recipients, checks every queue limit, and admits all copies together. Each copy waits for its own authenticated recipient. Joining later does not replay an earlier broadcast. The terminal `/global` target uses these same semantics.

## Isolation

The gateway verifies the participant binding before adding session and thread scope to a native call. It first asks the connected client for `ssmcp.client.capabilities`; only clients advertising `thread_scope: 1` receive the optional scope fields. Clients that do not advertise this capability receive the source-audit envelope without the optional scope fields; normal wire-version admission still applies.

A thread's token cannot read another thread's mailbox. User requests addressed to one thread are not claimed by unbound calls or another participant. The runtime now retains the authenticated scope as an immutable in-process capability, consumed by the program listener. Native memory and trigger handlers have not been connected to this scope. Exact-thread user requests remain in their original history/outbox and are excluded from the shared workspace checkpoint.

## Notifications

The production local-user path is SteeringCommand::Publish through Queue::command, including an atomic snapshot for the /global target. Routing, retry, and capacity tests exercise this actual path. Unconnected generic dispatch/enqueue prototypes were removed from this integration; there is no public webhook-to-thread listener endpoint. Existing peer messages, ordinary user steering, and explicit `/send` program listeners retain separate delivery rules. Webhook-tagged records are excluded from the shared user-memory checkpoint.

## Lifetime and limits

A session ID lasts for that live connection. Thread registrations have a renewable 15-minute lease; reconnecting or expired registration requires discovery and a new registration. Queued peer messages remain until acknowledged, expired, or the session ends. The session supports 64 participants, with bounded payloads and mailboxes reported by the collaboration `limits` object.

This change queues and routes messages. It does not launch a model, invoke an external harness, or interrupt a running native operation.

For the implemented authenticated MCP and private Unix program listener, see [thread-listeners.md](thread-listeners.md).
