# Prophet attachment issues

This records attachment and response-delivery problems observed during the Sigla prototype work through 2026-10-28 08:19 UTC. The evidence is the current session transcript and the saved responses and patches under `/tmp`. The timestamps below are UTC.

Ten responses reported **26 failed attachment downloads**. In five responses, a complete patch was still received separately through Magic Wormhole; the failed attachments were individual source files or test logs. In the other five responses, the failed attachment was the advertised patch. One of those responses included the patch inline; the other four required a retry to obtain reviewable code or remained pending when work paused.

The warnings generally gave only `Could not download attachment '<name>'`. They did not state the underlying failure, so they do not establish whether the cause was the producer, attachment storage, the tool, or the network.

## Complete patches received despite failed individual downloads

| Response time | Prototype | Attachments that could not be downloaded | Outcome |
|---|---|---|---|
| 2026-10-27 23:18 | History file relationships | `mod.rs`, `render.rs`, `matching.rs`, `history.rs` | The complete `sigla-history-file-relationships.patch` arrived through Wormhole. It was reviewed, changed, tested, and committed as `3520f41`. |
| 2026-10-28 01:13 | Unsupported text history | `matching.rs`, `regions.rs`, `mod.rs`, `README.md` | The complete `sigla-history-text.patch` arrived through Wormhole. Review found correctness and resource problems; the patch was saved rather than integrated. |
| 2026-10-28 03:20 | Unusual Git filenames | `history.rs`, `selection.rs`, `matching.rs`, `names.rs`, `history-unit-tests-current.log`, `history-selection-tests-current.log`, `history-integration-tests-current.log` | The complete `history-git-filenames.patch` arrived through Wormhole. The missing logs could not substantiate the producer's test claims; local tests and review were run. The patch was saved because review found regressions. |
| 2026-10-28 04:05 | Binary content in history | `matching.rs`, `regions.rs`, `mod.rs`, `history.rs` | The complete `sigla-binary-history.patch` arrived through Wormhole. A large-file regression prevented integration. |
| 2026-10-28 06:38 | History candidate cache | `matching.rs`, `cache.rs` | The complete `sigla-selected-history-candidate-memo.patch` and its benchmark document arrived through Wormhole. The patch was reviewed, bounded further, tested, and committed as `e716a9b`. |

These warnings concerned the individual downloads. They do not mean the separately received patches were corrupt. Patch applicability and local tests were checked independently where the work proceeded.

## Advertised patch unavailable

| Response time | Prototype | Failure and outcome |
|---|---|---|
| 2026-10-28 03:32 | Literal history filenames | `sigla-literal-history-file.patch` could not be downloaded. The response included an inline patch, which was reviewed and integrated as `3bb388b`. |
| 2026-10-28 04:22 | Path-scoped attribute exclusions | `path_scope_exclusions.patch` could not be downloaded; the response described a fix but supplied no usable code. A single retry supplied a complete patch through Wormhole. That patch was saved as `/tmp/sigla-path-history-attributes.patch`, but review found false omissions and a work-limit regression. |
| 2026-10-28 05:25 | C# `allows ref struct` history | `sigla-ref-struct-history.patch` could not be downloaded; the response contained no usable patch. A single retry supplied a complete patch through Wormhole. It was saved as `/tmp/sigla-modern-constraints.patch`; grammar ordering and resource concerns prevented integration. |
| 2026-10-28 07:37 | C# generic alias references | `sigla-generic-alias-uses.patch` could not be downloaded. The response also said its Wormhole transport was unavailable. A single retry delivered `/tmp/sigla-csharp-generic-alias.patch` through normal Wormhole file reception; its SHA-256 matched the supplied value. Review was underway when the goal paused. |
| 2026-10-28 08:17 | Local symlink view | `sigla-local-symlink-view.patch` could not be downloaded; only a description was available. A single retry was submitted before the goal paused. After the turn was interrupted, its execution handle was no longer available and no result was saved. The remote request's final outcome is unknown. |

The generic-alias retry proposed both a text-mode/base64 transfer and a normal file-receive command. The normal file receive succeeded; the text-mode alternative was not used. A response should provide a directly usable file attachment or standard file-receive command rather than require an encoding workaround.

## Other delivery and packaging issues

- The first unsupported-text prototype request returned `ERROR: The existing conversation is no longer available for recovery.` It produced no usable response or patch. The permitted retry returned the complete patch, along with the individual-download warnings listed above.
- The locally saved reviewed diff for unreadable history candidates omitted the new untracked `src/history/regions/unavailable.rs`. Reconstructing that reviewed prototype requires both `/tmp/sigla-history-unreadable-reviewed.patch` and the original `/tmp/sigla-history-unreadable-candidates.patch`. This was a local patch-packaging issue, not an established Prophet download failure.
- Attempts to report several of these problems using `m-notify --warning` failed with `c: session is unavailable`. This is a separate notification-session failure; it does not identify the cause of the attachment failures.

## Evidence and limits

- The session transcript is `/home/dev/.codex/sessions/2026/10/27/rollout-2026-10-27T20-35-38-01a17edc-6faa-79d3-bb57-330e08b92b66.jsonl`.
- Saved summaries include `/tmp/sigla-history-attribute-unusable-response.md`, `/tmp/sigla-history-csharp-constraints-unusable-response.md`, `/tmp/sigla-generic-alias-unavailable-response.md`, `/tmp/sigla-symlink-view-unavailable-response.md`, and `/tmp/sigla-history-prototype-state.txt`.
- `/tmp` files are temporary. Wormhole codes are intentionally omitted: they are single-use and may already be consumed or expired.
- This inventory describes failures visible in the transcript and saved artifacts; it does not claim a diagnosis of the underlying attachment service.
