# Images in the client terminal

The client shows an image in Activity when a native tool result contains an MCP image block. The original MCP image still reaches the calling harness. Terminal rendering is a separate, local preview; it does not reopen a path or change workspace access.

## Clear pixels where the terminal supports them

The client sends a small Kitty Graphics Protocol query after its raw input pump is ready. It also asks for the terminal's cell dimensions. A successful graphics reply and valid dimensions enable a static PNG preview. This is based on the terminal's reply, not on a claimed `TERM_PROGRAM` value.

ztty's documented static graphics implementation supports the operations used here: direct PTY data, PNG, source rectangles, cell-sized placements, no cursor advance, and scoped deletion. The client implements this small protocol surface itself. It does not embed a Kitty renderer or copy a terminal implementation.

On a terminal that does not answer the query, the client keeps its ordinary colored half-block preview. `NO_COLOR` keeps a plain-text luminance preview. tmux and screen use the fallback in this version; passthrough wrappers and Unicode image placeholders are not implemented.

A direct SSH terminal can use clear images if its actual terminal answers the query. All image bytes travel through the PTY. No shared filesystem, temporary terminal-side file, or shared-memory transport is assumed. A missing or late reply leaves the fallback available.

## Bounds and lifecycle

The original image path goes through the existing workspace authorization and image-envelope checks. An MCP image is limited to **400 KiB**. PNG, JPEG, GIF, and WebP are accepted by the existing image reader.

Before decoding pixels, the preview checks dimensions up to **4096 pixels per side** and a decoded output buffer up to **64 MiB**. The decoder's allocation limit is additional best-effort accounting, not a process-wide memory guarantee.

The client then retains two bounded forms:

| Form | Retained limit | Purpose |
| --- | --- | --- |
| Cell preview | At most 128 × 128 RGBA pixels per retained image | Ordinary terminals and fallback |
| Native preview | A static RGBA8 PNG with each side at most 1024 pixels | Clear terminal display |
| Native PNG cache | At most 12 MiB of encoded PNG data across retained events | Prevent image history from growing without a bound |

Native previews preserve aspect ratio and do not enlarge the source pixels while preparing the cache. Small images are placed at their natural pixel size. Larger images fit the current Activity viewport. GIF and WebP previews show a single decoded frame.

A visible image belongs to one structured Activity event. Scrolling maps the visible rows to a bounded source crop. Folding an event, changing views, or removing an event deletes the client's own placements. Resizing recalculates their rectangles. Disconnect, reload, and interrupted transfer cleanup delete only IDs allocated by this client; they do not clear unrelated terminal images.

PNG encoding and protocol payload construction happen outside the display-state lock. Text layout stays in the shared renderer; Kitty output is emitted before and after its ordinary frame diff. Image error replies disable native display for that session and restore the fallback. Query and image replies are consumed before the composer, while bracketed-paste bytes remain opaque.

## Verification

The Rust image tests cover envelope and decode limits, true-color fallback, tiny images, query replies split across reads, delayed reply fragments, binary bracketed paste, PNG transport chunk boundaries, source cropping, and scoped cleanup. The EventView tests exercise actual decoded images through scrolling, folding, and very narrow panes.

The visual acceptance fixture uses an owned Xvfb and a real ztty window. It should be run with explicitly selected candidate binaries. Its screenshots and pixel checks are separate from the ordinary PTY tests: a VT cell decoder cannot prove that a terminal rendered a native image.

## Protocol references

- [Kitty graphics protocol](https://sw.kovidgoyal.net/kitty/graphics-protocol/) — direct transmission, capability queries, placement geometry, and deletion.
- [ztty protocol scope](https://github.com/LIghtJUNction/ztty/blob/main/docs/protocols.md#kitty-graphics) — the supported static image surface and terminal-side limits.
