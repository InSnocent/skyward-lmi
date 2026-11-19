//! Wallpaper import, cropping, opacity and frosted cards.

use super::*;

impl App {
    pub fn enable_wallpaper(&mut self) {
        self.state.wallpaper_enabled = true;
        self.refresh();
    }

    pub fn set_frosted_cards(&mut self, on: bool) {
        if self.state.frosted_cards != on {
            self.state.frosted_cards = on;
            self.refresh();
        }
    }

    pub fn set_wallpaper(&mut self, saved: Option<(Wallpaper, u8)>) {
        let (image, opacity) = saved.map_or((None, DEFAULT_OPACITY), |(image, opacity)| (Some(image), opacity.clamp(MIN_OPACITY, 100)));
        self.state.wallpaper_image = image;
        self.state.wallpaper_unreadable = false;
        self.state.wallpaper_opacity = opacity;
        self.wallpaper_opacity_before = None;
        self.refresh();
    }

    pub fn wallpaper_unreadable(&mut self, why: &str, now: u64) {
        self.set_wallpaper(None);
        self.state.wallpaper_unreadable = true;
        self.report(why, now);
    }

    pub fn wallpaper_opacity(&self) -> u8 {
        self.state.wallpaper_opacity
    }

    pub fn wallpaper_prepared(&mut self, result: Result<WallpaperCrop, String>, now: u64) {
        self.state.wallpaper_busy = false;
        if self.screen() != Screen::PickWallpaper || !self.state.data.storage_open() {
            self.refresh();
            return;
        }
        match result {
            Ok(crop) => {
                self.state.wallpaper_crop = Some(crop);
                self.open(Screen::CropWallpaper);
            }
            Err(why) => {
                self.refresh();
                self.report(&why, now);
            }
        }
    }

    pub fn wallpaper_crop(&self) -> Option<&WallpaperCrop> {
        self.state.wallpaper_crop.as_ref()
    }

    pub fn wallpaper_applied(&mut self, result: Result<Wallpaper, String>, now: u64) {
        self.state.wallpaper_busy = false;
        if self.screen() != Screen::CropWallpaper || !self.state.data.storage_open() {
            self.refresh();
            return;
        }
        match result {
            Ok(image) => {
                self.leave(true);
                self.state.wallpaper_crop = None;
                self.stack.pop();
                if self.stack.len() > 1 && self.screen() == Screen::PickWallpaper {
                    self.stack.pop();
                }
                self.settle_page();
                self.set_wallpaper(Some((image, self.state.wallpaper_opacity)));
            }
            Err(why) => {
                self.refresh();
                self.report(&why, now);
            }
        }
    }

    pub fn set_wallpaper_folder(&mut self, folder: String, files: Result<Vec<FileRow>, String>, listed: i64) {
        if self.screen() == Screen::PickWallpaper && self.state.data.storage_open() {
            self.show_folder(Screen::PickWallpaper, folder, files, listed);
        }
    }

    pub(super) fn save_wallpaper_opacity(&mut self) -> Option<Effect> {
        let before = self.wallpaper_opacity_before.take()?;
        let after = self.state.wallpaper_opacity;
        (before != after).then_some(Effect::SaveWallpaperOpacity { before, after })
    }

