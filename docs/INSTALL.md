# Install on Windows

1. Open the latest GitHub release and download the installer for Sonora, Luma or Vesper.
2. Run it and choose the install folder. Installation is per user and needs no administrator account.
3. Open the app from Start > Version2. Use Open or drag a local media file onto the window.

The complete app is in its own installer. Vesper's decoder DLLs are included; do not download an
extra codec runtime separately. The default folder is `%LOCALAPPDATA%/Programs/Version2/<App>`.
Windows file defaults are preserved. To make an app your default, choose it yourself in Windows
Settings > Apps > Default apps. Each installer can optionally offer the app in Windows Open with; that checkbox is off by default.

## Portable use

Download that app's portable ZIP, extract it into a fresh folder and run its EXE. Keep every Vesper
DLL beside Vesper.exe. Do not run an EXE directly inside the ZIP. Source archives are for developers,
not an extra runtime requirement.

## Updates and removal

Keep your media in your own folders, separate from the installation. Install a newer release in the
same application folder after you close that application yourself. Do not replace files while it runs.
Uninstall through Windows Settings > Apps > Installed apps. App preferences/caches may remain in
`%LOCALAPPDATA%/Version2`; your original media is not modified by viewing/playback.

These builds are not code-signed. Verify the release's SHA256SUMS.txt against the downloaded file
with PowerShell `Get-FileHash -Algorithm SHA256 <file>`. Do not bypass security warnings if the
download's origin or checksum does not match. Conditional Windows image codecs are explained in FORMATS.md.
