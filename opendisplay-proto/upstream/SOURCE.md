# Vendored OpenDisplay headers

Byte-for-byte copies, never edited here. `tests/header_sync.rs` compares the
crate's constants and struct sizes against them, so an upstream change fails the
build until it is handled.

| File | Upstream | Commit | SHA-256 |
| --- | --- | --- | --- |
| `opendisplay_protocol.h` (spec 2.2) | https://github.com/OpenDisplay/Firmware `include/` | `6d058329757da73f70d452e16ec663b5e694fe8f` (2026-10-13) | `cde3a72922b932344a0bee97fa8066e08934c2422a55e8c2ea0b74e3b73276ca` |
| `opendisplay_structs.h` (structs 2.1, config 1.5) | same | same | `b8fa88ce08dc2d94c69ea3006133d1701539b9eba560436978a3259ad5da16b9` |

The canonical home named inside the headers is `opendisplay-protocol/src/`. The
Firmware repository is GPL-3.0, and this project is GPL-3.0-or-later (`LICENSE`
at the root), so vendoring the headers, and reusing upstream code where useful,
is compatible.

To update: copy both files from a newer upstream commit, record it here, and run
`./build.sh test`.
