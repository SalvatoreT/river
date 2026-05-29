//! Embedded Freenet node (Android-only at runtime, portable at the
//! type level so host `cargo test` can exercise the no-JNI fallback
//! paths).
//!
//! On Android, `start_embedded_node` spawns a dedicated tokio
//! multi-thread runtime on a background OS thread and drives the
//! freenet node's network-mode event loop on it. The node binds its
//! WebSocket client API at the default `127.0.0.1:7509`; River's
//! `ConnectionManager` (native impl in
//! `freenet_api/connection_manager.rs`) connects to that endpoint.
//!
//! The node is *separate* from the Dioxus runtime — it owns its own
//! tokio reactor so the UI's event loop isn't sharing scheduling
//! pressure with wasmtime contract execution, and so that long-lived
//! node tasks (peer connection recv loops, transport drivers) don't
//! have to be `'static` against the Dioxus scope.
//!
//! **Network mode, not Local.** A Local-mode node only serves what's
//! been PUT to its own stores — Android users could create their own
//! rooms but could not join any room shared via invitation link,
//! because the network state never reaches their device. Network
//! mode makes the device a real Freenet peer that fetches contracts
//! and states through peers and gateways. See
//! `openspec/changes/android-bundled-node/design.md` decision #2 for
//! the full rationale and the mobile-NAT risks.
//!
//! Remaining caveats (tracked in the OpenSpec change's tasks.md):
//! - No foreground service yet — Android may kill the process when
//!   the app backgrounds (tasks 5.x).
//!
//! On non-Android targets, `start_embedded_node` is a no-op stub so
//! the module compiles for host `cargo check` / `cargo test` without
//! pulling in freenet, tokio, jni, or ndk-context.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

/// Tracks whether the Android activity is currently visible to the user.
///
/// Toggled by `MainActivity.onStart` / `onStop` via the
/// `Java_dev_dioxus_main_MainActivity_nativeSetForeground` JNI export
/// below. Read by `notifications::is_document_visible` on Android so
/// the new-message notifier matches the web build's semantics: a room
/// the user is "currently viewing" only suppresses notifications while
/// the app is on-screen. Once the user backgrounds the app (Home, app
/// switcher, screen off) every room — including the one they had open
/// — should fire a notification.
///
/// Default `true` so any code path that runs before the first
/// lifecycle callback (cold start of the embedded node thread, async
/// setup work) treats the app as visible. The lifecycle tick reaches
/// `onStart` within milliseconds of activity creation, so the
/// transient window is benign.
pub static ANDROID_FOREGROUND: AtomicBool = AtomicBool::new(true);

/// Read the foreground flag. Always returns `true` off-Android since
/// host targets have no equivalent visibility model.
pub fn android_is_foreground() -> bool {
    #[cfg(target_os = "android")]
    {
        ANDROID_FOREGROUND.load(Ordering::Acquire)
    }
    #[cfg(not(target_os = "android"))]
    {
        true
    }
}

/// Synthetic auth token registered with the embedded node's
/// `OriginContractMap` at startup, surfaced here so the UI's loopback
/// WebSocket dial can attach it as `?authToken=…`.
///
/// **Why this exists.** The chat-delegate's `check_origin` rejects any
/// `DelegateRequest::ApplicationMessage` whose `MessageOrigin` is
/// `None` with `"missing message origin"`. On the web build the gateway
/// shell injects `window.__FREENET_AUTH_TOKEN__`, and the WS handler
/// looks the token up in a map populated when the shell HTML was served
/// to mint the page's contract origin. On Android the UI loads via wry's
/// custom protocol, never goes through a gateway shell, and would
/// otherwise dial the loopback WS anonymously — every delegate save
/// times out and the room is lost on restart.
///
/// We close that gap by pre-registering a random token under River's
/// **published web-container contract id** (so the attested origin
/// matches what web clients send) in `serve_client_api_with_listener_and_contracts`'s
/// returned map, then publishing the token here for
/// `connection_manager` to pick up. `OnceLock` because the node starts
/// exactly once per process and the token is immutable thereafter.
pub static EMBEDDED_AUTH_TOKEN: OnceLock<String> = OnceLock::new();

/// Per-room snapshot the background notification watcher needs to turn an
/// incoming room-state update into a notification WITHOUT the Dioxus UI
/// being alive.
///
/// Bug B (see `openspec/changes/android-bundled-node/design-background-notifications.md`):
/// the normal notify path (`room_synchronizer` → `notify_new_messages` →
/// `post_message_notification`) runs in the WebView UI, which Android
/// suspends in the background — exactly when notifications matter. The
/// embedded node keeps receiving state on its own runtime, so a watcher
/// running there can post notifications even while the UI is frozen. The
/// only thing the watcher lacks is River-level context (which member is
/// "me", the decrypted room name, and how far the user has already read),
/// so the UI publishes that here whenever it has live `RoomData`.
#[derive(Clone)]
pub struct RoomNotifContext {
    pub owner_vk: ed25519_dalek::VerifyingKey,
    pub self_member_id: river_core::room_state::member::MemberId,
    /// Decrypted display name. Empty if not yet known.
    pub room_name: String,
    /// Room secrets by version, so the watcher can decrypt private message
    /// bodies natively (Phase 2). Empty for public rooms.
    pub secrets: std::collections::HashMap<u32, [u8; 32]>,
    /// Decrypted member nicknames, so the watcher can name the sender
    /// without re-deriving it from `member_info` + secrets per message.
    pub nicknames: std::collections::HashMap<river_core::room_state::member::MemberId, String>,
    /// Read-watermark: max message timestamp (ms since epoch) already
    /// surfaced. Seeded ONCE on first publish (the room's history baseline),
    /// then advanced only by a notification — by the watcher
    /// ([`notif_advance_last_seen`]) or the UI ([`notif_mark_notified`]).
    /// Both paths notify only for messages strictly newer than this, so the
    /// two never double-notify (Phase 2 coordination). NOT advanced by
    /// routine context refreshes, otherwise a refresh landing between a
    /// message's arrival and the watcher processing it would suppress it.
    pub last_seen_ms: u64,
}

/// `None` until first populated; lazily initialised on first write so it
/// can be a plain `const`-constructible static (`HashMap::new` is not
/// const). Keyed by the room owner's `MemberId`.
static NOTIF_CONTEXTS: std::sync::Mutex<
    Option<std::collections::HashMap<river_core::room_state::member::MemberId, RoomNotifContext>>,
> = std::sync::Mutex::new(None);

