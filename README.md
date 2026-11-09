# diff

A lightweight screen clipper for macOS and Windows. diff quietly keeps the last few seconds of your screen, and saves a clip when you press a shortcut or say "diff clip that".

- Record a display, a window, or part of the screen
- Automatic window mode picks up the game or app you're using
- Save the last 15 seconds to 5 minutes
- Clip by keyboard shortcut or by voice, with voice detection running on your computer
- Includes computer sound and your microphone
- Browse, play, rename and trim clips in a small library, with stars and filters for name, date and length
- Nothing leaves your computer: no accounts, no uploads

## Install

Get the latest release from the Releases page:

- **macOS:** open `diff.dmg` and drag diff to Applications
- **Windows:** run `diff-setup.exe`

macOS will ask for Screen Recording and Microphone access the first time you use diff.

## Use

Press `⌥⇧C` on Mac or `Alt+Shift+C` on Windows, or say "diff clip that". Clips are saved to `Movies/diff` on Mac and `Videos\diff` on Windows. Find everything else in the menu bar or tray icon.

## Develop

Needs Node, Rust, and on macOS the Xcode Command Line Tools. Building the Windows installer on a Mac also needs `llvm`, `nsis` and `cargo-xwin`.

```
npm install
npm run tauri dev
```

| Command | What it does |
|---|---|
| `npm run builds` | Builds `diff.app` and `diff-setup.exe` into `builds/` |
| `npm run app` / `npm run exe` | Builds just one of them |
| `npm run dmg` | Builds `diff.app` and packages it as `diff.dmg` |
| `npm run release` | Builds all three release files |
| `npm run check` | Type-checks the interface |
| `cd src-tauri && cargo test` | Runs the Rust tests |

See [RELEASES.md](RELEASES.md) for how to publish a release.
