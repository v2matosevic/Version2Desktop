//! Owner-triggered subtitle search.
//!
//! Vesper never contacts a subtitle provider. This module turns text the owner can see and edit
//! into a search address and hands it to Windows to open in the owner's own browser, and only when
//! the owner presses Search. There is no request, upload, account, key, scraping or download here:
//! the provider APIs need a registered application key and, past a small anonymous allowance, a
//! signed-in account (OpenSubtitles documents five anonymous downloads per IP per 24 hours and
//! forbids asking users for their own keys), so the honest scope is "open the provider's search
//! page", and the owner downloads the file there and drops it onto the window.
use serde::{Deserialize, Serialize};
use std::path::Path;

/// A subtitle language as the providers know it, plus what the decoder needs to prefer it.
pub struct Language {
    /// Language id used by opensubtitles.org's `sublanguageid-` filter (read from its search form).
    pub id: &'static str,
    pub name: &'static str,
    /// Codes libmpv matches against track language tags (`--slang`).
    pub tags: &'static str,
    /// The Windows code page older subtitle files in this language are usually saved in. The
    /// decoder build cannot convert them itself, so Vesper does (see `subtext`).
    pub codepage: Option<u32>,
}

const fn lang(
    id: &'static str,
    name: &'static str,
    tags: &'static str,
    codepage: Option<u32>,
) -> Language {
    Language {
        id,
        name,
        tags,
        codepage,
    }
}

/// Ids are the `sublanguageid` values on opensubtitles.org's own search form (checked 2026-10-11).
pub const LANGUAGES: &[Language] = &[
    lang("hrv", "Croatian", "hr,hrv", Some(1250)),
    lang("eng", "English", "en,eng", Some(1252)),
    lang("bos", "Bosnian", "bs,bos", Some(1250)),
    lang("scc", "Serbian", "sr,srp,scc", Some(1250)),
    lang("slv", "Slovenian", "sl,slv", Some(1250)),
    lang("mac", "Macedonian", "mk,mkd,mac", Some(1251)),
    lang("alb", "Albanian", "sq,sqi,alb", Some(1250)),
    lang("ger", "German", "de,deu,ger", Some(1252)),
    lang("fre", "French", "fr,fra,fre", Some(1252)),
    lang("spa", "Spanish", "es,spa", Some(1252)),
    lang("ita", "Italian", "it,ita", Some(1252)),
    lang("por", "Portuguese", "pt,por", Some(1252)),
    lang("pob", "Portuguese (Brazil)", "pt-BR,pob,pt,por", Some(1252)),
    lang("dut", "Dutch", "nl,nld,dut", Some(1252)),
    lang("pol", "Polish", "pl,pol", Some(1250)),
    lang("cze", "Czech", "cs,ces,cze", Some(1250)),
    lang("slo", "Slovak", "sk,slk,slo", Some(1250)),
    lang("hun", "Hungarian", "hu,hun", Some(1250)),
    lang("rum", "Romanian", "ro,ron,rum", Some(1250)),
    lang("bul", "Bulgarian", "bg,bul", Some(1251)),
    lang("ell", "Greek", "el,ell,gre", Some(1253)),
    lang("tur", "Turkish", "tr,tur", Some(1254)),
    lang("rus", "Russian", "ru,rus", Some(1251)),
    lang("ukr", "Ukrainian", "uk,ukr", Some(1251)),
    lang("swe", "Swedish", "sv,swe", Some(1252)),
    lang("nor", "Norwegian", "no,nor,nb", Some(1252)),
    lang("dan", "Danish", "da,dan", Some(1252)),
    lang("fin", "Finnish", "fi,fin", Some(1252)),
    lang("ice", "Icelandic", "is,isl,ice", Some(1252)),
    lang("heb", "Hebrew", "he,heb", Some(1255)),
    lang("ara", "Arabic", "ar,ara", Some(1256)),
    lang("per", "Persian", "fa,fas,per", Some(1256)),
    lang("hin", "Hindi", "hi,hin", None),
    lang("chi", "Chinese (simplified)", "zh,zho,chi", Some(936)),
    lang("jpn", "Japanese", "ja,jpn", Some(932)),
    lang("kor", "Korean", "ko,kor", Some(949)),
    lang("tha", "Thai", "th,tha", Some(874)),
    lang("vie", "Vietnamese", "vi,vie", Some(1258)),
    lang("ind", "Indonesian", "id,ind", Some(1252)),
];

