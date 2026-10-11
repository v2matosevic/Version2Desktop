//! What Windows says about the user's accessibility settings, read once at startup.
//! Both reads are cheap (one syscall, one registry value) and fail soft to the default.

/// `false` when "Animation effects" is off in Settings > Accessibility > Visual effects.
pub fn animations_enabled() -> bool {
    imp::animations_enabled().unwrap_or(true)
}

/// Settings > Accessibility > Text size, as a multiplier (1.0 ..= 2.25).
pub fn text_scale() -> f32 {
    imp::text_scale_percent()
        .map(|p| (p as f32 / 100.0).clamp(1.0, 2.25))
        .unwrap_or(1.0)
}

#[cfg(windows)]
mod imp {
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
    use windows_sys::Win32::UI::WindowsAndMessaging::{SystemParametersInfoW, SPI_GETCLIENTAREAANIMATION};

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub fn animations_enabled() -> Option<bool> {
        let mut on: i32 = 1;
        // SAFETY: SPI_GETCLIENTAREAANIMATION writes one BOOL to the pointer we own.
        let ok = unsafe {
            SystemParametersInfoW(SPI_GETCLIENTAREAANIMATION, 0, (&mut on as *mut i32).cast(), 0)
        };
        (ok != 0).then_some(on != 0)
    }

    pub fn text_scale_percent() -> Option<u32> {
        let key = wide(r"Software\Microsoft\Accessibility");
        let name = wide("TextScaleFactor");
        let mut value: u32 = 0;
        let mut len = std::mem::size_of::<u32>() as u32;
        // SAFETY: the buffers are NUL-terminated UTF-16 we own; RRF_RT_REG_DWORD bounds the write to 4 bytes.
        let status = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                name.as_ptr(),
                RRF_RT_REG_DWORD,
                std::ptr::null_mut(),
                (&mut value as *mut u32).cast(),
                &mut len,
            )
        };
        (status == ERROR_SUCCESS).then_some(value)
    }
}

#[cfg(not(windows))]
mod imp {
    pub fn animations_enabled() -> Option<bool> {
        None
    }
    pub fn text_scale_percent() -> Option<u32> {
        None
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn reads_are_in_range_on_this_machine() {
        let s = super::text_scale();
        assert!((1.0..=2.25).contains(&s), "{s}");
        let _ = super::animations_enabled();
    }
}
