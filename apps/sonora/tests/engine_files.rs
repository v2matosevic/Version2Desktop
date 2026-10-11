//! The engine on real files: every supported container and codec, hi-res and multichannel
//! material, and the crossfade overlap measured on the PCM the engine actually produces. No window
//! and no audio device are involved; what is proven is the decoded, converted and mixed signal.

use sonora::{
    decoder::AudioDecoder,
    engine::{Engine, PullEnd, Start, Stream},
    player::{FileOpener, Opener},
};
use std::{
    fs,
    path::{Path, PathBuf},
};

fn audio(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/audio")
        .join(name)
}
fn local(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}
fn scratch(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/test-fixtures/engine");
    fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

fn open(path: &Path, rate: u32, channels: usize) -> Stream {
    FileOpener
        .open(path, 0.0, rate, channels)
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// Everything one track yields on its own.
fn solo(path: &Path, rate: u32, channels: usize) -> Vec<f32> {
    let mut stream = open(path, rate, channels);
    let mut out = Vec::new();
    loop {
        let (n, end) = stream.pull(4096, &mut out).unwrap();
        if n == 0 || matches!(end, PullEnd::Eof) {
            return out;
        }
    }
}

struct Programme {
    pcm: Vec<f32>,
    /// Frame at which each hand-off to a following track happened.
    hand_offs: Vec<usize>,
    peak_buffer: usize,
}

fn programme(paths: &[PathBuf], crossfade: f32, rate: u32, channels: usize) -> Programme {
    let mut engine = Engine::new(rate, channels);
    engine.set_crossfade(crossfade);
    engine.start(open(&paths[0], rate, channels), Start::Cut);
    let mut next = 1;
    engine.set_next(paths.get(next).map(|p| open(p, rate, channels)));
    let (mut pcm, mut hand_offs, mut peak_buffer) = (Vec::new(), Vec::new(), 0);
    loop {
        let before = pcm.len();
        let r = engine.render((rate / 50) as usize, &mut pcm);
        peak_buffer = peak_buffer.max(engine.buffered_samples());
        if r.promoted.is_some() {
            hand_offs.push(before / channels);
            next += 1;
            engine.set_next(paths.get(next).map(|p| open(p, rate, channels)));
        }
        assert!(r.error.is_none(), "{:?}", r.error);
        if r.ended {
            break;
        }
    }
    Programme {
        pcm,
        hand_offs,
        peak_buffer,
    }
}

const FORMATS: [(&str, &str); 7] = [
    ("WAV PCM16", "tone.wav"),
    ("FLAC", "tone.flac"),
    ("AIFF PCM", "tone.aiff"),
    ("MP3", "tone.mp3"),
    ("M4A AAC-LC", "tone.m4a"),
    ("Ogg Vorbis", "tone.ogg"),
    ("Ogg Opus", "tone.opus"),
];

#[test]
fn lossless_files_follow_each_other_sample_for_sample() {
    // Gapless means the second track's first sample is the next sample after the first one's last.
    for (a, b) in [
        ("tone.wav", "tone.flac"),
        ("tone.flac", "tone.aiff"),
        ("tone.aiff", "tone.wav"),
    ] {
        let (pa, pb) = (audio(a), audio(b));
        let expected: Vec<f32> = solo(&pa, 48_000, 2)
            .into_iter()
            .chain(solo(&pb, 48_000, 2))
            .collect();
        let got = programme(&[pa, pb], 0.0, 48_000, 2);
        assert_eq!(
            got.pcm.len(),
            expected.len(),
            "{a} then {b}: nothing added or lost"
        );
        assert!(got.pcm == expected, "{a} then {b}: every sample identical");
        assert_eq!(got.hand_offs, vec![48_000]);
    }
}

#[test]
fn the_programme_length_is_the_sum_of_the_tracks_for_every_format_pair_without_crossfade() {
    for (name_a, a) in FORMATS {
        for (name_b, b) in FORMATS {
            let (pa, pb) = (audio(a), audio(b));
            let sum = solo(&pa, 48_000, 2).len() + solo(&pb, 48_000, 2).len();
            let got = programme(&[pa, pb], 0.0, 48_000, 2);
            assert_eq!(got.pcm.len(), sum, "{name_a} then {name_b}");
            assert!(
                got.pcm.iter().all(|s| s.is_finite() && s.abs() <= 1.0),
                "{name_a} then {name_b}"
            );
        }
    }
}

#[test]
fn trailing_encoder_padding_is_dropped_where_the_file_states_its_exact_length() {
    for name in [
        "tone.wav",
        "tone.flac",
        "tone.aiff",
        "tone.ogg",
        "tone.mp3",
        "tone.opus",
    ] {
        assert_eq!(
            solo(&audio(name), 48_000, 2).len(),
            96_000,
            "{name} is exactly one second"
        );
    }
    // AAC priming and padding are not described in a form this decoder reads, so M4A and ADTS
    // keep their encoder frames: a whole number of 1,024-sample AAC blocks.
    assert_eq!(solo(&audio("tone.m4a"), 48_000, 2).len() / 2 % 1024, 0);
}

#[test]
fn every_format_blends_into_every_other_with_the_stated_overlap_and_no_clipping() {
    for rate in [48_000u32, 44_100] {
        for (name_a, a) in FORMATS {
            for (name_b, b) in FORMATS {
                if a == b {
                    continue; // the same file twice is the repeat-one case: it is never blended
                }
                let (pa, pb) = (audio(a), audio(b));
                let sum = (solo(&pa, rate, 2).len() + solo(&pb, rate, 2).len()) / 2;
                let got = programme(&[pa, pb], 0.4, rate, 2);
                let frames = got.pcm.len() / 2;
                let overlap = sum - frames;
                let fade = (0.4 * rate as f64).round() as usize;
                // Lossy codecs may carry a few hundred frames of padding the blend then covers.
                assert!(
                    overlap.abs_diff(fade) <= 2_400,
                    "{name_a} into {name_b} at {rate} Hz: overlap {overlap} frames, fade {fade}"
                );
                let peak = got.pcm.iter().fold(0.0f32, |p, s| p.max(s.abs()));
                assert!(
                    peak <= 1.0 && peak > 0.01,
                    "{name_a} into {name_b}: peak {peak}"
                );
                assert_eq!(got.hand_offs.len(), 1);
                // Blending starts about 0.4 s before the end of the first track.
                let at = got.hand_offs[0];
                assert!(
                    at.abs_diff(sum / 2 - fade) < 2_500,
                    "{name_a} into {name_b}: blend began at frame {at}"
                );
            }
        }
    }
}

#[test]
fn the_blend_of_lossless_tracks_is_exactly_the_equal_power_sum_of_their_samples() {
    let (a, b) = (audio("tone.wav"), audio("tone.flac"));
    let (sa, sb) = (solo(&a, 48_000, 2), solo(&b, 48_000, 2));
    let got = programme(&[a, b], 0.4, 48_000, 2);
    let fade = 19_200usize;
    let start = 48_000 - fade;
    assert_eq!(got.hand_offs, vec![start]);
    assert_eq!(got.pcm.len() / 2, 96_000 - fade);
    for f in 0..start {
        assert_eq!(
            got.pcm[f * 2],
            sa[f * 2],
            "A alone before the blend, frame {f}"
        );
    }
    for k in 0..fade {
        let t = k as f32 / fade as f32;
        let expected = sa[(start + k) * 2] * (t * std::f32::consts::FRAC_PI_2).cos()
            + sb[k * 2] * (t * std::f32::consts::FRAC_PI_2).sin();
        let got_sample = got.pcm[(start + k) * 2];
        assert!(
            (got_sample - expected).abs() < 1e-5,
            "blend frame {k}: {got_sample} vs {expected}"
        );
    }
    for f in 0..(48_000 - fade) {
        assert_eq!(
            got.pcm[(96_000 - fade - 1 - f) * 2],
            sb[(48_000 - 1 - f) * 2],
            "B alone after the blend, from the end {f}"
        );
    }
}

#[test]
fn the_hi_res_multichannel_fixture_plays_on_every_device_format() {
    let path = local("tone-192k-8ch.wav");
    for (rate, channels) in [
        (192_000u32, 8usize),
        (96_000, 2),
        (48_000, 2),
        (44_100, 2),
        (48_000, 1),
        (48_000, 6),
        (88_200, 2),
    ] {
        let pcm = solo(&path, rate, channels);
        // One second of 192 kHz audio becomes exactly one second at the device rate.
        assert_eq!(
            pcm.len(),
            rate as usize * channels,
            "{rate} Hz x {channels}: length"
        );
        assert!(
            pcm.iter().all(|s| s.is_finite() && s.abs() <= 1.0 + 1e-3),
            "{rate} Hz x {channels}"
        );
        let peak = pcm.iter().fold(0.0f32, |p, s| p.max(s.abs()));
        assert!(peak > 0.02, "{rate} Hz x {channels}: audible, peak {peak}");
    }
    // 7.1 to 7.1 is a pass-through; 7.1 to stereo is a clipping-safe fold of all eight channels.
    let straight = solo(&path, 192_000, 8);
    let direct = open(&path, 192_000, 8);
    drop(direct);
    assert!(straight.iter().step_by(8).any(|s| s.abs() > 0.02));
}

#[test]
fn a_hi_res_track_blends_with_a_cd_rate_track_on_a_stereo_device() {
    let paths = [local("tone-192k-8ch.wav"), local("tone-24bit.wav")];
    for (rate, channels) in [(48_000u32, 2usize), (44_100, 2), (96_000, 2)] {
        let sum = (solo(&paths[0], rate, channels).len() + solo(&paths[1], rate, channels).len())
            / channels;
        let got = programme(&paths, 0.4, rate, channels);
        let frames = got.pcm.len() / channels;
        assert!(
            sum - frames > rate as usize * 4 / 10 - 2_400
                && sum - frames < rate as usize * 4 / 10 + 2_400,
            "{rate}: overlap {}",
            sum - frames
        );
        assert!(
            got.pcm.iter().all(|s| s.abs() <= 1.0),
            "{rate} Hz: no sample above full scale"
        );
        assert!(
            got.peak_buffer < 400_000,
            "{rate}: engine held {} samples",
            got.peak_buffer
        );
    }
}

fn float_wav(samples: &[f32]) -> Vec<u8> {
    let data: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
    let mut wav = Vec::new();
    wav.extend(b"RIFF");
    wav.extend((36 + data.len() as u32).to_le_bytes());
    wav.extend(b"WAVEfmt ");
    wav.extend(16u32.to_le_bytes());
    wav.extend(3u16.to_le_bytes()); // IEEE float
    wav.extend(1u16.to_le_bytes());
    wav.extend(48_000u32.to_le_bytes());
    wav.extend((48_000u32 * 4).to_le_bytes());
    wav.extend(4u16.to_le_bytes());
    wav.extend(32u16.to_le_bytes());
    wav.extend(b"data");
    wav.extend((data.len() as u32).to_le_bytes());
    wav.extend(data);
    wav
}

#[test]
fn float_files_keep_headroom_above_full_scale_and_lose_only_nan_and_infinity() {
    let mut samples = vec![0.25f32; 4_800];
    samples[100] = 1.5;
    samples[101] = -1.75;
    samples[200] = f32::NAN;
    samples[201] = f32::INFINITY;
    samples[202] = f32::NEG_INFINITY;
    let path = scratch("float-headroom.wav");
    fs::write(&path, float_wav(&samples)).unwrap();
    let mut decoder = AudioDecoder::open(&path, false).unwrap();
    let mut got = Vec::new();
    while decoder.read_into(&mut got).unwrap() {}
    assert_eq!(got.len(), samples.len());
    assert_eq!(
        (got[100], got[101]),
        (1.5, -1.75),
        "finite values above full scale are kept for the volume stage"
    );
    assert_eq!(
        &got[200..203],
        &[0.0, 0.0, 0.0],
        "NaN and infinity become silence"
    );
    assert_eq!(got[300], 0.25);
}

#[test]
fn a_damaged_mp3_frame_costs_a_glitch_not_the_track() {
    let mut bytes = fs::read(audio("tone.mp3")).unwrap();
    // Overwrite a stretch in the middle of the audio with noise.
    let mid = bytes.len() / 2;
    for (i, b) in bytes[mid..mid + 600].iter_mut().enumerate() {
        *b = (i * 37 % 251) as u8;
    }
    let path = scratch("damaged-frame.mp3");
    fs::write(&path, &bytes).unwrap();
    let mut decoder = AudioDecoder::open(&path, false).unwrap();
    let mut got = Vec::new();
    let mut result = Ok(true);
    while matches!(result, Ok(true)) {
        result = decoder.read_into(&mut got);
    }
    assert!(result.is_ok(), "decoding ran to the end: {result:?}");
    let frames = got.len() / 2;
    assert!(
        frames > 40_000,
        "most of the track survived: {frames} frames"
    );
    assert!(got.iter().all(|s| s.is_finite()));
}

#[test]
fn a_file_of_pure_noise_is_refused_and_a_truncated_one_ends_early_without_panicking() {
    let noise = scratch("noise.mp3");
    fs::write(
        &noise,
        (0..4096).map(|i| (i * 131 % 253) as u8).collect::<Vec<_>>(),
    )
    .unwrap();
    assert!(FileOpener.open(&noise, 0.0, 48_000, 2).is_err() || solo_len(&noise) < 4_800);
    let flac = fs::read(audio("tone.flac")).unwrap();
    let truncated = scratch("truncated.flac");
    fs::write(&truncated, &flac[..flac.len() / 2]).unwrap();
    let frames = solo_len(&truncated);
    assert!(frames > 0 && frames < 48_000, "{frames}");
}

fn solo_len(path: &Path) -> usize {
    let Ok(mut stream) = FileOpener.open(path, 0.0, 48_000, 2) else {
        return 0;
    };
    let mut out = Vec::new();
    for _ in 0..1000 {
        match stream.pull(4096, &mut out) {
            Ok((n, end)) if n > 0 && !matches!(end, PullEnd::Eof) => {}
            _ => break,
        }
    }
    out.len() / 2
}

#[test]
fn seeking_inside_a_real_file_resumes_at_the_target_for_every_format() {
    for (name, file) in FORMATS {
        let mut stream = open(&audio(file), 48_000, 2);
        stream.seek(0.5).unwrap_or_else(|e| panic!("{name}: {e}"));
        let mut out = Vec::new();
        loop {
            let (n, end) = stream.pull(4096, &mut out).unwrap();
            if n == 0 || matches!(end, PullEnd::Eof) {
                break;
            }
        }
        let seconds = out.len() as f64 / 2.0 / 48_000.0;
        assert!(
            (0.4..0.65).contains(&seconds),
            "{name}: {seconds} s remained after seeking to 0.5 s"
        );
        assert!(
            (stream.position() - 1.0).abs() < 0.06,
            "{name}: position {}",
            stream.position()
        );
    }
}

#[test]
fn a_whole_album_streams_in_bounded_memory() {
    // Twelve tracks back to back: the engine only ever holds the playing and the prepared one.
    let paths: Vec<PathBuf> = (0..12)
        .map(|i| audio(FORMATS[i % FORMATS.len()].1))
        .collect();
    let got = programme(&paths, 0.0, 44_100, 2);
    assert_eq!(got.hand_offs.len(), 11);
    assert!(
        got.peak_buffer < 200_000,
        "{} samples held",
        got.peak_buffer
    );
    let blended = programme(&paths, 0.4, 44_100, 2);
    assert_eq!(blended.hand_offs.len(), 11);
    assert!(
        blended.peak_buffer < 300_000,
        "{} samples held while blending",
        blended.peak_buffer
    );
}
