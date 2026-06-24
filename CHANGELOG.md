# Changelog

## v0.1.4
- **Security — path-traversal sandbox** — file paths in peer messages are now
  validated lexically (`routing::is_safe_rel`); a compromised mesh peer can no
  longer read/write/delete files outside the shared folder via absolute paths or
  `..` segments.
- **Security — incoming frame size cap** — `protocol::MAX_MSG_SIZE` (1 GiB) rejects
  oversized frames before allocation, blocking a per-frame memory-bomb DoS.
- No wire-format change; a mixed mesh (older nodes) stays compatible.

## v0.1.3
- **Real-time peer tracking** — application heartbeat (4s ping / 12s timeout) reaps
  dead/half-open peers quickly; peer list & availability stay accurate.
- **Shared change history** — who/when/what is synced across all nodes (one common
  audit trail); browse via the GUI **History** window.
- **Sync all** — download a whole folder with a progress window; cancellable;
  skips files whose holder is offline; survives a holder dropping mid-download;
  re-running resumes what's missing.
- **Available column** — shows whether a file is fetchable (a holder is online).
- **Origin + holders** — Location shows the file's creator plus a live holder list.
- **Offline-edit detection** — files changed while minisync was off are causally
  tracked (concurrent offline edits → conflict copy, not silent overwrite).
- **Fixes** — deterministic dedup for simultaneous peer connections; Unicode **NFC**
  path normalization (Korean/CJC filenames; collapses NFD/NFC catalog duplicates);
  CJK font loading in the GUI; reliable download fallback; conflict notifications.

## v0.1.2
- Packaged installers: macOS **.dmg** (minisync.app), Windows **.zip**, Linux **.tar.gz**.

## v0.1.1
- Cross-platform pre-built binaries: macOS · Windows · Linux (all with GUI).

## v0.1.0
- Initial release — P2P full-mesh folder sync, TLS, CRDT text merge, selective sync.
