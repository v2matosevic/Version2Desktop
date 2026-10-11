# App marks

| App | Files | Mark |
|---|---|---|
| Sonora | `sonora/icon.ico`, `icon-256.png`, `icon-1024.png`, `icon-64.rgba` | A folded ribbon wave in coral to warm amber |
| Luma | `luma/…` same set | A teal glass prism with a beam of light passing through it |
| Vesper | `vesper/…` same set | A violet lens ring with a warm glow at its centre |

One family: a single dimensional glass-and-acrylic object per app, transparent background, no
tile, no letters, the same top-left light and contact shadow. Each is meant to be told apart by
shape alone at 16 px on a dark and a light taskbar.

`icon.ico` holds 16, 20, 24, 32, 40, 48, 64, 128 and 256 px for the executable resource, the
installer and shortcuts. `icon-64.rgba` is raw 64×64 RGBA, embedded by `v2_ui::mark_rgba()` for
the title bar, the taskbar and the in-app header mark (no PNG decoder needed at runtime).
`marks-on-dark.png` / `marks-on-light.png` show all three at 256, 48, 32, 24 and 16 px on both
grounds.

## Provenance (2026-10-10)

This set replaces the 2026-10-09 letter-on-tile monograms, which Marko rejected as outdated.

1. Brief: one object per app (Sonora a ribbon waveform, Luma a glass prism with a light beam,
   Vesper an evening object), Fluent-style lighting, about 84 % of the canvas, bold silhouette,
   no letters, tiles, film strips, play triangles or music notes.
2. Each generated with `gpt-image` on Marko's ChatGPT subscription with a transparent background.
   Sonora was kept from its first generation. Luma and Vesper were generated a second time after
   review: the first Luma prism had keyed-out holes and read as a play button, and the first
   Vesper crescent-and-star was replaced by the lens ring.
3. One small transparency hole that background removal left in Luma's prism was filled locally
   as a pixel repair, without restyling. Creative changes to these marks go through GPT Image.
4. Cropped to the object with a small margin and resampled (Lanczos) to every size.

The source generations stayed in the design session's scratchpad; the committed files here are
the deliverables. Status: original artwork released under MIT (see `../v2-ui/NOTICE`).
