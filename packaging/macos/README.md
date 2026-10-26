# macOS package

`package.sh BINARY VERSION OUTDIR` makes a Minecraft: Rust Edition app and two downloads:

- `Minecraft Rust Edition.app`, with the launcher, its app settings, and icon.
- `horizon-launcher-macos-arm64.dmg`, the drag-and-drop installer.
- `horizon-launcher-macos-arm64.zip`, used for launcher updates.

The DMG background is drawn from `background.html`; `dmgbuild` writes the Finder layout without
opening a desktop window. The launcher can replace its own app bundle during an update. A bare
launcher binary can update only itself.

## Signing

The package script ad hoc-signs and checks the app by default. Ad hoc signatures identify a
consistent local build, but they do not identify a developer. Gatekeeper may ask the player to
approve a downloaded app the first time it opens.

For a public release, set `MACOS_SIGN_IDENTITY` to a Developer ID certificate you control. Set
`MACOS_NOTARY_PROFILE` to your own `notarytool` profile to notarize and staple the app and DMG.
This fork does not include or use another project's signing identity or credentials. The
launcher still checks downloaded update signatures; packaging does not bypass those checks.

Example local package command:

```sh
cargo build --manifest-path launcher/Cargo.toml --release --locked
packaging/macos/package.sh launcher/target/release/horizon-launcher 0.2.0 out
```

## Gatekeeper with an ad hoc build

After downloading an ad hoc build, macOS may block it the first time:

- macOS 14 and earlier: Control-click the app in Applications, choose **Open**, then confirm.
- macOS 15 and later: open it once, then use **System Settings › Privacy & Security › Open Anyway**.

After the first approval, it opens normally. Launcher updates are not downloaded through a
browser, so they do not receive the browser's quarantine flag.
