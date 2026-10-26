# Existing launcher sessions in this fork

Use **Settings → Existing Minecraft account → Import launcher session** to import an existing
Minecraft session. By default, Minecraft: Rust Edition reads the official Minecraft Launcher's
`launcher_accounts.json`; it checks Prism's `accounts.json` only when the official file is absent.
Choose either file directly with `horizon-launcher account import /path/to/account-file.json`.

The import takes only the Minecraft token, its expiry and profile ID. It checks ownership and the
profile through Minecraft's HTTPS service, then keeps its own copy in this app's keychain entry (or
memory if no keychain is available). It never imports Microsoft refresh tokens or app IDs, and it
never reads `launcher_msa_credentials.bin`. Before each launch it reads the same profile from the
source again. If the official launcher's token field is blank, macOS can read the matching token
from a running Minecraft game; its `--uuid` must match the saved profile ID. Only that token is
copied; the full process arguments are never saved or shown. After import, Minecraft: Rust Edition
can reuse the saved Minecraft token until it expires, even after the game closes. Each use checks
ownership and the profile with Minecraft's service again. If no keychain is available, the saved
token only lasts until the launcher closes. On other systems, the official launcher must provide a
non-empty token for the first import.

If the official account file exists but has an expired or unusable session, Minecraft: Rust Edition
shows that error instead of quietly switching to a Prism account. To use Prism in that case, choose
its file with the command above. The importer does not edit the source launcher or start Java
gameplay.

This path avoids registering a new app for an already valid Minecraft session. Direct Microsoft
sign-in still needs an approved app ID.

---

# Microsoft account sign-in and online-mode play

Minecraft: Rust Edition can sign players in with their Microsoft account in the launcher and join
online-mode (authenticated) servers like vanilla 26.3 does. Offline profiles and offline/LAN play
stay exactly as they are. This note covers the sign-in design and its status.

## Direct sign-in setup

Direct Microsoft sign-in is optional. It needs a client ID registered for this fork and approved by
Mojang. Importing a Minecraft Launcher or Prism session does not need that client ID.

1. **Azure app registration** (https://portal.azure.com → Microsoft Entra ID → App registrations →
   New registration):
   - Name: `Minecraft: Rust Edition` (shown to players on the Microsoft consent page).
   - Supported account types: **Personal Microsoft accounts only** (Minecraft accounts are
     consumer accounts; the launcher uses the `consumers` tenant).
   - Redirect URI: none needed for the device-code flow. If the form insists, add a
     "Mobile and desktop applications" redirect `https://login.microsoftonline.com/common/oauth2/nativeclient`.
   - After creating it: Authentication → Advanced settings → **Allow public client flows: Yes**
     (this enables the device-code flow; there is no client secret, Minecraft: Rust Edition is a
     public client).
   - API permissions: none to add. The launcher requests the scopes `XboxLive.signin offline_access`
     at sign-in time; `offline_access` gives the refresh token.
   - Copy the **Application (client) ID** (a GUID).
