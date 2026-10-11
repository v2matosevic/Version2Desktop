# Licenses and decoder provenance

MIT applies to first-party app/shared UI code and original artwork. It does not replace licenses
on vendored/dependency code, operating-system components or codec libraries. No Windows fonts
are redistributed. Preserve the notices supplied with each package.

Sonora includes MIT/BSD and MPL components with their required notices and MPL source.
Luma includes its BSD AVIF decoder sources/notices; optional Windows image codecs are not bundled.

Vesper's Windows package uses native MSYS2 UCRT64 libmpv/FFmpeg and their dependency DLLs. The
binary combination is distributed under GPL-3.0-or-later, with first-party MIT source retained.
The precise binary archives, import closure, SHA256, package build metadata and source-only archives
are recorded in decoder-packages.lock.json. Corresponding decoder source archives/build recipes
are available as the release's decoder-source asset; they are not an extra runtime dependency.

Sources: MSYS2 package/source repositories, upstream mpv and FFmpeg. Versioned producer archives
replace an earlier private decoder whose complete source provenance was unresolved. Rebuilding or
changing dependencies requires refreshing both runtime and corresponding-source/notices records.
