# Build and verify

Use Windows x64, Rust 1.94 or newer with the MSVC toolchain, Visual Studio C++ build tools/Windows
SDK and CMake (Luma's embedded AVIF codec). Runtime users need none of these developer tools.

From each `apps/<app>` directory:

```powershell
cargo fmt --package <app> -- --check
cargo clippy --locked --release --all-targets --no-deps -- -D warnings
cargo test --locked --release --all-targets
cargo build --locked --release
```

For Vesper, first run `python scripts/fetch-decoder.py --out apps/vesper/vendor/runtime` from the
repository root. It downloads pinned MSYS2 packages, validates archive/file SHA256 and extracts
only the exact runtime DLLs. Set `VESPER_TEST_DLL` to the absolute vendor/runtime/libmpv-2.dll path
for real decoder tests. Copy those runtime DLLs beside target/release/vesper.exe for normal launching.
Run headless Vesper tests BEFORE the final release build; they can overwrite the same target EXE.

App Cargo.lock files are committed. Do not update dependencies merely to build. Tests use owned
generated fixtures; Windows codec tests can report an explicit skip when that extension is absent.
Device/physical media keys, real input and perceptual checks are separate from CI.
The source release includes build/packaging scripts and exact third-party source provenance.

## Packaging

Install Python 3.11+, 7-Zip and NSIS as developer tools. `scripts/package-release.py`
creates a fresh offline portable payload and per-user installer, including exact locked Rust
notices and MPL source. It refuses an existing stage rather than replacing an older payload.

```powershell
python scripts/package-release.py --app sonora --exe apps/sonora/target/release/sonora.exe --out dist/sonora
python scripts/package-release.py --app luma --exe apps/luma/target/release/luma.exe --out dist/luma
python scripts/package-release.py --app vesper --exe apps/vesper/target/release/vesper.exe --out dist/vesper --decoder apps/vesper/vendor/runtime --decoder-notices decoder-source/licenses
```

Pass `--nsis C:/path/to/makensis.exe` when NSIS is elsewhere. For Vesper extract the matching
release decoder-source asset into `decoder-source` first. Preserve its source archives, build
metadata and notices when redistributing. Packages include documentation and a file/hash manifest.
Unsigned installers have no publisher certificate.

Formatting checks target the first-party app package. Vendored upstream codec formatting is preserved.