pub fn language(id: &str) -> Option<&'static Language> {
    LANGUAGES.iter().find(|l| l.id == id)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Provider {
    OpenSubtitles,
    OpenSubtitlesOrg,
    Titlovi,
}

impl Provider {
    pub const ALL: [Provider; 3] = [
        Provider::OpenSubtitles,
        Provider::OpenSubtitlesOrg,
        Provider::Titlovi,
    ];
    pub fn name(self) -> &'static str {
        match self {
            Provider::OpenSubtitles => "OpenSubtitles.com",
            Provider::OpenSubtitlesOrg => "OpenSubtitles.org",
            Provider::Titlovi => "Titlovi.com",
        }
    }
    pub fn host(self) -> &'static str {
        match self {
            Provider::OpenSubtitles => "www.opensubtitles.com",
            Provider::OpenSubtitlesOrg => "www.opensubtitles.org",
            Provider::Titlovi => "titlovi.com",
        }
    }
    /// What the search page does with the chosen language, so the owner is not surprised.
    pub fn language_note(self) -> &'static str {
        match self {
            Provider::OpenSubtitles => "The site uses the language set on it; change it there.",
            Provider::OpenSubtitlesOrg => "Filtered to the language you chose.",
            Provider::Titlovi => "Shows Balkan languages together; pick one on the page.",
        }
    }
    pub fn applies_language(self) -> bool {
        matches!(self, Provider::OpenSubtitlesOrg)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum SearchError {
    EmptyQuery,
    /// A press is still being handed to Windows; one action at a time.
    Busy,
    Opener(String),
}

impl std::fmt::Display for SearchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SearchError::EmptyQuery => f.write_str("Type a movie or show title to search for."),
            SearchError::Busy => f.write_str("Still opening your browser."),
            SearchError::Opener(e) => write!(f, "Windows could not open the browser: {e}"),
        }
    }
}

const MAX_QUERY_CHARS: usize = 120;

/// What actually goes into the address: trimmed, whitespace collapsed, controls and path
/// separators removed, bounded. Never derived from anything but the text shown to the owner.
pub fn clean_query(text: &str) -> String {
    let mut out = String::new();
    for word in text
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '/' | '\\' | '?' | '#' | '%') {
                ' '
            } else {
                c
            }
        })
        .collect::<String>()
        .split_whitespace()
    {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
        if out.chars().count() >= MAX_QUERY_CHARS {
            break;
        }
    }
    out.chars().take(MAX_QUERY_CHARS).collect()
}

/// RFC 3986 percent-encoding of UTF-8: only unreserved characters stay as they are.
pub fn percent_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len() * 3);
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// The search address for a provider. `language_id` is a [`Language::id`] or `None` for any.
pub fn search_url(
    provider: Provider,
    query: &str,
    language_id: Option<&str>,
) -> Result<String, SearchError> {
    let query = clean_query(query);
    if query.is_empty() {
        return Err(SearchError::EmptyQuery);
    }
    let q = percent_encode(&query);
    Ok(match provider {
        Provider::OpenSubtitles => format!(
            "https://www.opensubtitles.com/en/search-all/q-{q}/hearing_impaired-include/machine_translated-/trusted_sources-"
        ),
        Provider::OpenSubtitlesOrg => {
            let id = language_id.and_then(language).map_or("all", |l| l.id);
            format!("https://www.opensubtitles.org/en/search2/sublanguageid-{id}/moviename-{q}")
        }
        Provider::Titlovi => format!("https://titlovi.com/titlovi/?prijevod={q}"),
    })
}

