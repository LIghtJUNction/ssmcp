# Images and guide links in the terminal

Activity can display an MCP image block returned by a native tool. The preview consumes that authorized block locally; it does not reopen a source path or change the tool result delivered to the calling harness. The implementation is in [the image decoder](../src/client/terminal/images.rs) and [the Activity view](../src/client/terminal/events.rs).

## Native display and fallback

The client queries Kitty graphics support after the raw input pump is ready. Native placement requires a successful reply and valid terminal cell dimensions. A terminal name such as `TERM_PROGRAM` is insufficient evidence of support. A missing, late or failed reply keeps the fallback available; an image error disables native display for the session. See [capability detection and placement](../src/client/terminal/images/kitty.rs).

Image data travels through the PTY using the [Kitty graphics protocol](https://sw.kovidgoyal.net/kitty/graphics-protocol/). This permits a direct SSH connection without a shared filesystem or terminal-side temporary image files. The implementation uses direct PNG transmission, bounded source crops and deletion of its own placements.

Ordinary terminals use a colored half-block preview. `NO_COLOR` uses a plain-text luminance preview. Activity labels this fallback **Low-resolution thumbnail** because it cannot preserve small text in a screenshot. tmux and screen use the fallback in this version; passthrough wrappers and Unicode image placeholders are not implemented. These choices are made by [the graphics adapter](../src/client/terminal/images/kitty.rs) and [the cell renderer](../src/client/terminal/images.rs).

## Limits and image ownership

The native image envelope, decoding buffer and retained preview have separate limits. Accepting a larger local attachment does not increase the size of an MCP image block.

| Stage | Limit | Source |
| --- | --- | --- |
| Native MCP image bytes | 400 KiB per image block | [output validation](../src/output.rs) |
| Preview dimensions | 4096 pixels per side | [image decoder](../src/client/terminal/images.rs) |
| Decoded preview output | 64 MiB before pixel allocation | [image decoder](../src/client/terminal/images.rs) |
| Retained cell preview | At most 128 × 128 RGBA pixels | [image decoder](../src/client/terminal/images.rs) |
| Retained native preview | Static PNG, original pixels up to 4096 per side when the encoded PNG fits 4 MiB + 64 KiB; otherwise one resize to at most 1024 per side | [native image preparation](../src/client/terminal/images/kitty.rs) |
| Encoded native payloads in Activity | 12 MiB across retained events | [Activity retention](../src/client/terminal/events.rs) |

Decoder allocation accounting is best effort in addition to the explicit dimension and output-buffer checks. Native preparation first streams the original pixels into a lossless PNG with a bounded output buffer; it does not collect a second whole-image compressed buffer. If that exceeds the existing per-image payload budget, it makes one attempt at a reduced size; a further failure keeps only the cell thumbnail. Activity shows both preview and source dimensions when they differ. GIF and WebP previews use one decoded frame. See [decoding](../src/client/terminal/images.rs) and [native image preparation](../src/client/terminal/images/kitty.rs).

Native images fit the Activity pane's width without enlarging source pixels. Tall screenshots retain their proportional height in the transcript instead of shrinking into an 18-row box. When a new tall image arrives while following output, Activity starts at its heading and pauses automatic following so later tool output cannot pull it away. Scroll to inspect the image; **Ctrl+End** returns to the latest output and resumes following. The compositor crops only the visible source rows and reuses the uploaded pixels. The cell fallback uses the available pane height while keeping its 128 × 128 pixel storage limit. See [Activity layout](../src/client/terminal/events.rs) and [placement geometry](../src/client/terminal/images/kitty.rs).

Each Activity image belongs to its retained event. Scrolling changes the source crop, and folding, clearing or leaving the view removes the associated placement. Resizing recalculates placement geometry. Reload and interrupted-transfer cleanup delete IDs owned by this client. PNG preparation occurs outside the display-state lock, while the compositor coordinates image operations with the normal text-frame update. See [event ownership](../src/client/terminal/events.rs) and [the graphics compositor](../src/client/terminal/images/kitty.rs).

## Pasting a PNG into the composer

The dashboard examines raw bracketed-paste bytes before UTF-8 decoding. PNG chunk lengths and the final `IEND` chunk determine when an outer paste-end marker is allowed. An identical marker, Ctrl-C, Ctrl-] or F7 inside a PNG chunk remains image data. The importer then validates the image and inserts a private, editable `@reference`. See [PNG framing](../src/client/input/paste.rs), [composer input](../src/client/terminal/conversation.rs) and [attachment import](../src/client/attachments.rs).