/// Publish (or refresh) the notification context for a room. Called from
/// the UI wherever it has live `RoomData`.
///
/// `baseline_ms` seeds `last_seen_ms` ONLY on the first observation of a
/// room (its message history at that point). Subsequent calls refresh the
/// name / secrets / nicknames but leave `last_seen_ms` to the notify
/// paths — see [`RoomNotifContext::last_seen_ms`]. A no-op in practice
/// off-Android (the UI only calls this under `cfg(target_os = "android")`),
/// but defined cross-target so the call site type-checks everywhere.
pub fn update_notif_context(
    owner_vk: ed25519_dalek::VerifyingKey,
    self_member_id: river_core::room_state::member::MemberId,
    room_name: String,
    secrets: std::collections::HashMap<u32, [u8; 32]>,
    nicknames: std::collections::HashMap<river_core::room_state::member::MemberId, String>,
    baseline_ms: u64,
) {
    use river_core::room_state::member::MemberId;
    let key = MemberId::from(&owner_vk);
    if let Ok(mut guard) = NOTIF_CONTEXTS.lock() {
        let map = guard.get_or_insert_with(std::collections::HashMap::new);
        match map.get_mut(&key) {
            Some(entry) => {
                entry.owner_vk = owner_vk;
                entry.self_member_id = self_member_id;
                if !room_name.is_empty() {
                    entry.room_name = room_name;
                }
                if !secrets.is_empty() {
                    entry.secrets = secrets;
                }
                if !nicknames.is_empty() {
                    entry.nicknames = nicknames;
                }
                // last_seen_ms intentionally left untouched here.
            }
            None => {
                map.insert(
                    key,
                    RoomNotifContext {
                        owner_vk,
                        self_member_id,
                        room_name,
                        secrets,
                        nicknames,
                        last_seen_ms: baseline_ms,
                    },
                );
            }
        }
    }
}

/// Read a room's read-watermark (0 if unknown). The UI's notify path calls
/// this (keyed by the room owner's verifying key) to skip messages the
/// background watcher has already surfaced.
pub fn notif_watermark(owner_vk: &ed25519_dalek::VerifyingKey) -> u64 {
    use river_core::room_state::member::MemberId;
    let key = MemberId::from(owner_vk);
    NOTIF_CONTEXTS
        .lock()
        .ok()
        .and_then(|g| g.as_ref().and_then(|m| m.get(&key).map(|c| c.last_seen_ms)))
        .unwrap_or(0)
}

/// Advance a room's read-watermark (monotonic). The UI calls this after it
/// posts a notification so the watcher won't re-notify the same message
/// (and vice-versa).
pub fn notif_mark_notified(owner_vk: &ed25519_dalek::VerifyingKey, ts_ms: u64) {
    use river_core::room_state::member::MemberId;
    let key = MemberId::from(owner_vk);
    if let Ok(mut guard) = NOTIF_CONTEXTS.lock() {
        if let Some(map) = guard.as_mut() {
            if let Some(c) = map.get_mut(&key) {
                c.last_seen_ms = c.last_seen_ms.max(ts_ms);
            }
        }
    }
}

