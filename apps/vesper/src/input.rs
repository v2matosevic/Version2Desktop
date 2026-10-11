//! Keyboard shortcuts as data: which key means which player command, and when a key belongs to a
//! text field or a focused control instead. Pure over an `egui::Context`'s input, so the whole
//! table is tested with real egui input events and no window.
use eframe::egui::{self, Key, Modifiers};

#[derive(Clone, Debug, PartialEq)]
pub enum Cmd {
    OpenFile,
    ToggleHelp,
    TogglePause,
    SeekBy(f64),
    Restart,
    VolumeBy(f64),
    ToggleMute,
    ToggleFullscreen,
    /// Esc: leave full screen, or give up on a file that is still opening.
    Escape,
    CycleSubtitles,
    CycleAudio,
    ToggleSubtitles,
    SpeedBy(f64),
    SpeedReset,
    FrameStep(i8),
    SubtitleDelayBy(f64),
    ToggleInfo,
    FindSubtitles,
    PreviousVideo,
    NextVideo,
}

/// What the app knows that decides who owns a key.
#[derive(Clone, Copy, Debug, Default)]
pub struct Focus {
    /// A text field is being typed into: letters belong to it.
    pub typing: bool,
    /// Some control has keyboard focus (reached with Tab): Space, Enter and arrows belong to it.
    pub control: bool,
    /// The subtitle search sheet is open: only Esc, F1 and Ctrl+O still act.
    pub sheet: bool,
}

impl Focus {
    pub fn from_context(ctx: &egui::Context, sheet: bool) -> Self {
        Self {
            typing: ctx.egui_wants_keyboard_input(),
            control: ctx.memory(|m| m.focused().is_some()) || ctx.egui_wants_keyboard_input(),
            sheet,
        }
    }
}