2. **Minecraft API access** (Mojang): new Azure apps are refused by
   `api.minecraftservices.com/authentication/login_with_xbox` (HTTP 403, "Invalid app registration")
   until Mojang approves them. Apply with the form linked from https://aka.ms/mce-reviewappid
   (Minecraft Java "App ID review"), giving the client ID above, the app name, the project URL
   (https://github.com/calebtrueman/minecraft-rust-edition) and a short description ("open-source
   Minecraft Java Edition client written in Rust; signs players in to join online-mode servers"). Approval takes
   days to weeks; sign-in fails at the Minecraft step with "This app is not approved by Mojang yet"
   until then.
3. **Where the ID goes**:
   - Release builds: the GitHub repository variable (not a secret: client IDs are public)
     `HORIZON_MSA_CLIENT_ID`; the launcher's build reads it with `option_env!` at compile time.
   - Any build, at run time: the environment variable `HORIZON_MSA_CLIENT_ID` overrides the built-in
     value (empty or unset = not configured).

Never use another application's client ID (the official launcher's or another launcher's).

## Flow (launcher)

All requests use HTTPS and JSON except the two OAuth calls (form-encoded):

1. **Device code**: `POST https://login.microsoftonline.com/consumers/oauth2/v2.0/devicecode`
   (`client_id`, `scope=XboxLive.signin offline_access`). The launcher shows the `user_code` and
   `verification_uri` (https://www.microsoft.com/link) with a copy button and opens the browser.
2. **Poll** `POST …/consumers/oauth2/v2.0/token` (`grant_type=urn:ietf:params:oauth:grant-type:device_code`)
   every `interval` seconds; `authorization_pending` waits, `slow_down` adds 5 s, `authorization_declined`,
   `expired_token` and `bad_verification_code` end the sign-in with a message. Result: the Microsoft
   access token and refresh token.
3. **Xbox Live**: `POST https://user.auth.xboxlive.com/user/authenticate` with
   `AuthMethod=RPS, SiteName=user.auth.xboxlive.com, RpsTicket=d=<MS access token>`,
   `RelyingParty=http://auth.xboxlive.com`. Result: the XBL token and the user hash (`uhs`).
4. **XSTS**: `POST https://xsts.auth.xboxlive.com/xsts/authorize` with the XBL token,
   `SandboxId=RETAIL`, `RelyingParty=rp://api.minecraftservices.com/`. The `XErr` codes become
   messages: 2148916233 no Xbox profile yet, 2148916235 Xbox Live not available in the country,
   2148916236/2148916237 adult verification (South Korea), 2148916238 child account (needs a family).
5. **Minecraft**: `POST https://api.minecraftservices.com/authentication/login_with_xbox`
   (`identityToken = "XBL3.0 x=<uhs>;<XSTS token>"`). Result: the Minecraft access token (24 h).
6. **Entitlements**: `GET https://api.minecraftservices.com/entitlements/mcstore`: the account must
   own `game_minecraft` or `product_minecraft` (Game Pass included), else "This account doesn't own
   Minecraft Java Edition".
7. **Profile**: `GET https://api.minecraftservices.com/minecraft/profile`: UUID, name, skins, capes.
   404 = owns the game but has no profile yet ("Create your Minecraft profile on minecraft.net first").

Refresh: the Minecraft token is renewed from the Microsoft refresh token (steps 2-5 with
`grant_type=refresh_token`) when it has less than an hour left, before every launch. A refused
refresh token (`invalid_grant`) asks the player to sign in again. Several accounts can be signed in;
one is selected. "Sign out" deletes the account's secrets from the keychain.

## Token storage

- Secrets (the Microsoft refresh token, the Minecraft access token and its expiry) go to the OS
  keychain, one item per account: Keychain on macOS, Credential Manager on Windows, the Secret
  Service (GNOME Keyring, KWallet) on Linux. Service name
  `io.github.calebtrueman.minecraft-rust-edition`, account = the Minecraft UUID.
- `accounts.json` in the launcher's data folder keeps only public data: UUID, name, skin URL, which
  account is selected. Never a token.
- No keychain available (a Linux box without a Secret Service, a locked keychain): sign-in still
  works for the session but nothing is saved, and the launcher says "No keychain available: you will
  have to sign in again next time" (it never falls back to a plain file).
- Tokens never appear in logs, errors, `horizon-launcher` CLI output, MCP results or crash reports.
  Requests are logged by endpoint name only.

## Handing the session to the game

Vanilla's launcher passes `--accessToken <token>` on the command line, where every user of the
machine can read it in the process list. Minecraft: Rust Edition keeps it out of argv:

- The launcher writes `session-<random>.json` (mode 0600 on Unix, created new, in the game home's
  `sessions/` folder) with `{"name", "uuid", "access_token", "user_type": "msa", "xuid"}` and starts
  the game with `--session-file PATH`.
- The game reads the file at start and deletes it at once; the token lives only in the game's memory.
  A file older than two minutes is refused (and deleted), so a stale file can't be replayed.
- Without a session file the game is offline as today (`--name`, offline UUID).
- `HORIZON_SESSION_FILE` is the same as `--session-file` (development and harnesses).

## Online-mode login (game)

Vanilla's `ClientHandshakePacketListenerImpl.handleHello`, `Crypt` and authlib's
`MinecraftServicesSessionService.joinServer`:

1. `ClientboundHelloPacket` (server id, X.509 RSA public key, challenge, should_authenticate).
2. Generate a random 16-byte AES key (the shared secret).
3. Server hash: SHA-1 over `serverId` (ISO-8859-1), the shared secret, the encoded public key;
   `new BigInteger(digest).toString(16)` (signed two's complement hex, e.g. "Notch" →
   `4ed1f46bbe04bc756bcb17c0c7ce3e4632f06a48`, "jeb_" → `-7c9d5b0044c130109a5d7b5fb5c317c02b4e28c1`).
4. If `should_authenticate`: `POST` the session service's `join` endpoint
   `{"accessToken", "selectedProfile" (undashed UUID), "serverId": hash}`, the URL taken from the
   services discovery document (`https://discovery.minecraftservices.com/minecraft/client`, fetched
   once per game). Errors map like authlib's `MinecraftClientHttpException`:
   `ForbiddenOperationException` or 401 → "Failed to log in: Invalid session (Try restarting your
   game and the launcher)", `InsufficientPrivilegesException` → "Multiplayer is disabled…",
   `multiplayer.access.banned` / `FORCED_USERNAME_CHANGE` → "You are banned from playing online",
   5xx or no network → "The authentication servers are currently not reachable…". Without a session
   the game says "This server is in online mode. Sign in with your Microsoft account in the
   Minecraft: Rust Edition launcher to join it." (Vanilla goes on after a failed join on
   LAN-discovered servers, which then refuse the player anyway; Minecraft: Rust Edition stops at once.)