/// Milliseconds since the Unix epoch for a `SystemTime` (0 if before epoch).
#[cfg(any(target_os = "android", test))]
fn systime_ms(t: std::time::SystemTime) -> u64 {
    t.duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Pure decision + body composition for a background notification (Bug B
/// Phase 2). Given the freshly-added messages in a room update and the
/// room's notification context, decide whether to notify and render the
/// body EXACTLY as the watcher will post it: `Some((body, max_ts))` or
/// `None` if nothing qualifies. `max_ts` is the newest qualifying message
/// timestamp, used to advance the read-watermark.
///
/// Cross-target and JNI-free so it is unit-tested on the host. Mirrors the
/// UI's `notify_new_messages`: drops self-authored messages and any with
/// timestamp `<= last_seen_ms`, decrypts a single message's body via the
/// UI's shared `get_message_preview`, and falls back to "N new messages"
/// for a batch.
#[cfg(any(target_os = "android", test))]
fn compose_notification(
    msgs: &[river_core::room_state::message::AuthorizedMessageV1],
    self_member_id: river_core::room_state::member::MemberId,
    secrets: &std::collections::HashMap<u32, [u8; 32]>,
    nicknames: &std::collections::HashMap<river_core::room_state::member::MemberId, String>,
    last_seen_ms: u64,
) -> Option<(String, u64)> {
    let mut max_ts = last_seen_ms;
    let mut count = 0u32;
    let mut latest: Option<&river_core::room_state::message::AuthorizedMessageV1> = None;
    for m in msgs {
        if m.message.author == self_member_id {
            continue;
        }
        let ts = systime_ms(m.message.time);
        if ts > last_seen_ms {
            count += 1;
            if ts >= max_ts {
                max_ts = ts;
                latest = Some(m);
            }
        }
    }
    if count == 0 {
        return None;
    }
    let body = if count == 1 {
        match latest {
            Some(m) => {
                let preview = crate::components::app::notifications::get_message_preview(
                    &m.message.content,
                    secrets,
                );
                let sender = nicknames
                    .get(&m.message.author)
                    .cloned()
                    .unwrap_or_else(|| "Someone".to_string());
                format!("{sender}: {preview}")
            }
            None => "New message".to_string(),
        }
    } else {
        format!("{count} new messages")
    };
    Some((body, max_ts))
}

/// Hardcoded fallback for the embedded Freenet node's storage dir.
///
/// Matches the package id in `ui/Dioxus.toml` (`org.freenet.river`).
/// On a real device the runtime path comes from
/// `Context.getFilesDir()` via JNI ([`android_files_dir`]) — this
/// constant is only used when the JNI lookup fails (emulator without
/// an attached Activity, unusual launch path, etc.) so the node can
/// still boot rather than aborting.
pub(crate) const FREENET_DATA_DIR_FALLBACK: &str = "/data/data/org.freenet.river/files/freenet";

/// Resolve the app's private files dir at runtime via JNI.
///
/// Walks the standard Android startup handles:
/// 1. `ndk_context::android_context()` exposes the `JavaVM` + the
///    activity's `Context` jobject, published by ndk-glue / wry / tao
///    during native startup.
/// 2. Attach the current thread to the VM.
/// 3. Call `Context.getFilesDir() -> java/io/File`.
/// 4. Call `File.getAbsolutePath() -> java/lang/String`.
/// 5. Decode the Java string to a Rust `String` and join `"freenet"`
///    onto it, giving the node its own subdirectory under the app's
///    private files area.
///
/// Returns `None` on any JNI failure (ndk-context not populated, VM
/// attach failure, `NoSuchMethodError`) AND on every non-Android
/// target. Callers fall back to [`FREENET_DATA_DIR_FALLBACK`] and
/// emit a `warn!` so the failure is visible in logcat.
#[cfg(target_os = "android")]
pub(crate) fn android_files_dir() -> Option<PathBuf> {
    use jni::objects::{JObject, JString};
    use jni::JavaVM;

    let ctx = ndk_context::android_context();
    let vm = unsafe { JavaVM::from_raw(ctx.vm().cast()) }.ok()?;
    let activity = unsafe { JObject::from_raw(ctx.context().cast()) };

    let mut env = vm.attach_current_thread().ok()?;

    // Context.getFilesDir() -> java/io/File
    let files_dir = env
        .call_method(&activity, "getFilesDir", "()Ljava/io/File;", &[])
        .ok()?
        .l()
        .ok()?;

    // File.getAbsolutePath() -> java/lang/String
    let path_jstr = env
        .call_method(&files_dir, "getAbsolutePath", "()Ljava/lang/String;", &[])
        .ok()?
        .l()
        .ok()?;

    let path_jstr: JString = path_jstr.into();
    let rust_path: String = env.get_string(&path_jstr).ok()?.into();

    Some(PathBuf::from(rust_path).join("freenet"))
}

/// Host stub: no Activity, no JNI, always return `None` so the
/// caller falls back to [`FREENET_DATA_DIR_FALLBACK`].
#[cfg(not(target_os = "android"))]
pub(crate) fn android_files_dir() -> Option<PathBuf> {
    None
}

/// Compute the node's storage dir, with the JNI lookup tried first
/// and a fall-back to the hardcoded package-private path on failure.
pub(crate) fn resolve_data_dir() -> PathBuf {
    android_files_dir().unwrap_or_else(|| PathBuf::from(FREENET_DATA_DIR_FALLBACK))
}

#[cfg(target_os = "android")]
mod android {
    use super::*;
    use std::path::Path;
    use std::sync::Mutex;

    use dioxus::logger::tracing::{error, info, warn};
    // `freenet::client_events` is `pub(crate)`; the public re-exports
    // for `AuthToken` and `ClientId` live in `freenet::dev_tool` (alongside
    // the other types meant for external integration tests). The
    // OriginContract<AuthToken, ContractInstanceId> tuple we construct
    // below is exactly what the existing integration tests use to
    // pre-populate the map, so these are the right entry points.
    use freenet::config::ConfigArgs;
    use freenet::dev_tool::{AuthToken, ClientId};
    use freenet::local_node::{NodeConfig, OperationMode};
    use freenet::server::{serve_client_api_with_listener_and_contracts, OriginContract};
    use freenet_stdlib::prelude::ContractInstanceId;
    use std::str::FromStr;
    use tokio::sync::oneshot;

    /// Base58 contract id of River's published web-container contract
    /// (`raAqMhMG7KUpXBU2SxgCQ3Vh4PYjttxdSWd9ftV7RLv`, the same id
    /// `published-contract/contract-id.txt` records). We attest this id
    /// to the chat delegate as the embedded UI's origin so the
    /// delegate's per-origin storage namespace (`signing_key:{origin}:…`,
    /// `outbound_dms:{origin}:…`, etc.) matches what web clients use —
    /// keeps the namespace coherent if delegate state ever syncs across
    /// devices, and keeps the chat-delegate's `check_origin` guard
    /// satisfied. If the published web-container parameters ever shift
    /// the contract id, update both files in lockstep.
    const WEB_CONTAINER_CONTRACT_ID: &str = "raAqMhMG7KUpXBU2SxgCQ3Vh4PYjttxdSWd9ftV7RLv";

    /// Park for the lifetime of the process so that
    /// `Java_dev_dioxus_main_RiverNodeService_nativeOnServiceStop` can
    /// fire the oneshot from a foreign thread (the JNI callback runs on
    /// whatever thread the Android service is destroyed on, NOT the
    /// freenet-worker tokio runtime).
    ///
    /// Populated once `run_node()` reaches its `select!` point and parks
    /// the receiver. If the user hits Home and then the foreground
    /// service is destroyed before the node has finished booting,
    /// `nativeOnServiceStop` finds `None` in the Mutex and short-circuits —
    /// the process will be killed by the OS anyway and there's nothing
    /// for us to gracefully drop.
    static SHUTDOWN_TX: Mutex<Option<oneshot::Sender<()>>> = Mutex::new(None);

    /// Fallback `gateways.toml` and its referenced X25519 public-key files,
    /// snapshotted from `https://freenet.org/keys/` at release time.
    ///
    /// Used only on first launch IF freenet's auto-fetch from
    /// `freenet.org` fails (offline, DNS blocked, etc.). Once the live
    /// fetch succeeds, freenet overwrites `config_dir/gateways.toml` with
    /// the freshly-fetched paths and these fallbacks are unused. See
    /// `vendor/freenet/src/config.rs::load_gateways_from_index` for the
    /// canonical fetch behaviour and the local-cache fallback path that
    /// reads this file on retry.
    ///
    /// Refresh procedure: re-fetch from
    /// `https://freenet.org/keys/{gateways.toml,public.nova.gw.pem,public.vega.gw.pem}`
    /// when cutting a release, replace the three files under
    /// `ui/assets/freenet/`, and verify a debug APK can bootstrap with
    /// `freenet.org` DNS blocked.
    const FALLBACK_GATEWAYS_TOML: &[u8] = include_bytes!("../assets/freenet/gateways.toml");
    const FALLBACK_NOVA_PUBKEY: &[u8] = include_bytes!("../assets/freenet/public.nova.gw.pem");
    const FALLBACK_VEGA_PUBKEY: &[u8] = include_bytes!("../assets/freenet/public.vega.gw.pem");

    /// Boot the in-process Freenet node on a dedicated background thread.
    ///
    /// Returns immediately; the node runs for the lifetime of the process
    /// (or until an unrecoverable error, which is logged). Safe to call
    /// multiple times — guarded by a one-shot.
    pub fn start_embedded_node() {
        use std::sync::Once;
        static START: Once = Once::new();
        START.call_once(|| {
            info!("Spawning embedded Freenet node thread");
            let handle = std::thread::Builder::new()
                .name("freenet-embedded".into())
                .stack_size(4 * 1024 * 1024) // wasmtime needs >stack default
                .spawn(|| {
                    let rt = match tokio::runtime::Builder::new_multi_thread()
                        .enable_all()
                        .thread_name("freenet-worker")
                        .build()
                    {
                        Ok(rt) => rt,
                        Err(e) => {
                            error!("Failed to build tokio runtime for embedded node: {e}");
                            return;
                        }
                    };
                    rt.block_on(async move {
                        match run_node().await {
                            Ok(()) => info!("Embedded Freenet node exited cleanly"),
                            Err(e) => {
                                error!("Embedded Freenet node exited with error: {e:?}")
                            }
                        }
                    });
                });
            if let Err(e) = handle {
                error!("Failed to spawn freenet-embedded thread: {e}");
            }
        });
    }

    /// Build a network-mode `Config` and drive the node's event loop.
    ///
    /// Mirrors `freenet/src/bin/freenet.rs::run_network` with one
    /// Android-specific twist (step 1.5):
    ///   1. Pre-bind the WS API listener AND grab the
    ///      `OriginContractMap` via
    ///      `serve_client_api_with_listener_and_contracts`. We use this
    ///      entry point (not the simpler `serve_client_api`) because the
    ///      map is the only way to attest an origin to the chat delegate
    ///      without a gateway shell.
    ///   1.5. Insert a synthetic auth_token entry into the map under
    ///      River's web-container contract id, and publish the token via
    ///      [`EMBEDDED_AUTH_TOKEN`] so the UI's loopback dial can append
    ///      `?authToken=…`. See [`EMBEDDED_AUTH_TOKEN`]'s doc for why.
    ///   2. `NodeConfig::new` loads peer-state config (gateway list,
    ///      peer id, etc.).
    ///   3. `node_config.build(clients)` wires the client API into the
    ///      node.
    ///   4. `freenet::run_network_node` drives the event loop forever.
    async fn run_node() -> anyhow::Result<()> {
        let data_dir = resolve_data_dir();
        if android_files_dir().is_none() {
            warn!(
                "JNI lookup for Context.getFilesDir() failed; falling back to {}. \
                 The node will still try to boot, but if the package id ever drifts \
                 from `org.freenet.river` (see ui/Dioxus.toml) writes will land in \
                 the wrong place. This usually means an emulator launched without \
                 an attached Activity, or the ndk-context handles weren't populated.",
                FREENET_DATA_DIR_FALLBACK
            );
        }
        if let Err(e) = std::fs::create_dir_all(&data_dir) {
            warn!("Could not create node data dir {data_dir:?}: {e}");
            // Continue anyway — freenet's own setup will surface the
            // error through anyhow with full context.
        }

        // Stage the fallback `gateways.toml` + PEMs into the node's
        // config dir BEFORE `args.build()`, because `ConfigArgs::build`
        // is itself what loads the gateway list — if neither a live
        // fetch nor a local cache produces one, build() returns
        // `Cannot initialize node without gateways` and we never even
        // get a `Config` to inspect. The config dir layout is fixed
        // (see vendor/freenet/src/config.rs::ConfigPaths::build):
        //
        //   config_dir  = data_dir            (the path we set above)
        //   secrets_dir = data_dir.join("secrets")
        //
        // Best-effort: failures are logged but don't abort startup,
        // because freenet's own first-launch HTTPS fetch from
        // `freenet.org` is the primary path. The bundled fallback
        // only matters when first launch is offline (no network).
        let config_dir = data_dir.clone();
        let secrets_dir = data_dir.join("secrets");
        if let Err(e) = stage_fallback_gateways(&config_dir, &secrets_dir) {
            warn!("Could not stage fallback gateways: {e}. Live fetch will be attempted.");
        }

        let mut args = ConfigArgs {
            mode: Some(OperationMode::Network),
            ..ConfigArgs::default()
        };
        args.config_paths.config_dir = Some(data_dir.clone());
        args.config_paths.data_dir = Some(data_dir.clone());
        args.config_paths.log_dir = Some(data_dir.join("logs"));
        // Effectively disable the token-expiry sweep. The synthetic
        // auth_token we register below is loopback-only, never leaves the
        // device, and nothing in the WS request path updates the entry's
        // `last_accessed` field — at the default 24h TTL the cleanup task
        // would silently reap it and every subsequent ApplicationMessage
        // would start failing with `missing message origin` again. Set
        // `u64::MAX` so the cleanup retain-comparison never evicts. (No
        // overflow: `Duration::from_secs(u64::MAX)` saturates, and
        // `elapsed < ttl` is always true.)
        args.ws_api.token_ttl_seconds = Some(u64::MAX);

        info!("Building freenet network Config at {:?}", data_dir);
        let config = args.build().await?;
        let ws_socket = config.ws_api.clone();

        // Pre-bind the WS API listener ourselves so we can hand it to
        // `serve_client_api_with_listener_and_contracts`. That entry
        // point returns the `OriginContractMap` we need to populate
        // with the synthetic auth_token before any request lands; the
        // shorter `serve_client_api(config)` would let freenet bind
        // internally but doesn't surface the map. Freenet's
        // `serve_with_listener` calls `set_nonblocking(true)` on the
        // listener before `tokio::net::TcpListener::from_std`, so we
        // pass a plain blocking listener here.
        info!(
            "Starting client API on {:?}:{}",
            ws_socket.address, ws_socket.port
        );
        let listener =
            std::net::TcpListener::bind((ws_socket.address, ws_socket.port)).map_err(|e| {
                anyhow::anyhow!(
                    "failed to bind WS API listener on {}:{} ({e}). \
                     If another freenet process is already running on this device, \
                     stop it before relaunching River.",
                    ws_socket.address,
                    ws_socket.port,
                )
            })?;
        let (clients, origin_contracts) =
            serve_client_api_with_listener_and_contracts(ws_socket, listener)
                .await
                .map_err(|e| anyhow::anyhow!("failed to start client API: {e}"))?;

        // Pre-register a synthetic auth_token so the chat delegate's
        // `check_origin` finds an attested `MessageOrigin::WebApp(contract_id)`
        // on every ApplicationMessage, instead of `None` (which the delegate
        // rejects with "missing message origin" → every save times out → the
        // user's room dies on relaunch).
        //
        // The contract id we attest is River's published web-container id,
        // so the chat-delegate's per-origin storage namespace
        // (`signing_key:{origin}:…`, `outbound_dms:{origin}:…`, etc.) lines
        // up byte-for-byte with what web clients write — keeps delegate state
        // coherent if it ever syncs across devices, and is the same gate web
        // clients hit (since the gateway shell attests this same id).
        //
        // Fatal-on-failure intentional: if the contract-id constant ever drifts
        // out of `bs58` decode shape we want the node boot to fail loudly,
        // because every delegate request after this would be silently broken.
        let contract_id = ContractInstanceId::from_str(WEB_CONTAINER_CONTRACT_ID).map_err(|e| {
            anyhow::anyhow!(
                "WEB_CONTAINER_CONTRACT_ID ({WEB_CONTAINER_CONTRACT_ID:?}) failed to \
                     parse as base58: {e}. The constant must stay in sync with \
                     `published-contract/contract-id.txt`."
            )
        })?;
        let auth_token = AuthToken::generate();
        origin_contracts.insert(
            auth_token.clone(),
            OriginContract::new(contract_id, ClientId::next()),
        );
        let token_string = auth_token.as_str().to_string();
        // `OnceLock::set` is idempotent at the call-site we control
        // (`Once`-guarded `start_embedded_node`); if a future change ever
        // double-boots the node we silently keep the first token, since
        // the URL-builder in `connection_manager` already cached it.
        let _ = EMBEDDED_AUTH_TOKEN.set(token_string);
        info!(
            "Synthetic auth_token registered against {} \
             ({} entries in origin_contracts)",
            WEB_CONTAINER_CONTRACT_ID,
            origin_contracts.len(),
        );

        info!("Initialising NodeConfig (loads gateways.toml, derives peer id)");
        let node_config = NodeConfig::new(config).await?;

        info!("Building network node");
        let node = node_config.build(clients).await?;

        // Park the shutdown receiver before entering the event loop so
        // that a service-stop intent landing the instant after the
        // notification appears can still tear us down. The lock is held
        // only across the assignment.
        let (tx, shutdown_rx) = oneshot::channel::<()>();
        *SHUTDOWN_TX.lock().expect("SHUTDOWN_TX poisoned") = Some(tx);

        // Bug B (background notifications): a watcher that lives on THIS
        // runtime — kept alive by the foreground service — subscribes to
        // the user's rooms over the loopback WS and posts notifications
        // even while the WebView UI is suspended. Detached; it reconnects
        // on its own and never blocks node shutdown.
        tokio::spawn(run_notification_watcher());

        info!("Running network node event loop (with foreground-service shutdown hook)");
        tokio::select! {
            res = freenet::run_network_node(node) => {
                res?;
            }
            _ = shutdown_rx => {
                info!("Embedded node shutdown requested by RiverNodeService.onDestroy");
            }
        }
        Ok(())
    }

    use river_core::room_state::member::MemberId;

    /// Snapshot every room context the UI has published.
    fn notif_contexts_snapshot() -> Vec<RoomNotifContext> {
        super::NOTIF_CONTEXTS
            .lock()
            .ok()
            .and_then(|g| g.as_ref().map(|m| m.values().cloned().collect()))
            .unwrap_or_default()
    }

    /// Current `last_seen_ms` for a room (0 if unknown).
    fn notif_last_seen(room: MemberId) -> u64 {
        super::NOTIF_CONTEXTS
            .lock()
            .ok()
            .and_then(|g| {
                g.as_ref()
                    .and_then(|m| m.get(&room).map(|c| c.last_seen_ms))
            })
            .unwrap_or(0)
    }

    /// Advance a room's `last_seen_ms` (monotonic) so a re-sync of the same
    /// update doesn't re-notify.
    fn notif_advance_last_seen(room: MemberId, ts_ms: u64) {
        if let Ok(mut guard) = super::NOTIF_CONTEXTS.lock() {
            if let Some(map) = guard.as_mut() {
                if let Some(c) = map.get_mut(&room) {
                    c.last_seen_ms = c.last_seen_ms.max(ts_ms);
                }
            }
        }
    }

    /// Pull the freshly-added messages out of an incoming room-state
    /// update. Mirrors `room_synchronizer`'s delta/full-state handling but
    /// non-panicking (a malformed payload yields an empty vec, never a
    /// crash). Phase 1 reads only message metadata (author/time) — which is
    /// plaintext even in private rooms — so it needs no room secrets.
    fn extract_new_messages(
        update: &freenet_stdlib::prelude::UpdateData,
    ) -> Vec<river_core::room_state::message::AuthorizedMessageV1> {
        use freenet_stdlib::prelude::UpdateData;
        use river_core::room_state::{ChatRoomStateV1, ChatRoomStateV1Delta};
        fn de<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Option<T> {
            ciborium::de::from_reader(bytes).ok()
        }
        match update {
            UpdateData::Delta(d) => de::<ChatRoomStateV1Delta>(d.as_ref())
                .and_then(|x| x.recent_messages)
                .unwrap_or_default(),
            UpdateData::State(s) => de::<ChatRoomStateV1>(s.as_ref())
                .map(|x| x.recent_messages.messages)
                .unwrap_or_default(),
            UpdateData::StateAndDelta { delta, .. } => de::<ChatRoomStateV1Delta>(delta.as_ref())
                .and_then(|x| x.recent_messages)
                .unwrap_or_default(),
            _ => Vec::new(),
        }
    }

    /// Decide whether an incoming update warrants a notification and, if so,
    /// post it. Advances `last_seen` regardless so the same message can't
    /// re-notify on a state re-sync.
    fn handle_room_update(room: MemberId, update: &freenet_stdlib::prelude::UpdateData) {
        let msgs = extract_new_messages(update);
        if msgs.is_empty() {
            return;
        }
        let Some(ctx) = super::NOTIF_CONTEXTS
            .lock()
            .ok()
            .and_then(|g| g.as_ref().and_then(|m| m.get(&room).cloned()))
        else {
            return;
        };
        let last_seen = notif_last_seen(room);
        // Phase 2: the decide + decrypt + render step is the cross-target,
        // unit-tested `compose_notification` (reuses the UI's preview decoder
        // and the context's decrypted nicknames), so the watcher posts the
        // same "sender: preview" the UI would.
        let Some((body, max_ts)) = super::compose_notification(
            &msgs,
            ctx.self_member_id,
            &ctx.secrets,
            &ctx.nicknames,
            last_seen,
        ) else {
            return;
        };
        // Advance the watermark even when foregrounded so neither path
        // re-notifies these messages (the UI reads the same watermark).
        notif_advance_last_seen(room, max_ts);

        // When the UI is in the foreground it owns notifications (and runs
        // its own current-room suppression). The watcher only fires while
        // backgrounded — where the UI is suspended and would otherwise
        // never notify.
        if super::android_is_foreground() {
            return;
        }
        let title = if ctx.room_name.is_empty() {
            "River".to_string()
        } else {
            ctx.room_name.clone()
        };
        // Log the composed body so an on-device backgrounded send shows
        // exactly what the watcher rendered, independent of the UI path.
        info!("notif watcher: posting background notification for room {room} body={body:?}");
        post_message_notification(&title, &body, &room.to_string());
    }

    /// Background notification watcher (Bug B). Runs on the node's tokio
    /// runtime (kept alive by the foreground service), connects to the
    /// embedded node's loopback WS as a second client, subscribes to every
    /// room the UI has published a context for, and posts a notification
    /// when a message from another member arrives — even while the WebView
    /// UI is suspended.
    ///
    /// Phase 1: posts a coarse "N new messages" with the UI-decrypted room
    /// name; it does not decrypt message bodies (deferred to Phase 2). It
    /// only fires while backgrounded (`!android_is_foreground()`) so the UI
    /// owns notifications whenever it's alive. Fully fail-safe: every error
    /// is logged and the loop reconnects; it never panics and never blocks
    /// node shutdown.
    ///
    /// See `openspec/changes/android-bundled-node/design-background-notifications.md`.
    async fn run_notification_watcher() {
        use freenet_stdlib::client_api::{
            ClientRequest, ContractRequest, ContractResponse, HostResponse, WebApi,
        };
        // `ContractInstanceId` is already imported at module scope.
        use std::collections::{HashMap, HashSet};
        use std::time::Duration;

        loop {
            let Some(token) = super::EMBEDDED_AUTH_TOKEN.get().cloned() else {
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            };
            let url = format!(
                "ws://127.0.0.1:7509/v1/contract/command?encodingProtocol=native&authToken={token}"
            );
            let stream = match tokio_tungstenite::connect_async(&url).await {
                Ok((s, _)) => s,
                Err(e) => {
                    warn!("notif watcher: connect failed ({e}); retrying in 5s");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    continue;
                }
            };
            info!("notif watcher: connected to embedded node WS");
            let mut api = WebApi::start(stream);
            let mut subscribed: HashSet<ContractInstanceId> = HashSet::new();
            let mut id_to_room: HashMap<ContractInstanceId, MemberId> = HashMap::new();

            let _disconnected: bool = 'conn: loop {
                // Reconcile subscriptions: subscribe to any room the UI has
                // published a context for that we're not yet watching.
                for ctx in notif_contexts_snapshot() {
                    let iid = *crate::util::owner_vk_to_contract_key(&ctx.owner_vk).id();
                    let room = MemberId::from(&ctx.owner_vk);
                    id_to_room.insert(iid, room);
                    if subscribed.insert(iid) {
                        let req = ClientRequest::ContractOp(ContractRequest::Subscribe {
                            key: iid,
                            summary: None,
                        });
                        if let Err(e) = api.send(req).await {
                            warn!("notif watcher: subscribe send failed ({e}); reconnecting");
                            break 'conn true;
                        }
                        info!("notif watcher: subscribed to room {room}");
                    }
                }

                // Block on the next response, but wake every 5s to reconcile
                // subscriptions for rooms the UI added after we connected.
                match tokio::time::timeout(Duration::from_secs(5), api.recv()).await {
                    Err(_elapsed) => continue,
                    Ok(Err(e)) => {
                        warn!("notif watcher: recv error ({e}); reconnecting");
                        break 'conn true;
                    }
                    Ok(Ok(HostResponse::ContractResponse(
                        ContractResponse::UpdateNotification { key, update },
                    ))) => {
                        if let Some(room) = id_to_room.get(&*key.id()).copied() {
                            handle_room_update(room, &update);
                        }
                    }
                    Ok(Ok(_)) => {}
                }
            };
            // Disconnected — pause briefly, then the outer loop reconnects.
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
    }

    /// Post (or update) a "new message" Android notification by calling
    /// into `RiverNodeService.postMessageNotification` over JNI.
    ///
    /// Called from `crate::components::app::notifications::show_notification`
    /// when the room synchronizer detects an incoming message the user
    /// should see, AND the user is either not in that room or not
    /// looking at the app right now.
    ///
    /// `tag` is the room's stable per-room identifier — subsequent
    /// messages in the same room replace the previous notification
    /// instead of stacking. Body text already includes "{sender}: …"
    /// formatting from the caller.
    ///
    /// Safe to invoke from any thread. The JNI `attach_current_thread`
    /// call below attaches whichever tokio worker happens to be
    /// running the synchronizer task. Every failure mode is logged
    /// and returns rather than panicking, so a JNI hiccup never
    /// crashes the node runtime.
    pub fn post_message_notification(title: &str, body: &str, tag: &str) {
        use jni::objects::{JClass, JObject, JValue};
        use jni::JavaVM;

        let ctx = ndk_context::android_context();
        let vm = match unsafe { JavaVM::from_raw(ctx.vm().cast()) } {
            Ok(vm) => vm,
            Err(e) => {
                warn!("post_message_notification: JavaVM::from_raw failed: {e}");
                return;
            }
        };
        let activity = unsafe { JObject::from_raw(ctx.context().cast()) };

        let mut env = match vm.attach_current_thread() {
            Ok(env) => env,
            Err(e) => {
                warn!("post_message_notification: attach_current_thread failed: {e}");
                return;
            }
        };

        // All the JNI work is wrapped in a single fallible closure so any
        // error logs once and clears the pending Java exception below.
        //
        // CRITICAL: we must resolve `RiverNodeService` through the Activity's
        // own ClassLoader, NOT via the bare class name. `call_static_method`
        // with a class-name string resolves it via JNI `FindClass`, and on a
        // thread attached with `attach_current_thread()` (this runs on a tokio
        // worker driving the synchronizer — a pure native thread with no Java
        // frames) `FindClass` resolves against the SYSTEM classloader, which
        // cannot see app classes. The bare lookup therefore throws
        // `NoClassDefFoundError` and the notification is silently dropped.
        // Going through `activity.getClassLoader().loadClass(...)` uses the
        // app classloader, which does know about `RiverNodeService`.
        let post = |env: &mut jni::JNIEnv| -> Result<(), jni::errors::Error> {
            let loader = env
                .call_method(
                    &activity,
                    "getClassLoader",
                    "()Ljava/lang/ClassLoader;",
                    &[],
                )?
                .l()?;
            let class_name = env.new_string("dev.dioxus.main.RiverNodeService")?;
            let service_class: JClass = env
                .call_method(
                    &loader,
                    "loadClass",
                    "(Ljava/lang/String;)Ljava/lang/Class;",
                    &[JValue::Object(&class_name)],
                )?
                .l()?
                .into();

            let title_j = env.new_string(title)?;
            let body_j = env.new_string(body)?;
            let tag_j = env.new_string(tag)?;

            env.call_static_method(
                &service_class,
                "postMessageNotification",
                "(Landroid/content/Context;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)V",
                &[
                    JValue::Object(&activity),
                    JValue::Object(&title_j),
                    JValue::Object(&body_j),
                    JValue::Object(&tag_j),
                ],
            )?;
            Ok(())
        };

        if let Err(e) = post(&mut env) {
            warn!("post_message_notification: JNI post failed: {e}");
            // Clear any pending Java exception so the next JNI use isn't
            // poisoned. exception_clear is itself best-effort.
            let _ = env.exception_clear();
        }
    }

    /// JNI hook invoked from `RiverNodeService.onDestroy()` (see
    /// `ui/android/kotlin/dev/dioxus/main/RiverNodeService.kt`).
    ///
    /// Fires the parked oneshot so the freenet-worker tokio runtime
    /// drops the network node + transport drivers in an orderly fashion
    /// rather than being SIGKILL'd by Android. Best-effort: if the node
    /// hasn't reached `run_network_node` yet (e.g. user mashed Stop
    /// during boot), the sender is `None` and we no-op.
    ///
    /// JNI ABI: the function name must match the fully-qualified
    /// Java/Kotlin class + method name, with `.` replaced by `_`. Any
    /// rename on either side must be made in lock-step.
    ///
    /// Signature uses the raw `jni::sys` C types so we don't carry a
    /// `JNIEnv<'local>` lifetime through a `#[no_mangle] extern "system"`
    /// — we never call back into the JVM from this function, so the
    /// raw pointers are all we need.
    #[no_mangle]
    pub unsafe extern "system" fn Java_dev_dioxus_main_RiverNodeService_nativeOnServiceStop(
        _env: *mut jni::sys::JNIEnv,
        _class: jni::sys::jclass,
    ) {
        match SHUTDOWN_TX.lock() {
            Ok(mut slot) => match slot.take() {
                Some(tx) => {
                    if tx.send(()).is_err() {
                        warn!("Embedded node already gone — shutdown signal dropped");
                    } else {
                        info!("Shutdown signal sent to embedded node");
                    }
                }
                None => {
                    info!(
                        "RiverNodeService.onDestroy fired before embedded node reached \
                         the event loop — nothing to signal"
                    );
                }
            },
            Err(e) => {
                error!("SHUTDOWN_TX mutex poisoned: {e}");
            }
        }
    }

    /// JNI hook invoked from `MainActivity.onStart` / `onStop`.
    ///
    /// Updates [`ANDROID_FOREGROUND`] in lock-step with the activity's
    /// visibility so `notifications::is_document_visible` reports the
    /// right state when deciding whether to post a notification for a
    /// message in the room the user has open. See `MainActivity.kt`
    /// for the call sites and the rationale block.
    #[no_mangle]
    pub unsafe extern "system" fn Java_dev_dioxus_main_MainActivity_nativeSetForeground(
        _env: *mut jni::sys::JNIEnv,
        _class: jni::sys::jclass,
        foreground: jni::sys::jboolean,
    ) {
        let on = foreground != 0;
        ANDROID_FOREGROUND.store(on, Ordering::Release);
        info!("Activity foreground state: {on}");
    }

    /// Stage the bundled fallback `gateways.toml` + PEMs into
    /// `config_dir` and `secrets_dir`, ONLY if
    /// `config_dir/gateways.toml` doesn't already exist.
    ///
    /// Freenet's [`NodeConfig::new`] tries the live remote fetch
    /// first; on success, it overwrites `config_dir/gateways.toml`
    /// (and the PEMs in `secrets_dir`) with the freshly-fetched
    /// copy. On failure, it falls back to parsing whatever is
    /// already at `config_dir/gateways.toml`. By pre-staging the
    /// bundled fallback when that file is absent, we guarantee an
    /// offline first-launch still has a valid gateways list to
    /// parse — without it the node would error out with `Cannot
    /// initialize node without gateways`.
    ///
    /// We do NOT overwrite an existing `gateways.toml`: any file
    /// already at that path is freenet's own cache from a prior
    /// successful fetch and is at least as fresh as our bundle.
    ///
    /// The bundled PEMs match the snapshot in `ui/assets/freenet/`.
    /// If the live fetch later succeeds, freenet overwrites the
    /// same PEM filenames in `secrets_dir` with the fresh content —
    /// our stale bytes don't linger.
    fn stage_fallback_gateways(config_dir: &Path, secrets_dir: &Path) -> std::io::Result<()> {
        let gateways_file = config_dir.join("gateways.toml");
        if gateways_file.exists() {
            return Ok(());
        }
        std::fs::create_dir_all(config_dir)?;
        std::fs::create_dir_all(secrets_dir)?;

        let nova_path = secrets_dir.join("public.nova.gw.pem");
        let vega_path = secrets_dir.join("public.vega.gw.pem");
        std::fs::write(&nova_path, FALLBACK_NOVA_PUBKEY)?;
        std::fs::write(&vega_path, FALLBACK_VEGA_PUBKEY)?;

        // Build the TOML with absolute paths. Freenet's local-cache
        // parser deserializes `public_key` straight into a `PathBuf`
        // and opens the file with no further path resolution;
        // relative paths would be resolved against the CWD, which
        // is undefined on Android.
        let toml = format!(
            "# Bundled fallback (used because freenet's live fetch failed).\n\
             [[gateways]]\n\
             public_key = \"{}\"\n\
             [gateways.address]\n\
             hostname = \"nova.locut.us:31337\"\n\
             \n\
             [[gateways]]\n\
             public_key = \"{}\"\n\
             [gateways.address]\n\
             hostname = \"vega.locut.us:31337\"\n",
            nova_path.display(),
            vega_path.display(),
        );
        std::fs::write(&gateways_file, toml)?;

        info!(
            "Staged fallback gateways.toml + 2 PEMs at {:?} ({} bytes bundled)",
            gateways_file,
            FALLBACK_GATEWAYS_TOML.len(),
        );
        Ok(())
    }
}

#[cfg(target_os = "android")]
pub use android::{post_message_notification, start_embedded_node};

/// Non-Android stub. The Android startup path in `App()` is itself
/// `cfg(target_os = "android")`-gated, so this stub is unreachable
/// in practice — but exposing it lets the module compile for host
/// `cargo check` / `cargo test` without dragging in freenet, tokio,
/// jni, or ndk-context.
#[cfg(not(target_os = "android"))]
#[allow(dead_code)]
pub fn start_embedded_node() {}

/// Non-Android stub for the new-message notification bridge. Callers
/// in `notifications.rs` are themselves `cfg(target_os = "android")`-
/// gated so this is unreachable in practice — the stub exists so
/// other native-target builds (host tests, future desktop) still
/// compile.
#[cfg(not(target_os = "android"))]
#[allow(dead_code)]
pub fn post_message_notification(_title: &str, _body: &str, _tag: &str) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_path_targets_known_package_id() {
        // The hardcoded fallback must point at the package id River
        // declares in `ui/Dioxus.toml` — if that identifier ever
        // changes, this constant has to follow or the bundled node
        // will write to a directory Android won't grant access to.
        assert!(
            FREENET_DATA_DIR_FALLBACK.contains("/org.freenet.river/"),
            "fallback {FREENET_DATA_DIR_FALLBACK} no longer targets org.freenet.river"
        );
        assert!(
            FREENET_DATA_DIR_FALLBACK.ends_with("/freenet"),
            "fallback {FREENET_DATA_DIR_FALLBACK} must end with /freenet \
             so the node has its own subdir under the app's private files area"
        );
    }

    /// On non-Android targets `android_files_dir` is the no-op stub,
    /// so `resolve_data_dir` MUST return the hardcoded fallback.
    /// Gated to non-android because on a real device the JNI lookup
    /// succeeds and returns a different (real) path — the assertion
    /// would not hold there.
    #[cfg(not(target_os = "android"))]
    #[test]
    fn resolve_data_dir_returns_fallback_off_device() {
        assert!(android_files_dir().is_none(), "host stub must return None");
        let dir = resolve_data_dir();
        assert_eq!(dir, PathBuf::from(FREENET_DATA_DIR_FALLBACK));
    }

    // ---- Bug B Phase 2: compose_notification (decrypt + render + dedup) ----

    use river_core::room_state::member::MemberId;
    use river_core::room_state::message::{AuthorizedMessageV1, MessageV1, RoomMessageBody};
    use std::collections::HashMap;
    use std::time::{Duration, UNIX_EPOCH};

    fn test_member(seed: u8) -> MemberId {
        let sk = ed25519_dalek::SigningKey::from_bytes(&[seed; 32]);
        MemberId::from(&sk.verifying_key())
    }

    fn test_msg(author: MemberId, ts_ms: u64, content: RoomMessageBody) -> AuthorizedMessageV1 {
        AuthorizedMessageV1 {
            message: MessageV1 {
                room_owner: test_member(0),
                author,
                time: UNIX_EPOCH + Duration::from_millis(ts_ms),
                content,
            },
            // compose_notification never inspects the signature.
            signature: ed25519_dalek::Signature::from_bytes(&[0u8; 64]),
        }
    }

    #[test]
    fn compose_renders_sender_and_public_preview() {
        let me = test_member(1);
        let alice = test_member(2);
        let mut nicks = HashMap::new();
        nicks.insert(alice, "Alice".to_string());
        let msgs = vec![test_msg(
            alice,
            100,
            RoomMessageBody::public("hello".to_string()),
        )];
        let (body, max_ts) =
            compose_notification(&msgs, me, &HashMap::new(), &nicks, 0).expect("should notify");
        assert_eq!(body, "Alice: hello");
        assert_eq!(max_ts, 100);
    }

    #[test]
    fn compose_decrypts_private_body_natively() {
        use river_core::ecies::encrypt_with_symmetric_key;
        use river_core::room_state::content::{
            TextContentV1, CONTENT_TYPE_TEXT, TEXT_CONTENT_VERSION,
        };
        let me = test_member(1);
        let bob = test_member(3);
        let secret = [7u8; 32];
        let (ciphertext, nonce) = encrypt_with_symmetric_key(
            &secret,
            &TextContentV1::new("top secret".to_string()).encode(),
        );
        let content = RoomMessageBody::Private {
            content_type: CONTENT_TYPE_TEXT,
            content_version: TEXT_CONTENT_VERSION,
            ciphertext,
            nonce,
            secret_version: 5,
        };
        let mut secrets = HashMap::new();
        secrets.insert(5u32, secret);
        let mut nicks = HashMap::new();
        nicks.insert(bob, "Bob".to_string());
        let msgs = vec![test_msg(bob, 200, content)];
        let (body, max_ts) =
            compose_notification(&msgs, me, &secrets, &nicks, 0).expect("should notify");
        assert_eq!(body, "Bob: top secret");
        assert_eq!(max_ts, 200);
    }

    #[test]
    fn compose_skips_self_watermarked_and_batches() {
        let me = test_member(1);
        let alice = test_member(2);
        let mut nicks = HashMap::new();
        nicks.insert(alice, "Alice".to_string());

        // Only a self-authored message → nothing to notify.
        let only_self = vec![test_msg(
            me,
            100,
            RoomMessageBody::public("mine".to_string()),
        )];
        assert!(compose_notification(&only_self, me, &HashMap::new(), &nicks, 0).is_none());

        // All at/below the watermark → already surfaced, nothing new.
        let old = vec![test_msg(
            alice,
            50,
            RoomMessageBody::public("old".to_string()),
        )];
        assert!(compose_notification(&old, me, &HashMap::new(), &nicks, 50).is_none());

        // Two new messages from others → batch summary, watermark to newest.
        let batch = vec![
            test_msg(alice, 100, RoomMessageBody::public("a".to_string())),
            test_msg(alice, 110, RoomMessageBody::public("b".to_string())),
        ];
        let (body, max_ts) =
            compose_notification(&batch, me, &HashMap::new(), &nicks, 0).expect("should notify");
        assert_eq!(body, "2 new messages");
        assert_eq!(max_ts, 110);
    }

    #[test]
    fn compose_unknown_sender_falls_back_to_someone() {
        let me = test_member(1);
        let stranger = test_member(9);
        let msgs = vec![test_msg(
            stranger,
            100,
            RoomMessageBody::public("hi".to_string()),
        )];
        let (body, _) = compose_notification(&msgs, me, &HashMap::new(), &HashMap::new(), 0)
            .expect("should notify");
        assert_eq!(body, "Someone: hi");
    }
}
