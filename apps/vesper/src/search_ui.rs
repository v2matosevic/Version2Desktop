//! The subtitle search sheet as a function of its form. It only reports what the owner did this
//! frame; opening the browser is the caller's decision, taken from `SheetEvents::submit`. Drawing,
//! typing, choosing a site or language, and pressing Esc never reach the network.
use crate::subsearch::{self, SearchForm};
use eframe::egui::{self, Key};
use v2_ui::Icon;

#[derive(Debug, Default, PartialEq)]
pub struct SheetEvents {
    /// The owner pressed Search or Enter in the title field.
    pub submit: bool,
    pub copy_address: bool,
    pub load_downloaded: bool,
    /// The window's close button was used.
    pub closed: bool,
    /// Where the Search button was drawn, so a test can click it.
    pub search_button: Option<egui::Rect>,
}

pub fn show(ctx: &egui::Context, form: &mut SearchForm, focus_title: bool) -> SheetEvents {
    let mut events = SheetEvents::default();
    let mut open = true;
    // A small window scrolls the sheet instead of clipping it.
    let max_height = (ctx.content_rect().height() - 24.0).max(200.0);
    egui::Window::new("Find subtitles online")
        .collapsible(false)
        .resizable(false)
        .max_height(max_height)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .open(&mut open)
        .show(ctx, |ui| {
            ui.set_width(440.0);
            egui::ScrollArea::vertical()
                .max_height(max_height - 56.0)
                .auto_shrink([false, true])
                .show(ui, |ui| {
            ui.label(v2_ui::caption(
                "Search by title and language in your browser. Download a subtitle there, then drop it here.",
            ));
            ui.add_space(10.0);
            ui.label("Movie or show");
            let title = ui.add(
                egui::TextEdit::singleline(&mut form.query)
                    .hint_text("Title, for example Blade Runner 2049")
                    .desired_width(f32::INFINITY),
            );
            if focus_title {
                title.request_focus();
            }
            if title.lost_focus() && ctx.input(|i| i.key_pressed(Key::Enter)) {
                events.submit = true;
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.label("Language");
                let shown = form
                    .language
                    .as_deref()
                    .and_then(subsearch::language)
                    .map_or("Any language", |l| l.name);
                egui::ComboBox::from_id_salt("vesper-subtitle-language")
                    .selected_text(shown)
                    .height(300.0)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut form.language, None, "Any language");
                        for language in subsearch::LANGUAGES {
                            ui.selectable_value(
                                &mut form.language,
                                Some(language.id.to_string()),
                                language.name,
                            );
                        }
                    });
            });
            ui.add_space(8.0);
            ui.label("Site");
            ui.horizontal_wrapped(|ui| {
                for provider in subsearch::Provider::ALL {
                    ui.radio_value(&mut form.provider, provider, provider.name());
                }
            });
            ui.label(v2_ui::caption(form.provider.language_note()));
            ui.add_space(8.0);
            let address = form.url();
            if let Ok(url) = &address {
                ui.collapsing("Address that will open", |ui| {
                    ui.add(
                        egui::Label::new(egui::RichText::new(url).monospace().size(12.0)).wrap(),
                    );
                });
            }
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                let search = v2_ui::Button::primary("Search in browser")
                    .icon(Icon::Search)
                    .pill()
                    .enabled(address.is_ok() && !form.pending)
                    .show(ui);
                events.search_button = Some(search.rect);
                if search.clicked() {
                    events.submit = true;
                }
                if v2_ui::Button::secondary("Copy address")
                    .pill()
                    .enabled(address.is_ok())
                    .show(ui)
                    .clicked()
                {
                    events.copy_address = true;
                }
            });
            match &form.status {
                Some(Ok(done)) => {
                    ui.add_space(8.0);
                    ui.label(done.as_str());
                    ui.label(v2_ui::caption(
                        "Save the subtitle there (a .zip needs unzipping first), then drop it on this window or choose it here.",
                    ));
                    if v2_ui::Button::secondary("Load the downloaded file…")
                        .icon(Icon::Open)
                        .pill()
                        .show(ui)
                        .clicked()
                    {
                        events.load_downloaded = true;
                    }
                }
                Some(Err(e)) => {
                    ui.add_space(8.0);
                    ui.colored_label(v2_ui::palette_of(ui).danger, e.as_str());
                }
                None => {}
            }
            ui.add_space(8.0);
            ui.label(v2_ui::caption(
                "Nothing is sent until you press Search, and Vesper does not download for you. Playing files stays fully offline.",
            ));
                });
        });
    events.closed = !open;
    events
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subsearch::Provider;
    use egui::{Event, PointerButton, Pos2, RawInput};

    fn screen() -> egui::Rect {
        egui::Rect::from_min_size(Pos2::ZERO, egui::vec2(900.0, 700.0))
    }
    fn frame(
        ctx: &egui::Context,
        form: &mut SearchForm,
        events: Vec<Event>,
        focus_title: bool,
    ) -> SheetEvents {
        let mut out = SheetEvents::default();
        let _ = ctx.run_ui(
            RawInput {
                screen_rect: Some(screen()),
                events,
                ..Default::default()
            },
            |ui| out = show(ui.ctx(), form, focus_title),
        );
        out
    }
    fn form() -> SearchForm {
        SearchForm::new(
            "Blade Runner 2049".into(),
            Some("hrv".into()),
            Provider::OpenSubtitlesOrg,
        )
    }
    fn click(at: Pos2) -> Vec<Event> {
        let button = |pressed| Event::PointerButton {
            pos: at,
            button: PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        vec![Event::PointerMoved(at), button(true), button(false)]
    }

    #[test]
    fn drawing_typing_and_choosing_do_not_submit() {
        let ctx = egui::Context::default();
        v2_ui::install(&ctx, v2_ui::App::Vesper);
        let mut form = form();
        for _ in 0..3 {
            assert!(!frame(&ctx, &mut form, vec![], true).submit);
        }
        let typed = frame(
            &ctx,
            &mut form,
            vec![
                Event::Text(" Part".into()),
                Event::Key {
                    key: Key::Escape,
                    physical_key: Some(Key::Escape),
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
            false,
        );
        assert!(!typed.submit && !typed.copy_address && !typed.load_downloaded);
        form.provider = Provider::Titlovi;
        form.language = None;
        assert!(!frame(&ctx, &mut form, vec![], false).submit);
        assert!(!form.pending && form.status.is_none());
    }

    #[test]
    fn clicking_the_search_button_submits_exactly_once_and_nothing_else_does() {
        let ctx = egui::Context::default();
        v2_ui::install(&ctx, v2_ui::App::Vesper);
        let mut form = form();
        // First frames lay the window out and tell us where the button is.
        let mut button = None;
        for _ in 0..3 {
            button = frame(&ctx, &mut form, vec![], false).search_button;
        }
        let button = button.expect("the button is drawn");
        let first = frame(&ctx, &mut form, click(button.center()), false);
        assert!(first.submit, "a click on Search must submit");
        let after = frame(&ctx, &mut form, vec![], false);
        assert!(!after.submit, "no second submit without another press");
    }

    #[test]
    fn enter_in_the_title_field_submits_and_an_empty_title_disables_the_button() {
        let ctx = egui::Context::default();
        v2_ui::install(&ctx, v2_ui::App::Vesper);
        let mut form = form();
        // Focus the field, then press Enter while it has focus.
        frame(&ctx, &mut form, vec![], true);
        frame(&ctx, &mut form, vec![], false);
        let enter = Event::Key {
            key: Key::Enter,
            physical_key: Some(Key::Enter),
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        };
        let pressed = frame(&ctx, &mut form, vec![enter], false);
        assert!(pressed.submit);
        // With nothing to search for, the address is not offered and a click does not submit.
        let mut empty = SearchForm::new(String::new(), None, Provider::Titlovi);
        let mut button = None;
        for _ in 0..3 {
            button = frame(&ctx, &mut empty, vec![], false).search_button;
        }
        let clicked = frame(&ctx, &mut empty, click(button.unwrap().center()), false);
        assert!(!clicked.submit);
    }
}
