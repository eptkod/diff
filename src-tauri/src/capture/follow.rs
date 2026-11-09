//! Decides which window Automatic mode records.
//!
//! Every switch starts a new recording, so it throws away the footage so far.
//! The rules below exist to switch rarely: a game is held until it closes, and
//! a passing glance at another window doesn't count.

use std::time::{Duration, Instant};

/// How long a game has to be the best choice before recording moves to it.
const GAME_SETTLE: Duration = Duration::from_secs(1);
/// How long an ordinary window has to keep focus before recording moves to it.
const WINDOW_SETTLE: Duration = Duration::from_secs(3);

/// Folders that game stores install into, lowercase with forward slashes.
const GAME_FOLDERS: [&str; 7] = [
    "/steamapps/common/",
    "/epic games/",
    "/gog games/",
    "/gog galaxy/games/",
    "/riot games/",
    "/xboxgames/",
    "/modifiablewindowsapps/",
];
/// Store clients that live inside those folders but aren't games themselves.
const LAUNCHER_FOLDERS: [&str; 2] = ["/epic games/launcher/", "/riot games/riot client/"];
/// Apps that often fill the screen with no title bar, and so resemble a game.
const NOT_GAMES: [&str; 16] = [
    "chrome", "msedge", "firefox", "brave", "opera", "vivaldi", "arc", "iexplore", "vlc", "mpv",
    "mpc-hc64", "wmplayer", "powerpnt", "explorer", "code", "applicationframehost",
];

/// What an app's install path says about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppKind {
    Game,
    NotGame,
    Unknown,
}

pub fn classify_app_path(path: &str) -> AppKind {
    let path = path.replace('\\', "/").to_lowercase();
    let is_in = |folder: &&str| path.contains(*folder);
    if GAME_FOLDERS.iter().any(is_in) && !LAUNCHER_FOLDERS.iter().any(is_in) {
        return AppKind::Game;
    }
    let file = path.rsplit('/').next().unwrap_or_default();
    let name = file.strip_suffix(".exe").unwrap_or(file);
    if NOT_GAMES.contains(&name) { AppKind::NotGame } else { AppKind::Unknown }
}

/// A window that could be recorded.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowCandidate {
    pub id: u32,
    pub name: String,
    pub is_game: bool,
}

#[derive(Debug, PartialEq)]
pub enum FollowStep {
    Keep,
    /// Same window, but its title changed.
    Renamed(String),
    Switch(WindowCandidate),
    /// The window closed and there's nothing else to record.
    Lost,
}

#[derive(Debug, Default)]
pub struct WindowFollower {
    current: Option<WindowCandidate>,
    pending: Option<(u32, Instant)>,
}