    pub fn wallpaper_opacity_not_saved(&mut self, before: u8, why: &str, now: u64) {
        self.state.wallpaper_opacity = before;
        self.refresh();
        self.report(why, now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::testing::*;

    #[test]
    fn lmi_settings_offer_a_wallpaper_page() {
        let (f, i) = (fonts(), info());
        let mut app = App::new(Screen::Settings, W, H, PPD);
        app.enable_wallpaper();
        let frame = app.frame(&f, &i);
        assert!(
            frame.hits.iter().any(|h| h.action == Action::Open(Screen::Wallpaper)),
            "lmi appearance settings should open the wallpaper page"
        );
    }

    #[test]
    fn wallpaper_appearance_offers_a_frosted_cards_switch() {
        let (f, i) = (fonts(), info());
        let mut app = App::new(Screen::Settings, W, H, PPD);
        app.enable_wallpaper();
        app.data_waiting(None);
        let toggle = Action::Toggle(Switch::FrostedCards);
        assert!(!app.state.frosted_cards);
        assert_eq!(tap(&mut app, &f, &i, toggle), vec![Effect::SaveFrostedCards(true)]);
        app.set_frosted_cards(true);
        assert_eq!(tap(&mut app, &f, &i, toggle), vec![Effect::SaveFrostedCards(false)]);
    }

    #[test]
    fn frosted_cards_apply_in_both_themes_with_unlocked_wallpaper() {
        let (f, i) = (fonts(), info());
        let (w, h, ppd) = (1080, 2400, 2.75);
        let image = test_wallpaper(w, h);
        for dark in [false, true] {
            let mut app = App::new(Screen::Home, w, h, ppd);
            app.enable_wallpaper();
            app.state.dark = dark;
            app.set_frosted_cards(true);
            app.set_wallpaper(Some((image.clone(), 65)));
            assert!(app.frame(&f, &i).frosted_cards.is_empty());
            app.data_waiting(None);
            let rect = app.frame(&f, &i).frosted_cards[0];
            let mut screen = Canvas::new(w, h);
            app.compose(&mut screen, &f, &i, "12:00", 0);
            let at = ((rect.y as usize + 16) * w as usize + rect.w as usize / 2) * 4;
            let frosted = screen.data[at..at + 4].to_vec();

            app.set_frosted_cards(false);
            app.compose(&mut screen, &f, &i, "12:00", 1);
            assert_ne!(frosted, screen.data[at..at + 4]);
            app.set_frosted_cards(true);
            app.set_wallpaper(Some((image.clone(), 0)));
            assert_eq!(app.wallpaper_opacity(), 5, "an older 0% setting loads at the new minimum");
            assert!(!app.frame(&f, &i).frosted_cards.is_empty());
            app.set_wallpaper(None);
            assert!(app.frame(&f, &i).frosted_cards.is_empty());

            app.set_wallpaper(Some((image.clone(), 65)));
            app.open(Screen::Install);
            assert!(!app.frame(&f, &i).frosted_cards.is_empty());
            assert!(app.theme().pal.card.a < 255);
            app.open(Screen::Settings);
            assert_eq!(app.frame(&f, &i).frosted_cards.len(), 7);
        }
    }

    #[test]
    fn wallpaper_and_glass_follow_the_user_across_recovery_pages() {
        let (f, i) = (fonts(), info());
        let pages = [
            Screen::Home, Screen::Settings, Screen::Advanced, Screen::Install,
            Screen::Reboot, Screen::Backup, Screen::Mount, Screen::Wallpaper,
            Screen::Files, Screen::Search, Screen::Terminal, Screen::About, Screen::Rename,
        ];
        let card_pages = [
            Screen::Home, Screen::Settings, Screen::Advanced, Screen::Install,
            Screen::Reboot, Screen::Backup, Screen::Mount, Screen::Wallpaper, Screen::Rename,
        ];
        let image = test_wallpaper(W, H);
        for dark in [false, true] {
            for page in pages {
                let mut app = App::new(page, W, H, PPD);
                app.enable_wallpaper();
                app.data_waiting(None);
                if page == Screen::Files {
                    app.set_files("/".into(), Ok(vec![entry("example.txt", FileKind::File)]), 0);
                }
                app.state.dark = dark;
                app.set_frosted_cards(true);
                app.set_wallpaper(Some((image.clone(), 100)));
                if card_pages.contains(&page) {
                    assert!(!app.frame(&f, &i).frosted_cards.is_empty(), "{page:?} should have glass cards");
                }
                let mut screen = Canvas::new(W, H);
                app.tick(0);
                app.compose(&mut screen, &f, &i, "12:00", 0);
                assert!(screen.data.iter().skip(3).step_by(4).all(|alpha| *alpha == 255), "{page:?} has transparent pixels");
                let at = (((H / 2) * W + 2) * 4) as usize;
                let wallpaper = screen.data[at..at + 4].to_vec();
                app.set_wallpaper(None);
                app.compose(&mut screen, &f, &i, "12:00", 0);
                assert_ne!(wallpaper, screen.data[at..at + 4], "{page:?} should show the wallpaper");
            }
        }
    }

    #[test]
    fn wallpaper_keeps_every_pixel_opaque_during_search_transitions() {
        let (f, i) = (fonts(), info());
        let mut app = App::new(Screen::Home, W, H, PPD);
        app.enable_wallpaper();
        app.data_waiting(None);
        app.set_wallpaper(Some((test_wallpaper(W, H), 80)));
        app.set_frosted_cards(true);
        let mut screen = Canvas::new(W, H);
        app.tick(0);
        app.compose(&mut screen, &f, &i, "12:00", 0);
        let (x, y) = spot(&mut app, &f, &i, Action::Open(Screen::Search));
        app.touch(Touch::Down(x, y), &f, 10);
        app.touch(Touch::Up, &f, 20);
        for now in [20, 40, 80, 160, 320, 620] {
            app.tick(now);
            app.compose(&mut screen, &f, &i, "12:00", now);
            assert!(screen.data.iter().skip(3).step_by(4).all(|alpha| *alpha == 255), "search at {now} ms");
        }
    }

    #[test]
    fn wallpaper_browses_only_after_owner_unlock_and_never_requests_installation() {
        let (f, i) = (fonts(), info());
        let mut app = App::new(Screen::Settings, W, H, PPD);
        app.enable_wallpaper();
        let wallpaper = Action::Open(Screen::Wallpaper);
        let choose = Action::Open(Screen::PickWallpaper);
        assert!(tap(&mut app, &f, &i, wallpaper).is_empty());
        assert!(!app.frame(&f, &i).hits.iter().any(|hit| hit.action == choose));
        assert_eq!(tap(&mut app, &f, &i, Action::Open(Screen::Decrypt)), vec![Effect::OpenData]);
        app.data_waiting(Some((0, Keys::Digits)));
        assert!(app.back(0).is_empty());
        assert!(!app.frame(&f, &i).hits.iter().any(|hit| hit.action == choose));
        app.data_waiting(None);
        assert_eq!(tap(&mut app, &f, &i, choose), vec![Effect::ReadWallpaperFolder("".into())]);

        let row = |name: &str, kind| FileRow { name: name.into(), name_utf8: true, kind, size: 0, modified: 0, modified_known: true, mode: 0, uid: 0, gid: 0 };
        app.set_wallpaper_folder("".into(), Ok(vec![row("DCIM", FileKind::Folder { items: None })]), 0);
        assert_eq!(tap(&mut app, &f, &i, Action::WallpaperEntry(0)), vec![Effect::ReadWallpaperFolder("DCIM".into())]);
        app.set_wallpaper_folder("DCIM".into(), Ok(vec![row("photo.jpg", FileKind::File)]), 0);
        assert_eq!(app.back(0), vec![Effect::ReadWallpaperFolder("".into())]);
        app.set_wallpaper_folder("DCIM".into(), Ok(vec![row("photo.jpg", FileKind::File)]), 0);
        assert_eq!(tap(&mut app, &f, &i, Action::WallpaperEntry(0)), vec![Effect::PrepareWallpaper("DCIM/photo.jpg".into())]);
        app.wallpaper_prepared(Err("unable to open picture".into()), 0);
        assert_eq!(app.screen(), Screen::PickWallpaper);
        assert!(app.state.wallpaper().is_none());
        assert_eq!(tap(&mut app, &f, &i, Action::WallpaperEntry(0)), vec![Effect::PrepareWallpaper("DCIM/photo.jpg".into())]);
        let mut png = Vec::new();
        test_wallpaper(24, 16).write_png(&mut png).unwrap();
        let crop = WallpaperCrop::decode(std::io::Cursor::new(png), (W, H)).unwrap();
        app.wallpaper_prepared(Ok(crop.clone()), 0);
        assert_eq!(app.screen(), Screen::CropWallpaper);
        assert!(app.state.wallpaper().is_none(), "a selected picture must wait for confirmation");
        assert_eq!(tap(&mut app, &f, &i, Action::ApplyWallpaper), vec![Effect::SaveWallpaper]);
        app.wallpaper_applied(Ok(crop.render()), 0);
        assert_eq!(app.screen(), Screen::Wallpaper);
        assert_eq!(app.wallpaper_opacity(), DEFAULT_OPACITY);
        assert_eq!(tap(&mut app, &f, &i, Action::ResetWallpaper), vec![Effect::ResetWallpaper]);
    }

    #[test]
    fn wallpaper_crop_drags_from_the_dimmed_area_and_back_discards_the_draft() {
        let (f, i) = (fonts(), info());
        let (w, h) = (360, 800);
        let mut app = App::new(Screen::Wallpaper, w, h, 1.0);
        app.enable_wallpaper();
        app.data_waiting(None);
        let original = test_wallpaper(w, h);
        app.set_wallpaper(Some((original.clone(), 65)));
        assert_eq!(
            tap(&mut app, &f, &i, Action::Open(Screen::PickWallpaper)),
            vec![Effect::ReadWallpaperFolder(String::new())]
        );

        let mut source = Canvas::filled(96, 32, Rgba::rgb(240, 20, 50));
        source.fill_rect(32.0, 0.0, 64.0, 32.0, Rgba::rgb(18, 90, 235));
        let mut png = Vec::new();
        source.write_png(&mut png).unwrap();
        app.wallpaper_prepared(Ok(WallpaperCrop::decode(std::io::Cursor::new(png), (w, h)).unwrap()), 0);
        let (viewport, frame) = app.frame(&f, &i).wallpaper_preview.unwrap();
        let (x, y) = frame.centre();
        let outside = frame.x - 4.0;
        assert!(viewport.contains(outside, y));
        let index = |x: f32| ((y as u32 * w + x as u32) * 4) as usize;
        let at = index(x);
        let edge = index(outside);
        let mut screen = Canvas::new(w, h);
        app.compose(&mut screen, &f, &i, "12:00", 1000);
        assert_eq!(&screen.data[at..at + 4], &[18, 90, 235, 255]);
        assert_eq!(&screen.data[edge..edge + 4], &[10, 49, 129, 255]);
        assert!(app.touch(Touch::Down(outside, y), &f, 1001).is_empty());
        assert!(app.touch(Touch::Move(outside + 2000.0, y), &f, 1002).is_empty());
        assert!(app.touch(Touch::Up, &f, 1003).is_empty());
        app.compose(&mut screen, &f, &i, "12:00", 1004);
        assert_eq!(&screen.data[at..at + 4], &[240, 20, 50, 255]);
        let bg = app.theme().pal.bg;
        assert_eq!(&screen.data[edge..edge + 4], &[bg.r, bg.g, bg.b, 255]);

        assert!(app.back(1005).is_empty());
        assert_eq!(app.screen(), Screen::PickWallpaper);
        assert!(app.wallpaper_crop().is_none());
        assert_eq!(app.state.wallpaper(), Some(&original));
        assert_eq!(app.wallpaper_opacity(), 65);
    }

    #[test]
    fn wallpaper_slider_previews_during_a_drag_and_saves_once_on_release() {
        let (f, i) = (fonts(), info());
        let mut app = App::new(Screen::Wallpaper, W, H, PPD);
        app.enable_wallpaper();
        app.data_waiting(None);
        app.set_wallpaper(Some((test_wallpaper(W, H), DEFAULT_OPACITY)));
        let mut screen = Canvas::new(W, H);
        app.compose(&mut screen, &f, &i, "12:00", 0);
        let before = screen.data.clone();
        let track = app.frame(&f, &i).hits.iter().find(|h| h.action == Action::Slider(Slider::WallpaperOpacity)).unwrap().rect;
        let (_, y) = track.centre();
        assert!(app.touch(Touch::Down(track.x + 1.0, y), &f, 0).is_empty());
        app.compose(&mut screen, &f, &i, "12:00", 1);
        assert_eq!(app.wallpaper_opacity(), 5);
        let theme = app.theme();
        let x = (W / 2) as usize;
        let y_sample = (theme.m.content_top + theme.m.dp(55.0)).round() as usize;
        let at = (y_sample * W as usize + x) * 4;
        assert_ne!(&screen.data[at..at + 4], &before[at..at + 4]);
        assert!(app.touch(Touch::Move(track.x + track.w - 1.0, y), &f, 2).is_empty());
        assert_eq!(app.touch(Touch::Up, &f, 3), vec![Effect::SaveWallpaperOpacity { before: DEFAULT_OPACITY, after: 100 }]);
        assert_eq!(app.wallpaper_opacity(), 100);
        let _ = app.frame(&f, &i);
        assert!(app.touch(Touch::Down(track.x + 1.0, y), &f, 4).is_empty());
        assert_eq!(app.touch(Touch::Up, &f, 5), vec![Effect::SaveWallpaperOpacity { before: 100, after: 5 }]);
    }

    #[test]
    fn wallpaper_background_follows_the_theme_and_disappears_while_locked() {
        let (f, i) = (fonts(), info());
        let mut app = App::new(Screen::Home, W, H, PPD);
        app.enable_wallpaper();
        let mut screen = Canvas::new(W, H);
        let bg = |canvas: &Canvas| canvas.data[..4].to_vec();
        app.set_wallpaper(Some((test_wallpaper(W, H), 100)));
        app.compose(&mut screen, &f, &i, "12:00", 0);
        let plain_light = bg(&screen);
        app.data_waiting(None);
        app.compose(&mut screen, &f, &i, "12:00", 1);
        let wallpaper_light = bg(&screen);
        assert_ne!(wallpaper_light, plain_light);
        assert_eq!(wallpaper_light[3], 255);
        app.state.dark = true;
        app.refresh();
        app.compose(&mut screen, &f, &i, "12:00", 2);
        let wallpaper_dark = bg(&screen);
        assert!(wallpaper_dark[0] < wallpaper_light[0]);
        app.data_failed("closed".into());
        app.compose(&mut screen, &f, &i, "12:00", 3);
        assert_eq!(bg(&screen), vec![0, 0, 0, 255]);
    }
}