/// Consume this frame's shortcut keys and return the commands they mean, in order.
pub fn commands(ctx: &egui::Context, focus: Focus) -> Vec<Cmd> {
    let mut out = Vec::new();
    let take = |modifiers: Modifiers, key: Key| ctx.input_mut(|i| i.consume_key(modifiers, key));
    if take(Modifiers::CTRL, Key::O) {
        out.push(Cmd::OpenFile);
    }
    if take(Modifiers::NONE, Key::F1) {
        out.push(Cmd::ToggleHelp);
    }
    if focus.sheet {
        if take(Modifiers::NONE, Key::Escape) {
            out.push(Cmd::Escape);
        }
        return out;
    }
    if take(Modifiers::CTRL, Key::F) || take(Modifiers::CTRL | Modifiers::SHIFT, Key::F) {
        out.push(Cmd::FindSubtitles);
    }
    if take(Modifiers::NONE, Key::Escape) {
        out.push(Cmd::Escape);
    }
    if !focus.control && take(Modifiers::NONE, Key::Space) {
        out.push(Cmd::TogglePause);
    }
    if !focus.control {
        // Shift first: a plain-arrow check could otherwise also match a shifted press.
        if take(Modifiers::SHIFT, Key::ArrowLeft) {
            out.push(Cmd::SeekBy(-30.0));
        } else if take(Modifiers::NONE, Key::ArrowLeft) {
            out.push(Cmd::SeekBy(-5.0));
        }
        if take(Modifiers::SHIFT, Key::ArrowRight) {
            out.push(Cmd::SeekBy(30.0));
        } else if take(Modifiers::NONE, Key::ArrowRight) {
            out.push(Cmd::SeekBy(5.0));
        }
        if take(Modifiers::NONE, Key::ArrowUp) {
            out.push(Cmd::VolumeBy(5.0));
        }
        if take(Modifiers::NONE, Key::ArrowDown) {
            out.push(Cmd::VolumeBy(-5.0));
        }
        if take(Modifiers::NONE, Key::Home) {
            out.push(Cmd::Restart);
        }
        if take(Modifiers::NONE, Key::PageUp) {
            out.push(Cmd::PreviousVideo);
        }
        if take(Modifiers::NONE, Key::PageDown) {
            out.push(Cmd::NextVideo);
        }
    }
    if focus.typing {
        return out;
    }
    let letters = [
        (Key::J, Cmd::SeekBy(-10.0)),
        (Key::L, Cmd::SeekBy(10.0)),
        (Key::M, Cmd::ToggleMute),
        (Key::S, Cmd::CycleSubtitles),
        (Key::A, Cmd::CycleAudio),
        (Key::V, Cmd::ToggleSubtitles),
        (Key::I, Cmd::ToggleInfo),
        (Key::Z, Cmd::SubtitleDelayBy(-0.1)),
        (Key::X, Cmd::SubtitleDelayBy(0.1)),
        (Key::OpenBracket, Cmd::SpeedBy(-0.25)),
        (Key::CloseBracket, Cmd::SpeedBy(0.25)),
        (Key::Backspace, Cmd::SpeedReset),
        (Key::Comma, Cmd::FrameStep(-1)),
        (Key::Period, Cmd::FrameStep(1)),
    ];
    for (key, cmd) in letters {
        if take(Modifiers::NONE, key) {
            out.push(cmd);
        }
    }
    if take(Modifiers::NONE, Key::F) || take(Modifiers::NONE, Key::F11) {
        out.push(Cmd::ToggleFullscreen);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{Event, RawInput};

    fn press(key: Key, modifiers: Modifiers) -> Event {
        Event::Key {
            key,
            physical_key: Some(key),
            pressed: true,
            repeat: false,
            modifiers,
        }
    }
    fn run(events: Vec<Event>, focus: Focus) -> Vec<Cmd> {
        let ctx = egui::Context::default();
        let mut out = Vec::new();
        let modifiers = events
            .iter()
            .find_map(|e| match e {
                Event::Key { modifiers, .. } => Some(*modifiers),
                _ => None,
            })
            .unwrap_or_default();
        let _ = ctx.run_ui(
            RawInput {
                events,
                modifiers,
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(800.0, 600.0),
                )),
                ..Default::default()
            },
            |ui| out = commands(ui.ctx(), focus),
        );
        out
    }
    fn plain(key: Key) -> Vec<Cmd> {
        run(vec![press(key, Modifiers::NONE)], Focus::default())
    }

    #[test]
    fn transport_keys_map_to_player_commands() {
        assert_eq!(plain(Key::Space), vec![Cmd::TogglePause]);
        assert_eq!(plain(Key::ArrowLeft), vec![Cmd::SeekBy(-5.0)]);
        assert_eq!(plain(Key::ArrowRight), vec![Cmd::SeekBy(5.0)]);
        assert_eq!(plain(Key::J), vec![Cmd::SeekBy(-10.0)]);
        assert_eq!(plain(Key::L), vec![Cmd::SeekBy(10.0)]);
        assert_eq!(plain(Key::ArrowUp), vec![Cmd::VolumeBy(5.0)]);
        assert_eq!(plain(Key::ArrowDown), vec![Cmd::VolumeBy(-5.0)]);
        assert_eq!(plain(Key::M), vec![Cmd::ToggleMute]);
        assert_eq!(plain(Key::F), vec![Cmd::ToggleFullscreen]);
        assert_eq!(plain(Key::F11), vec![Cmd::ToggleFullscreen]);
        assert_eq!(plain(Key::Home), vec![Cmd::Restart]);
        assert_eq!(plain(Key::PageUp), vec![Cmd::PreviousVideo]);
        assert_eq!(plain(Key::PageDown), vec![Cmd::NextVideo]);
        assert_eq!(plain(Key::Escape), vec![Cmd::Escape]);
    }

    #[test]
    fn track_speed_and_subtitle_keys_map_to_commands() {
        assert_eq!(plain(Key::S), vec![Cmd::CycleSubtitles]);
        assert_eq!(plain(Key::A), vec![Cmd::CycleAudio]);
        assert_eq!(plain(Key::V), vec![Cmd::ToggleSubtitles]);
        assert_eq!(plain(Key::I), vec![Cmd::ToggleInfo]);
        assert_eq!(plain(Key::Z), vec![Cmd::SubtitleDelayBy(-0.1)]);
        assert_eq!(plain(Key::X), vec![Cmd::SubtitleDelayBy(0.1)]);
        assert_eq!(plain(Key::OpenBracket), vec![Cmd::SpeedBy(-0.25)]);
        assert_eq!(plain(Key::CloseBracket), vec![Cmd::SpeedBy(0.25)]);
        assert_eq!(plain(Key::Backspace), vec![Cmd::SpeedReset]);
        assert_eq!(plain(Key::Comma), vec![Cmd::FrameStep(-1)]);
        assert_eq!(plain(Key::Period), vec![Cmd::FrameStep(1)]);
        assert_eq!(plain(Key::F1), vec![Cmd::ToggleHelp]);
    }

    #[test]
    fn shift_arrows_seek_thirty_seconds_and_not_five() {
        assert_eq!(
            run(
                vec![press(Key::ArrowLeft, Modifiers::SHIFT)],
                Focus::default()
            ),
            vec![Cmd::SeekBy(-30.0)]
        );
        assert_eq!(
            run(
                vec![press(Key::ArrowRight, Modifiers::SHIFT)],
                Focus::default()
            ),
            vec![Cmd::SeekBy(30.0)]
        );
    }

    #[test]
    fn control_shortcuts_open_and_search() {
        assert_eq!(
            run(vec![press(Key::O, Modifiers::CTRL)], Focus::default()),
            vec![Cmd::OpenFile]
        );
        assert_eq!(
            run(vec![press(Key::F, Modifiers::CTRL)], Focus::default()),
            vec![Cmd::FindSubtitles]
        );
    }

    #[test]
    fn a_focused_control_keeps_space_and_the_arrows() {
        let focus = Focus {
            control: true,
            ..Focus::default()
        };
        for key in [
            Key::Space,
            Key::ArrowLeft,
            Key::ArrowRight,
            Key::ArrowUp,
            Key::ArrowDown,
        ] {
            assert!(
                run(vec![press(key, Modifiers::NONE)], focus).is_empty(),
                "{key:?}"
            );
        }
        // Letters still reach the player when only a button or slider has focus.
        assert_eq!(
            run(vec![press(Key::M, Modifiers::NONE)], focus),
            vec![Cmd::ToggleMute]
        );
    }

    #[test]
    fn typing_in_a_text_field_never_triggers_a_shortcut() {
        let focus = Focus {
            typing: true,
            control: true,
            sheet: false,
        };
        for key in [
            Key::M,
            Key::F,
            Key::J,
            Key::L,
            Key::S,
            Key::A,
            Key::V,
            Key::I,
            Key::Z,
            Key::X,
            Key::OpenBracket,
            Key::CloseBracket,
            Key::Backspace,
            Key::Comma,
            Key::Period,
            Key::Space,
            Key::ArrowLeft,
            Key::F11,
        ] {
            assert!(
                run(vec![press(key, Modifiers::NONE)], focus).is_empty(),
                "{key:?}"
            );
        }
    }

    #[test]
    fn while_the_search_sheet_is_open_only_escape_help_and_open_act() {
        let focus = Focus {
            sheet: true,
            typing: true,
            control: true,
        };
        assert_eq!(
            run(vec![press(Key::Escape, Modifiers::NONE)], focus),
            vec![Cmd::Escape]
        );
        assert_eq!(
            run(vec![press(Key::F1, Modifiers::NONE)], focus),
            vec![Cmd::ToggleHelp]
        );
        assert_eq!(
            run(vec![press(Key::O, Modifiers::CTRL)], focus),
            vec![Cmd::OpenFile]
        );
        let sheet_only = Focus {
            sheet: true,
            ..Focus::default()
        };
        for key in [Key::Space, Key::M, Key::F, Key::ArrowRight, Key::S] {
            assert!(
                run(vec![press(key, Modifiers::NONE)], sheet_only).is_empty(),
                "{key:?}"
            );
        }
    }

    #[test]
    fn an_unbound_key_does_nothing_and_two_keys_in_one_frame_both_act() {
        assert!(plain(Key::Q).is_empty());
        let both = run(
            vec![
                press(Key::M, Modifiers::NONE),
                press(Key::L, Modifiers::NONE),
            ],
            Focus::default(),
        );
        assert!(both.contains(&Cmd::ToggleMute) && both.contains(&Cmd::SeekBy(10.0)));
    }
}