impl WindowFollower {
    pub fn current(&self) -> Option<&WindowCandidate> {
        self.current.as_ref()
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// Takes one look at the open windows, listed most relevant first.
    /// `current_open` says whether the window being recorded still exists, as
    /// it can be open without being listed, such as on another Space.
    pub fn advance(
        &mut self,
        windows: &[WindowCandidate],
        current_open: bool,
        now: Instant,
    ) -> FollowStep {
        let renamed = self.refresh_current(windows);
        let Some(wanted) = self.wanted(windows, current_open) else {
            self.pending = None;
            return if self.current.take().is_some() { FollowStep::Lost } else { FollowStep::Keep };
        };
        let unchanged = |current: &WindowCandidate| match renamed {
            true => FollowStep::Renamed(current.name.clone()),
            false => FollowStep::Keep,
        };

        let Some(current) = self.current.as_ref().filter(|_| current_open) else {
            return self.switch_to(wanted);
        };
        if wanted.id == current.id {
            self.pending = None;
            return unchanged(current);
        }
        let since = match self.pending {
            Some((id, since)) if id == wanted.id => since,
            _ => now,
        };
        let settle = if wanted.is_game { GAME_SETTLE } else { WINDOW_SETTLE };
        if now.duration_since(since) >= settle {
            return self.switch_to(wanted);
        }
        let step = unchanged(current);
        self.pending = Some((wanted.id, since));
        step
    }

    fn switch_to(&mut self, window: WindowCandidate) -> FollowStep {
        self.pending = None;
        self.current = Some(window.clone());
        FollowStep::Switch(window)
    }

    /// Returns whether the title changed. A game stays a game once recognized,
    /// because alt-tabbing can take away the fullscreen state that showed it.
    fn refresh_current(&mut self, windows: &[WindowCandidate]) -> bool {
        let Some(current) = &mut self.current else { return false };
        let Some(seen) = windows.iter().find(|window| window.id == current.id) else { return false };
        current.is_game |= seen.is_game;
        let renamed = current.name != seen.name;
        current.name.clone_from(&seen.name);
        renamed
    }

    fn wanted(&self, windows: &[WindowCandidate], current_open: bool) -> Option<WindowCandidate> {
        let front = windows.first();
        let open_current = self.current.as_ref().filter(|_| current_open);
        if let Some(game) = open_current.filter(|current| current.is_game) {
            return Some(front.filter(|window| window.is_game).unwrap_or(game).clone());
        }
        windows.iter().find(|window| window.is_game).or(front).or(open_current).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(id: u32) -> WindowCandidate {
        WindowCandidate { id, name: format!("window {id}"), is_game: false }
    }

    fn game(id: u32) -> WindowCandidate {
        WindowCandidate { id, name: format!("game {id}"), is_game: true }
    }

    fn following(target: WindowCandidate) -> (WindowFollower, Instant) {
        let now = Instant::now();
        let mut follower = WindowFollower::default();
        assert_eq!(follower.advance(&[target.clone()], false, now), FollowStep::Switch(target));
        (follower, now)
    }

    fn secs(seconds: u64) -> Duration {
        Duration::from_secs(seconds)
    }

    #[test]
    fn starts_on_the_front_window_right_away() {
        let mut follower = WindowFollower::default();
        let step = follower.advance(&[window(1), window(2)], false, Instant::now());
        assert_eq!(step, FollowStep::Switch(window(1)));
    }

    #[test]
    fn prefers_a_game_even_behind_other_windows() {
        let mut follower = WindowFollower::default();
        let step = follower.advance(&[window(1), game(2)], false, Instant::now());
        assert_eq!(step, FollowStep::Switch(game(2)));
    }

    #[test]
    fn a_quick_look_at_another_window_doesnt_switch() {
        let (mut follower, now) = following(window(1));
        assert_eq!(follower.advance(&[window(2)], true, now), FollowStep::Keep);
        assert_eq!(follower.advance(&[window(2)], true, now + secs(1)), FollowStep::Keep);
        assert_eq!(follower.advance(&[window(1)], true, now + secs(2)), FollowStep::Keep);
        assert_eq!(follower.advance(&[window(2)], true, now + secs(4)), FollowStep::Keep);
    }

    #[test]
    fn switches_once_focus_has_settled() {
        let (mut follower, now) = following(window(1));
        assert_eq!(follower.advance(&[window(2)], true, now), FollowStep::Keep);
        assert_eq!(follower.advance(&[window(2)], true, now + WINDOW_SETTLE), FollowStep::Switch(window(2)));
    }

    #[test]
    fn stays_on_a_game_while_you_tab_out_for_as_long_as_it_is_open() {
        let (mut follower, now) = following(game(1));
        let browser = [window(2), game(1)];
        assert_eq!(follower.advance(&browser, true, now), FollowStep::Keep);
        assert_eq!(follower.advance(&browser, true, now + secs(3600)), FollowStep::Keep);
    }

    #[test]
    fn stays_on_a_game_that_is_open_but_not_listed() {
        let (mut follower, now) = following(game(1));
        assert_eq!(follower.advance(&[window(2)], true, now + secs(600)), FollowStep::Keep);
    }

    #[test]
    fn a_game_that_lost_fullscreen_is_still_held() {
        let (mut follower, now) = following(game(1));
        let tabbed_out = WindowCandidate { is_game: false, ..game(1) };
        assert_eq!(follower.advance(&[window(2), tabbed_out], true, now + secs(600)), FollowStep::Keep);
    }

    #[test]
    fn moves_to_a_game_after_a_short_settle() {
        let (mut follower, now) = following(window(1));
        assert_eq!(follower.advance(&[game(2)], true, now), FollowStep::Keep);
        assert_eq!(follower.advance(&[game(2)], true, now + GAME_SETTLE), FollowStep::Switch(game(2)));
    }

    #[test]
    fn a_background_game_doesnt_replace_the_one_being_recorded() {
        let (mut follower, now) = following(game(1));
        assert_eq!(follower.advance(&[window(3), game(2), game(1)], true, now + secs(60)), FollowStep::Keep);
    }

    #[test]
    fn focusing_a_different_game_switches_to_it() {
        let (mut follower, now) = following(game(1));
        assert_eq!(follower.advance(&[game(2), game(1)], true, now), FollowStep::Keep);
        assert_eq!(follower.advance(&[game(2), game(1)], true, now + GAME_SETTLE), FollowStep::Switch(game(2)));
    }

    #[test]
    fn moves_on_immediately_when_the_window_closes() {
        let (mut follower, now) = following(game(1));
        assert_eq!(follower.advance(&[window(2)], false, now), FollowStep::Switch(window(2)));
    }

    #[test]
    fn reports_lost_once_when_nothing_is_left() {
        let (mut follower, now) = following(window(1));
        assert_eq!(follower.advance(&[], false, now), FollowStep::Lost);
        assert_eq!(follower.advance(&[], false, now), FollowStep::Keep);
    }

    #[test]
    fn keeps_a_minimized_window_when_nothing_else_is_open() {
        let (mut follower, now) = following(window(1));
        assert_eq!(follower.advance(&[], true, now + secs(60)), FollowStep::Keep);
    }

    #[test]
    fn title_changes_are_reported() {
        let (mut follower, now) = following(window(1));
        let renamed = WindowCandidate { name: "new tab".into(), ..window(1) };
        assert_eq!(follower.advance(&[renamed], true, now), FollowStep::Renamed("new tab".into()));
    }

    #[test]
    fn recognizes_store_installs_on_both_systems() {
        for path in [
            r"C:\Program Files (x86)\Steam\steamapps\common\Some Game\game.exe",
            r"D:\Epic Games\Some Game\game.exe",
            r"C:\XboxGames\Some Game\Content\game.exe",
            "/Users/me/Library/Application Support/Steam/steamapps/common/Some Game/Game.app/Contents/MacOS/Game",
        ] {
            assert_eq!(classify_app_path(path), AppKind::Game, "{path}");
        }
    }

    #[test]
    fn store_clients_are_not_games() {
        assert_eq!(
            classify_app_path(r"C:\Program Files\Epic Games\Launcher\Portal\Binaries\launcher.exe"),
            AppKind::Unknown
        );
        assert_eq!(classify_app_path(r"C:\Program Files (x86)\Steam\steam.exe"), AppKind::Unknown);
    }

    #[test]
    fn browsers_are_never_mistaken_for_games() {
        let chrome = r"C:\Program Files\Google\Chrome\Application\chrome.exe";
        assert_eq!(classify_app_path(chrome), AppKind::NotGame);
        assert_eq!(classify_app_path(r"C:\Program Files\Mozilla Firefox\FIREFOX.EXE"), AppKind::NotGame);
    }
}