/// A guess at the title from a file name, for the owner to confirm or edit. Only the file name
/// is read: no folder name, drive or path leaves this function.
pub fn suggest_query(file: &Path) -> String {
    let Some(stem) = file.file_stem().and_then(|s| s.to_str()) else {
        return String::new();
    };
    // Drop bracketed release tags, then split on the separators release names use.
    let mut flat = String::new();
    let mut depth = 0_u32;
    for c in stem.chars() {
        match c {
            '[' | '(' | '{' => depth += 1,
            ']' | ')' | '}' => depth = depth.saturating_sub(1),
            _ if depth > 0 => {}
            '.' | '_' => flat.push(' '),
            _ => flat.push(c),
        }
    }
    let tokens: Vec<&str> = flat.split_whitespace().collect();
    let mut title: Vec<&str> = Vec::new();
    for (i, token) in tokens.iter().enumerate() {
        if i > 0 && is_release_tag(token) {
            if is_episode(token) {
                title.push(token);
            }
            break;
        }
        title.push(token);
    }
    // A trailing year is the release year, not part of the title, unless it is the only word.
    if title.len() > 1 && title.last().is_some_and(|t| is_year(t)) {
        title.pop();
    }
    clean_query(&title.join(" "))
}

fn is_year(token: &str) -> bool {
    token.len() == 4
        && token
            .parse::<u16>()
            .is_ok_and(|y| (1900..=2099).contains(&y))
}

fn is_episode(token: &str) -> bool {
    let t = token.to_ascii_lowercase();
    let bytes = t.as_bytes();
    // S01E02 or 1x02
    if bytes.first() == Some(&b's') {
        if let Some(e) = t.find('e') {
            return t[1..e].chars().all(|c| c.is_ascii_digit())
                && e > 1
                && t[e + 1..].chars().all(|c| c.is_ascii_digit())
                && t.len() > e + 1;
        }
        return false;
    }
    false
}

fn is_release_tag(token: &str) -> bool {
    let t = token.to_ascii_lowercase();
    if is_episode(token) {
        return true;
    }
    if is_year(token) {
        // A year ends the title only when more release information follows; the caller drops it.
        return false;
    }
    if t.len() >= 3
        && t.ends_with(['p', 'i'])
        && t[..t.len() - 1].chars().all(|c| c.is_ascii_digit())
        && t[..t.len() - 1].len() >= 3
    {
        return true; // 720p 1080p 2160p 1080i
    }
    matches!(
        t.as_str(),
        "4k" | "uhd"
            | "hdr"
            | "hdr10"
            | "hdr10+"
            | "dv"
            | "dolby"
            | "bluray"
            | "blu-ray"
            | "bdrip"
            | "brrip"
            | "bdremux"
            | "remux"
            | "webrip"
            | "web-dl"
            | "webdl"
            | "web"
            | "hdtv"
            | "dvdrip"
            | "dvd"
            | "hdrip"
            | "cam"
            | "hdcam"
            | "x264"
            | "x265"
            | "h264"
            | "h265"
            | "h.264"
            | "h.265"
            | "hevc"
            | "avc"
            | "av1"
            | "xvid"
            | "divx"
            | "aac"
            | "ac3"
            | "eac3"
            | "dts"
            | "dts-hd"
            | "ddp5"
            | "dd5"
            | "atmos"
            | "10bit"
            | "extended"
            | "unrated"
            | "remastered"
            | "proper"
            | "repack"
            | "internal"
            | "multi"
            | "dual"
            | "subbed"
            | "dubbed"
    )
}

fn allowed_https(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    Provider::ALL.iter().any(|p| p.host() == host)
        && url
            .bytes()
            .all(|b| b.is_ascii_graphic() && b != b'"' && b != b'<' && b != b'>' && b != b'`')
}

