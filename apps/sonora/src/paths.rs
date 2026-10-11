use std::path::Path;
/// Reject network/device namespaces before any filesystem probe. Extended local drive paths work.
pub fn local(path: &Path) -> Result<(), String> {
    let text = path.as_os_str().to_string_lossy();
    let normalized = text.replace('/', "\\");
    let extended = normalized.strip_prefix("\\\\?\\");
    let local_extended = extended.is_some_and(|p| {
        p.as_bytes().get(1) == Some(&b':')
            && p.as_bytes().get(2) == Some(&b'\\')
            && p.as_bytes()[0].is_ascii_alphabetic()
    });
    if text.contains("://")
        || (normalized.starts_with("\\\\") && !local_extended)
        || normalized.starts_with("\\??\\")
        || text.contains('\0')
    {
        return Err(
            "Sonora opens local filesystem paths only; network and device paths are unsupported"
                .into(),
        );
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn paths_stay_local_and_extended_unicode_paths_work() {
        for p in [
            "https://example.test/song.mp3",
            r"\\server\music.mp3",
            r"\\?\UNC\server\song.wav",
            r"\\.\pipe\music",
            r"\??\NUL",
        ] {
            assert!(local(Path::new(p)).is_err(), "{p}");
        }
        for p in [
            r"C:\Glazba č\song.wav",
            r"\\?\C:\Glazba č\song.wav",
            "song.opus",
        ] {
            assert!(local(Path::new(p)).is_ok(), "{p}");
        }
    }
}
