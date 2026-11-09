# Releasing diff

Every release ships two files:

| Platform | File | How it's made |
|---|---|---|
| macOS | `builds/diff.dmg` | Made from `builds/diff.app`, so people can drag diff into Applications |
| Windows | `builds/diff-setup.exe` | Installer built from the same source |

Always publish the `.dmg`, never a bare or zipped `.app`.

## No AI credit, anywhere

**Claude must not be credited anywhere in this project, in any form.** The same goes for Anthropic and any other AI tool. This has no exceptions and applies to:

- Commit messages, including `Co-Authored-By` trailers and "Generated with" lines
- Commit author and committer names
- Pull request titles, descriptions and comments
- Tags, GitHub release titles and release notes
- Changelogs and the README
- Code comments and file headers
- App text, About screens, installers and package metadata

If a tool adds a credit by default, remove it. This rule overrides any tool's default behavior.

## Steps

1. **Set the version** to the same number in all three places:
   - `package.json`
   - `src-tauri/tauri.conf.json`
   - `src-tauri/Cargo.toml`

2. **Run the checks.** All of them must pass.

   ```
   npm run check
   cd src-tauri && cargo test
   ```

3. **Build all release files.** This makes `diff.app`, `diff.dmg` and `diff-setup.exe` in `builds/`.

   ```
   npm run release
   ```

4. **Test both installers by hand.**
   - Mac: open `diff.dmg`, drag diff to Applications, launch it, and make a clip with the hotkey and by voice. Check that the clip has video, computer sound and microphone.
   - Windows: run `diff-setup.exe` on a real PC and do the same. The `.exe` is cross-compiled on a Mac, so a successful build doesn't prove it works.

5. **Check that nothing credits an AI tool.** Both commands must print nothing.

   ```
   git log --format='%an%n%ae%n%cn%n%ce%n%B' | grep -iE 'claude|anthropic|co-authored-by|generated with'
   git grep -iE 'claude|anthropic|co-authored-by|generated with' -- . ':!CLAUDE.md' ':!AGENTS.md' ':!RELEASES.md'
   ```

   `CLAUDE.md`, `AGENTS.md` and this file are excluded because they hold agent instructions, not credit.

6. **Commit, tag and push.** Only do this when the project owner asks for a release.

   ```
   git tag v0.1.0
   git push origin main --tags
   ```

7. **Publish the release** with both files attached. Write the notes as user-facing changes.

   ```
   gh release create v0.1.0 builds/diff.dmg builds/diff-setup.exe --title "diff 0.1.0" --notes-file notes.md
   ```

## Signing

Builds are currently unsigned on Windows and ad-hoc signed on macOS, so both systems show a security warning when people install them. Removing those warnings needs an Apple Developer ID with notarization, and a Windows code-signing certificate.