/// Hand an allow-listed https address to Windows, which opens the default browser. This is the
/// only place a search leaves the app, and it is reached only from [`SearchForm::submit`].
#[cfg(windows)]
pub fn open_in_browser(url: &str) -> Result<(), String> {
    use std::ffi::c_void;
    if !allowed_https(url) {
        return Err("Only the listed subtitle sites can be opened.".into());
    }
    #[link(name = "shell32")]
    unsafe extern "system" {
        fn ShellExecuteW(
            hwnd: *mut c_void,
            operation: *const u16,
            file: *const u16,
            parameters: *const u16,
            directory: *const u16,
            show: i32,
        ) -> isize;
    }
    let wide = |s: &str| -> Vec<u16> { s.encode_utf16().chain(std::iter::once(0)).collect() };
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            wide("open").as_ptr(),
            wide(url).as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1,
        )
    };
    if result > 32 {
        Ok(())
    } else {
        Err(format!("ShellExecute returned {result}"))
    }
}

#[cfg(not(windows))]
pub fn open_in_browser(_url: &str) -> Result<(), String> {
    Err("Opening a browser is only implemented on Windows.".into())
}

/// The search sheet's state. Constructing, editing or showing it does nothing; only
/// [`SearchForm::submit`] calls the opener, and only once per press.
pub struct SearchForm {
    pub query: String,
    pub language: Option<String>,
    pub provider: Provider,
    /// Last outcome, shown under the buttons.
    pub status: Option<Result<String, String>>,
    pub last_url: Option<String>,
    /// An address has been handed to the opener and its answer has not come back.
    pub pending: bool,
}