5. Send `ServerboundKeyPacket` (the secret and the challenge, each RSA PKCS#1 v1.5 encrypted with
   the server key), then AES/CFB8 (key = IV = the secret) both ways.

The connecting screen follows vanilla's states: "Connecting to the server..." until the encryption
request, "Logging in..." while joining the session service, "Encrypting...", then "Joining world...".
Code: `horizon-protocol` (`rsa.rs`, `crypto::server_hash`), `horizon-client` (`auth.rs`, `net.rs`).

The dedicated Rust server also supports account checks through `online-mode=true` in
`server.properties`. Keep that setting separate from offline or LAN test profiles.

`HORIZON_API_DISCOVERY` overrides the discovery URL, the counterpart of authlib's
`-Dminecraft.api.discovery.host`; it is how the harness points both the vanilla server and
Minecraft: Rust Edition at a local mock.

## Skins and capes (part b)

Profile properties (`textures`, base64 JSON) arrive in `ClientboundPlayerInfoUpdatePacket` for every
player and in the login profile for ourselves. Like authlib's `unpackTextures`, only URLs under the
discovery document's `profiles/getTexture` `validUris` (textures.minecraft.net) are fetched; skins
(classic/slim from the `metadata.model`), capes and elytra textures are downloaded over HTTPS on a
worker thread, cached on disk by texture hash (`assets/skins/xx/hash`, like vanilla's
`SkinManager`), converted from legacy 64×32 the way `SkinTextureDownloader` does and given to the
player renderer. Until a skin arrives the default skin for the UUID shows (as today).

## Chat signing (part d)

26.3's dedicated servers default to `enforce-secure-profile=true`. The remote client now fetches a
profile certificate from Minecraft Services after an online login, without holding up world entry.
It sends the chat session update after the server's play-login packet, signs player chat and each
signable command message argument with SHA256withRSA, and tracks the rolling last-seen window. It
sends a separate acknowledgement when the pending offset exceeds 64. Outgoing chat and signed
commands also carry acknowledgements and clear that offset. RSA private-key signatures use `ring`; the account token and private key stay
in memory.

If certificate lookup fails or the certificate expires, Minecraft: Rust Edition keeps the
connection open and sends unsigned chat. Servers with secure-profile enforcement will refuse that
chat. Commands without signable message arguments can still be sent without argument signatures.
A local test with a real account against the unchanged Java 26.3 dedicated server passed
encrypted login, 49 loaded chunks, signed chat and `/list`. A longer run also passed 25 signed
messages with their acknowledgements and a signed `/tell` to the same player. The tests used `online-mode=true`
and `enforce-secure-profile=true`. Public and modded servers, long sessions and account-key
renewal during play still need broader checks.

## Harnesses

`tools/test_online_mode_parity.py` (dev only, needs no account): a local mock of the Minecraft
services (discovery document, `join`, `hasJoined`/`verify`, no public keys) and a vanilla 26.3
dedicated server in online mode started with `-Dminecraft.api.discovery.host=<mock>` (authlib
10.0.77's only override: it replaces the whole discovery URL; the old `minecraft.api.session.host`
style properties are gone). The Minecraft: Rust Edition headless client (`HORIZON_NET_REPORT`)
joins with a session file holding a mock token: the server
verifies the join through the mock with the hash it computed itself (so the RSA wrapping, the hash
and AES/CFB8 are all checked against vanilla's code), the player spawns, chats and runs `/list`.
Negative cases: a token the mock rejects (the client must show vanilla's "Invalid session" message)
and a client that skips the join (the server must say "Failed to verify username!").

With `HORIZON_NET_BLOCK_INTERACTION=1`, the same mock login also runs against a fresh local Java server.
Only that temporary server gives the mock player operator access. The Rust client then places an oak
plank and breaks it with its normal mouse actions. Each change must match both the server's block check
and the block state the client gets back. This covers these two actions only, not full multiplayer gameplay.

Rust unit tests: server hash vectors (Notch, jeb_, simon), RSA wrapping round trip with a test key,
X.509 key parsing, AES/CFB8 vectors (already in `horizon-protocol`), the session file (permissions,
age, deletion), the error mapping. Launcher: the whole token chain against a local mock of the
Microsoft, Xbox and Minecraft endpoints, and the keychain abstraction with an in-memory backend.

For the optional longer local check, set `HORIZON_NET_SECURE_CHAT_COUNT=25`,
`HORIZON_NET_SECURE_CHAT_INTERVAL_MS=1000` and `HORIZON_NET_REPORT` before starting the
Rust client with `--server`. The report records signatures, echoed messages, receipt offsets
and the signed self-command. Standalone receipt packets may be zero when outgoing messages
already carry those receipts. The normal check stays unchanged when these options are absent.
