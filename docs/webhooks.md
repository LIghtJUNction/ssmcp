# Named webhook notifications

A client can send a notification to a destination selected by its operator. An account gateway can accept a notification for one live account or thread mailbox. Notifications arrive through the native message inbox with `sender.kind` set to `webhook`.

## Configure the sending client

Create a JSON file outside the controlled workspace, use an absolute path, and set `SSMCP_WEBHOOK_CONFIG` to that path before starting the client. On Unix, the file must belong to the effective user or root and only the owner may write it. The file must be a regular file and cannot be a symbolic link. The client reads it once at startup. The example selects a destination named `build`.

```json
{
  "schema_version": 1,
  "destinations": {
    "build": {
      "url": "https://receiver.example/hooks/build",
      "token_env": "BUILD_NOTIFICATION_TOKEN",
      "description": "Build completion"
    }
  }
}
```

Set the optional token variable in the environment of the sending client. Each named destination controls its own credential. The native catalog lists the name, description and whether a credential is configured. Callers supply a destination name, event, text and optional data.

```javascript
return await call(clientId, "ssmcp.webhooks.notify", {
  destination: "build",
  event: "build.done",
  delivery_id: "build-41",
  text: "All checks passed",
  data: { checks: 12 }
});
```

An HTTP success receipt records acceptance by the configured endpoint. The sender follows no redirects and makes one attempt. After a timeout, check the receiver before retrying with the same delivery ID. A receiver can use this ID for deduplication.

## Receive in SSMCP

Configure the destination URL as `https://HOST/api/client/webhooks?recipient_id=RECIPIENT`. URL-encode the exact recipient ID returned by the native `messages` list. The default OAuth resource is `mcp`. If the authorization grant was issued for a named resource, add `resource=NAME` to the query. The bearer token must belong to the recipient account. The gateway also verifies the live session and the target client's ability to decode webhook senders.

POST a JSON object with `schema_version` set to 1 and the fields `id`, `event` and `text`. Optional fields are `data` and `created_at_ms`. The receiver limits the encoded event, text and data to the native mailbox's 16 KiB per message. A successful HTTP 202 response contains the native queue receipt. Its message ID and status can be used to follow inclusion and acknowledgment. Retrying the same ID and content within the mailbox's idempotency window returns the original receipt. Reusing the ID with changed content returns an error. An expired retained receipt returns HTTP 410. The queue records its own receipt time.

An authenticated caller can read `native_messages` or use `messages` with the `inbox` action. A thread recipient requires its matching binding. A notification's text contains the event, text and data as JSON. The user-input and finish-decision queues stay separate.
