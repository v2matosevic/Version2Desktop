# Release 0.2.1 verification

Verified locally on Windows 11 x64 with Rust 1.94.0. Automated checks are scoped evidence,
not a guarantee for every media file, graphics driver or Windows configuration.

| App | Checks on the exact installed release binary |
| --- | --- |
| Sonora | 14 audio fixtures, five engine programmes, Windows media-session readback, four actual Windows device workloads; FLAC-to-MP3 crossfade had zero measured underruns |
| Luma | 26 built-in positive decodes, four built-in expected rejections, ten Windows-codec positives and four expected rejections, 46 fixture files unchanged, 28 layout captures |
| Vesper | 33 media fixtures including five 4K profiles and three expected rejections; playback/track/subtitle controller checks; native OpenGL video/subtitle readback and complete five-second paused-idle check |

Vesper's refreshed mpv 0.41.0 / FFmpeg 9.0.2 runtime uses 131 pinned MSYS2 DLLs. A reload crash
in a native dependency was reproduced and fixed by holding the selected decoder for the process
lifetime. Absolute paths are normalized before safe-directory DLL loading. Desktop tests
(93 library, two binary, eight real-player regressions) and headless tests (79 library, eight
real-player regressions) passed locally with that exact decoder. Strict release Clippy passed.
Sonora's 110 and Luma's 92 earlier code tests remain relevant to their unchanged functional
source; 0.2.1 changes version/licensing metadata. Fresh Windows CI reruns all app checks.

On this PC the five 4K samples produced changing native GPU-rendered pictures with
D3D11VA-copy. Brief samples prove rendering/codec coverage, not sustained playback of a
feature film. HEIC, synthetic DNG and JPEG XR samples work here; camera RAW models and
JPEG XL are registered but not sampled. Native codec availability on another PC can differ.

All installed payload files match their manifests. Start-menu and uninstall registrations
report 0.2.1. Installers are per-user, optional Open with registration starts off, and no
default-app/UserChoice settings are written. Portable archives passed CRC and every manifest
file/hash check; all Vesper DLLs match the decoder lock.

Not verified: owner-operated drag/drop, physical media keys/clipboard/fullscreen/mixed-DPI,
long listening/A-V sync, perceptual audio/image/video quality, HDR display output, every rare
profile or another GPU. The native computer-control helper was unavailable, so those are
not represented as completed UI checks. The published screenshots show actual app UI with
owned generated demonstration media.
