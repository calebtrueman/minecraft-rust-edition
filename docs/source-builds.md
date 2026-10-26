# Try a source build

These builds run the Rust game. They are unfinished test builds, not signed releases.
Fabric and Java mod execution remain incomplete. A successful build does not prove
that the game matches Java Edition on every server.

## From a build artifact

Unpack the archive into a new folder. On macOS, open **Minecraft Rust Edition.app**.
On Windows, open **horizon-launcher.exe**. On Linux, run `./horizon-launcher`.

In the launcher, open **Versions → Choose build folder** and choose the unpacked
folder. After installation, open **Play** and prepare the game files from your
Minecraft installation.

If you prefer the terminal, run these commands from the unpacked folder:

```sh
./horizon-launcher install-local . --tag local-test
./horizon-launcher
```

On Windows PowerShell, use:

```powershell
.\horizon-launcher.exe install-local . --tag local-test
.\horizon-launcher.exe
```

The executable's internal name is still `horizon-launcher`; its window is
Minecraft: Rust Edition.

The local install copies the selected build to the launcher's own data folder.
It does not make it a signed release or change downloaded-release checks.
Select the local build, point preparation at your Minecraft installation, and
prepare its resources. The Minecraft version must match `research-lock.json`.
This source build needs a matching JDK to compile the included original preparation
sources. The game itself runs in Rust. No Minecraft assets or game jars are included.

For online play, use **Import launcher session** in Settings. It reads the official
Minecraft Launcher first, or Prism when no official account file exists. On macOS,
a blank official token can be read from the matching running game. After import, the
saved session can be reused until it expires. To renew it, launch Minecraft through
the source launcher while online again.
This does not make unsupported server features or Java mods work.

## Build the package yourself

```sh
cargo build --profile fast --locked --features horizon-client/mods -p horizon-client --bin horizon-client --bin horizon-ctl -p horizon-prepare --bin horizon-prepare -p horizon-server --bin horizon-server
cargo build --manifest-path launcher/Cargo.toml --release --locked
cargo run --locked -p xtask -- package-source target/fast/horizon-client dist/minecraft-rust-edition
```

On macOS without a working Metal compiler, add `--features gpui/runtime_shaders`
to the launcher build command. The launcher will compile its shaders at startup.

Add `.exe` to the binary path on Windows. Copy the launcher from
`launcher/target/release/` into that package folder. If `CARGO_TARGET_DIR` is set,
both workspaces put their programs under that directory instead.

The manual **Rust Edition source builds** GitHub workflow follows these steps on
Linux, macOS and Windows. It stores artifacts on its run page; it neither creates
a release nor uses the upstream author's private machines, game files or signing key.
