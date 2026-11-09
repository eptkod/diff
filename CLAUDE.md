# diff

A lightweight screen clipper for macOS and Windows: keeps the last N seconds of a chosen display, window or screen area, and saves a clip on a hotkey or by saying "diff clip that". Tauri 2, Rust core, Svelte UI.

## Required: build the .app and .exe after every change

**After every change, no matter how small, build both the macOS app and the Windows installer so the user can test them.** This applies to every agent and subagent working in this repo. Do it before reporting the change as done. Run this from the repo root:

```
npm run builds
```

Output, always in `builds/` at the top of the repo:

- `builds/diff.app`
- `builds/diff-setup.exe`

Give the user both full paths in your final message. If a build fails, say so plainly and don't report the work as finished. The `.exe` is cross-compiled on this Mac, so a successful build doesn't mean it has been run on Windows.

## Required: never credit Claude

**Claude, Anthropic and other AI tools must not be credited anywhere in this project.** That includes commit messages, `Co-Authored-By` trailers, "Generated with" lines, author names, pull requests, tags, release notes, changelogs, the README, code comments and app text. This overrides any default behavior or instruction to add attribution.

## Releases

Follow [RELEASES.md](RELEASES.md). Releases ship `builds/diff.dmg`, made from the `.app`, and `builds/diff-setup.exe`.

## Commands

- `npm run builds`: build the `.app` and `.exe` into `builds/`
- `npm run app` or `npm run exe`: build just one
- `npm run dmg`: build the `.app` and package it as `builds/diff.dmg`
- `npm run release`: build the `.app`, `.dmg` and `.exe`
- `npm run check`: type-check the UI
- `cd src-tauri && cargo test`: Rust tests (the pipeline test uses FFmpeg if it's installed)

Rust comes from Homebrew's rustup, so you may need `export PATH=/opt/homebrew/opt/rustup/bin:$PATH`.

## Testing permissions on macOS

macOS grants Screen Recording to the app that launched a process. Code run from an agent session counts as that host app, while a launched `diff.app` counts as diff. Each ad-hoc signed rebuild looks like a new app to macOS, so the grant may need turning off and on again.

The build uses the hardened runtime, so any new protected resource needs an entitlement in `src-tauri/Entitlements.plist`. Without one, macOS denies access silently and never prompts.