impl SearchForm {
    pub fn new(query: String, language: Option<String>, provider: Provider) -> Self {
        Self {
            query,
            language,
            provider,
            status: None,
            last_url: None,
            pending: false,
        }
    }
    pub fn url(&self) -> Result<String, SearchError> {
        search_url(self.provider, &self.query, self.language.as_deref())
    }
    /// The press: validate, mark one action pending and return the address to open. Does no
    /// I/O itself, so the caller can open it on a worker and keep the window responsive.
    pub fn begin(&mut self) -> Result<String, SearchError> {
        if self.pending {
            return Err(SearchError::Busy);
        }
        match self.url() {
            Ok(url) => {
                self.pending = true;
                self.status = Some(Ok(format!(
                    "Opening {} in your browser…",
                    self.provider.name()
                )));
                Ok(url)
            }
            Err(e) => {
                self.status = Some(Err(e.to_string()));
                Err(e)
            }
        }
    }
    /// The opener's answer for the address `begin` returned.
    pub fn finished(&mut self, url: &str, result: Result<(), String>) {
        self.pending = false;
        match result {
            Ok(()) => {
                self.status = Some(Ok(format!(
                    "Opened {} in your browser.",
                    self.provider.name()
                )));
                self.last_url = Some(url.to_string());
            }
            Err(e) => {
                self.status = Some(Err(SearchError::Opener(e).to_string()));
            }
        }
    }
    /// `begin`, the opener and `finished` in one step, for callers that may block.
    pub fn submit(
        &mut self,
        open: &mut dyn FnMut(&str) -> Result<(), String>,
    ) -> Result<(), SearchError> {
        let url = self.begin()?;
        let result = open(&url);
        let failure = result.clone().err();
        self.finished(&url, result);
        failure.map_or(Ok(()), |e| Err(SearchError::Opener(e)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn percent_encoding_keeps_only_unreserved_characters() {
        assert_eq!(percent_encode("Blade Runner 2049"), "Blade%20Runner%202049");
        assert_eq!(percent_encode("a&b=c?d#e/f"), "a%26b%3Dc%3Fd%23e%2Ff");
        assert_eq!(
            percent_encode("č ć š ž đ"),
            "%C4%8D%20%C4%87%20%C5%A1%20%C5%BE%20%C4%91"
        );
        assert_eq!(percent_encode("日本"), "%E6%97%A5%E6%9C%AC");
        assert_eq!(percent_encode("A-b_c.d~e"), "A-b_c.d~e");
        assert_eq!(percent_encode("100%"), "100%25");
        assert_eq!(percent_encode("\"><script>"), "%22%3E%3Cscript%3E");
    }

    #[test]
    fn urls_match_the_shapes_verified_on_each_site() {
        let q = "Blade Runner 2049";
        assert_eq!(
            search_url(Provider::OpenSubtitles, q, Some("hrv")).unwrap(),
            "https://www.opensubtitles.com/en/search-all/q-Blade%20Runner%202049/hearing_impaired-include/machine_translated-/trusted_sources-"
        );
        assert_eq!(
            search_url(Provider::OpenSubtitlesOrg, q, Some("hrv")).unwrap(),
            "https://www.opensubtitles.org/en/search2/sublanguageid-hrv/moviename-Blade%20Runner%202049"
        );
        assert_eq!(
            search_url(Provider::OpenSubtitlesOrg, q, None).unwrap(),
            "https://www.opensubtitles.org/en/search2/sublanguageid-all/moviename-Blade%20Runner%202049"
        );
        assert_eq!(
            search_url(Provider::Titlovi, q, Some("hrv")).unwrap(),
            "https://titlovi.com/titlovi/?prijevod=Blade%20Runner%202049"
        );
    }

    #[test]
    fn every_url_stays_on_its_provider_host_whatever_the_text() {
        let hostile = [
            "evil.example/x",
            "a b\\c/d",
            "https://attacker.test/?q=1#frag",
            "x\r\nHost: evil",
            "\"; calc.exe; \"",
            "%2F..%2F",
        ];
        for provider in Provider::ALL {
            for text in hostile {
                let url = search_url(provider, text, Some("hrv")).unwrap();
                assert!(allowed_https(&url), "{url}");
                assert!(!url.contains(' ') && !url.contains('\n') && !url.contains('"'));
                // Whatever the owner typed appears only as percent-encoded unreserved text.
                let encoded = percent_encode(&clean_query(text));
                assert!(url.contains(&encoded), "{url} must carry {encoded}");
                assert!(
                    encoded
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"-._~%".contains(&b))
                );
            }
        }
        assert!(!allowed_https("http://www.opensubtitles.org/x"));
        assert!(!allowed_https("https://evil.test/www.opensubtitles.org"));
        assert!(!allowed_https("https://www.opensubtitles.org.evil.test/x"));
        assert!(!allowed_https("file:///C:/Windows/System32/calc.exe"));
        assert!(!allowed_https("https://www.opensubtitles.org/a b"));
    }

    #[test]
    fn empty_or_unusable_text_is_refused_without_a_url() {
        for text in ["", "   ", "\t\n", "///", "???"] {
            assert_eq!(
                search_url(Provider::OpenSubtitlesOrg, text, None),
                Err(SearchError::EmptyQuery),
                "{text:?}"
            );
        }
    }

    #[test]
    fn the_query_is_bounded() {
        let long = "word ".repeat(200);
        assert!(clean_query(&long).chars().count() <= MAX_QUERY_CHARS);
    }

    #[test]
    fn suggestions_use_only_the_file_name_and_strip_release_noise() {
        let cases = [
            (
                "B:\\Private Client Folder\\Movies\\Blade.Runner.2049.2017.1080p.BluRay.x264-GRP.mkv",
                "Blade Runner 2049",
            ),
            (
                "D:\\x\\Dune.2021.2160p.WEB-DL.DDP5.1.Atmos.HEVC.mkv",
                "Dune",
            ),
            (
                "C:\\a\\Show.Name.S02E05.720p.HDTV.x264-GRP.mp4",
                "Show Name S02E05",
            ),
            ("1917.2019.1080p.BluRay.mp4", "1917"),
            ("Moj omiljeni film [1080p] (2020).mp4", "Moj omiljeni film"),
            ("č ć š ž đ 日本語.mp4", "č ć š ž đ 日本語"),
            ("holiday_clip_final.mov", "holiday clip final"),
            ("2012.2009.BluRay.mkv", "2012"),
            ("Movie.Name.2019.mkv", "Movie Name"),
        ];
        for (path, expected) in cases {
            let got = suggest_query(&PathBuf::from(path));
            assert_eq!(got, expected, "{path}");
            for private in ["Private", "Client", "Folder", "B:", "D:", "C:"] {
                assert!(!got.contains(private), "path leaked into {got:?}");
            }
        }
    }

    #[test]
    fn language_ids_are_unique_and_carry_decoder_tags() {
        let mut ids: Vec<_> = LANGUAGES.iter().map(|l| l.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), LANGUAGES.len());
        assert_eq!(language("hrv").unwrap().name, "Croatian");
        assert!(language("xxx").is_none());
        assert!(LANGUAGES.iter().all(|l| !l.tags.is_empty()));
    }

    #[test]
    fn nothing_opens_until_submit_and_submit_opens_exactly_once() {
        let mut calls = Vec::new();
        let mut form = SearchForm::new(
            "Dune".into(),
            Some("hrv".into()),
            Provider::OpenSubtitlesOrg,
        );
        // Reading, editing and building the address never reach the opener.
        let _ = form.url();
        form.query.push_str(" Part Two");
        form.provider = Provider::Titlovi;
        let _ = form.url();
        assert!(calls.is_empty());
        let mut opener = |url: &str| {
            calls.push(url.to_string());
            Ok(())
        };
        form.submit(&mut opener).unwrap();
        assert_eq!(
            calls,
            vec!["https://titlovi.com/titlovi/?prijevod=Dune%20Part%20Two".to_string()]
        );
        assert!(matches!(form.status, Some(Ok(_))));
    }

    #[test]
    fn an_empty_title_never_calls_the_opener_and_a_failing_opener_is_reported() {
        let mut calls = 0;
        let mut form = SearchForm::new("  ".into(), None, Provider::OpenSubtitles);
        assert_eq!(
            form.submit(&mut |_| {
                calls += 1;
                Ok(())
            }),
            Err(SearchError::EmptyQuery)
        );
        assert_eq!(calls, 0);
        form.query = "Dune".into();
        let result = form.submit(&mut |_| Err("no browser".into()));
        assert_eq!(result, Err(SearchError::Opener("no browser".into())));
        assert!(matches!(form.status, Some(Err(ref e)) if e.contains("no browser")));
        assert!(form.last_url.is_none());
    }

    #[test]
    fn begin_marks_one_action_pending_and_finished_reports_the_answer() {
        let mut form = SearchForm::new("Dune".into(), None, Provider::Titlovi);
        let url = form.begin().unwrap();
        assert!(allowed_https(&url));
        assert!(form.pending);
        // A second press before the first answer is refused and does not produce a second address.
        assert_eq!(form.begin(), Err(SearchError::Busy));
        form.finished(&url, Err("no default browser".into()));
        assert!(!form.pending && form.last_url.is_none());
        assert!(matches!(form.status, Some(Err(ref e)) if e.contains("no default browser")));
        let url = form.begin().unwrap();
        form.finished(&url, Ok(()));
        assert!(!form.pending);
        assert_eq!(form.last_url.as_deref(), Some(url.as_str()));
        assert!(matches!(form.status, Some(Ok(ref s)) if s.starts_with("Opened ")));
    }

    #[cfg(windows)]
    #[test]
    fn the_real_opener_refuses_addresses_outside_the_allow_list_without_launching_anything() {
        for url in [
            "http://www.opensubtitles.org/en/search",
            "https://example.test/",
            "file:///C:/Windows/notepad.exe",
            "calc.exe",
            "https://www.opensubtitles.org/a b",
        ] {
            assert!(open_in_browser(url).is_err(), "{url}");
        }
    }
}