Image input is bounded at 16 MiB; text paste retains its 4 MiB limit. The importer may resize or re-encode a larger image to fit the 400 KiB native envelope. Invalid binary data, altered image signatures and image-import failures leave the existing draft intact. These are separate input and delivery limits, defined in [attachment import](../src/client/attachments.rs) and [text-reference storage](../src/client/references/paste.rs).

An unfinished image paste expires after two seconds without actual input progress. This includes a complete PNG whose outer paste-end marker never arrives. The decoder, graphics filter and composer share progress and recovery boundaries; redraw ticks do not extend the deadline. Recovery drops the pending paste and preserves the draft. In session attach, a writer's subsequent real Ctrl-C reaches the inner PTY, while an observer's Ctrl-C detaches. See [attach input](../src/session/attach/input.rs) and [attach routing](../src/session/attach.rs).

Headless attach keeps its existing UTF-8 text protocol. Opaque paste events bypass the local F7 parser so embedded bytes cannot select another session terminal; this path does not provide binary image import. Use the dashboard composer for the image-reference workflow.

## Guide URL and QR code

Each live session makes one anonymous request to the gateway's same-origin `/guide/` route with a two-second budget. A successful response selects that guide URL. A missing route, other unsuccessful response, network error or timeout selects the same-origin portal root instead. The probe does not follow redirects or send session credentials. Static UI previews do not make this network request. See [guide resolution](../src/client/terminal/guide.rs).

The sidebar keeps the complete selected URL available. A QR image is prepared only after graphics support and cell dimensions are confirmed and the entire code can fit. PNG generation runs lazily in a background task; startup does not pre-encode it. The cached image uses square 8-pixel modules with a four-module white quiet zone. It is placed at natural pixel size without cropping or scaling. An unsupported terminal or insufficient space keeps the URL fallback. See [guide rendering](../src/client/terminal/guide.rs) and [native QR preparation](../src/client/terminal/guide/native.rs).

A working local candidate route does not establish that the public gateway has been deployed. Check the public gateway independently before reporting its `/guide/` link as available.

## Verification

Run the fixtures with an explicitly selected candidate directory. The PTY fixture checks paste handling and the emitted graphics protocol:

```sh
python3 scripts/e2e_file_paste.py \
  --bin-dir /path/to/candidate-bins \
  --out out/file-paste
```

A VT cell decoder cannot verify native image pixels. The real-terminal fixture uses a private Xvfb display and a ztty window to capture and inspect actual rendered images:

```sh
python3 scripts/e2e_native_images.py \
  --bin-dir /path/to/candidate-bins \
  --ztty /path/to/ztty \
  --out out/native-images
```

The guide fixture checks the candidate gateway's English and Chinese routes, renders those pages in private headless Chromium, and decodes the native sidebar QR with zbar:

```sh
python3 scripts/e2e_guide.py \
  --bin-dir /path/to/candidate-bins \
  --ztty /path/to/ztty \
  --out out/guide
```

The visual fixtures require Linux, Xvfb, Pillow and python-Xlib; the guide fixture additionally needs Chromium and zbar. Record the selected binary hashes with each JSON result and screenshot. These commands describe the verification procedure; the completion report records which candidate actually passed.

## References

- [Kitty graphics protocol](https://sw.kovidgoyal.net/kitty/graphics-protocol/) specifies transmission, capability queries, placement and deletion.
- [ztty graphics support](https://github.com/LIghtJUNction/ztty/blob/main/docs/protocols.md#kitty-graphics) documents the terminal implementation used by the visual fixture.
- [Client input](client-input.md) describes composer references and local input commands.
