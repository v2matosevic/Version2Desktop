# Formats and practical limits

Containers, codecs and extensions differ. The following is representative coverage, not a promise
that every file carrying an extension will play.

| App | Representative support | Limits |
| --- | --- | --- |
| Sonora | MP3, WAV/PCM, FLAC, AAC/M4A, ALAC, Ogg Vorbis, Opus, AIFF | Rare layouts/profiles conditional; Windows shared-output processing; AAC/M4A padding not universally trimmed |
| Luma | JPEG, PNG, BMP, TIFF, GIF, WebP, animated WebP/APNG, still AVIF | Bounded pixels/frames; large baseline JPEG uses scaled previews; HDR/full color-depth output not promised |
| Luma via Windows | HEIC/HEIF, DNG, JPEG XR, camera RAW, JPEG XL | Installed WIC codecs decide. Synthetic HEIC/DNG/JXR samples checked; all camera RAW models and JXL not verified |
| Vesper | MP4/MOV, MKV, AVI, WebM, TS/M2TS, MPEG, FLV, WMV, OGV, 3GP and representative H.264/HEVC/VP9/AV1 | Profile/hardware dependent; video display framebuffer capped at 4K; no HDR display guarantee |

Luma never installs codecs. HEIC may need Microsoft's HEIF Image Extensions and, for HEVC images,
HEVC Video Extensions; RAW uses available Windows image extensions. Registration alone is not a
successful decode. Missing/failed/unchecked codecs are reported distinctly.

Vesper accepts local files. URL protocols, scripts/external media references, UNC/device/mapped
network and reparse paths are restricted. Subtitles have bounded size/cache; legacy codepages can
need an explicit choice. Local playback remains offline even though browser subtitle search is optional.
