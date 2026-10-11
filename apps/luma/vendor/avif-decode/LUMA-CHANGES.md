# Scoped downstream changes

Upstream: `avif-decode` 1.0.2, BSD-3-Clause, downloaded from crates.io on 2026-10-09.
The registry source checksum is recorded in the app dependency provenance report.

1. `src/lib.rs`: fixes native decoder threads at two instead of available CPU count.
2. `src/lib.rs`: peeks actual primary/alpha AV1 bitstream dimensions before frame
   allocation and rejects zero dimensions, over 8192 per side or over 24,000,000
   pixels. Container metadata is independently checked by Luma.

All original source and copyright/license files are retained. This dependency is
linked statically; it does not add a DLL, Store extension or external executable.
