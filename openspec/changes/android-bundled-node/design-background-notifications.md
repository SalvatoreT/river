# Design note: Android background message notifications

Status: **proposed** (discovered during on-device notification testing, 2026-05-28)

This note captures two bugs found while testing the new-message notification
feature (commit `b1d38320`, PR freenet/river#313) on a physical Pixel 6 Pro,
plus a design for the larger of the two.

## What was tested

Two debug-build phones on the live network, joined to the same room
("Very Fun Room", owner key `5ob9CQuo…`, member id `FUPOD6VP`). Messages
sent from phone B; notification behaviour observed on phone A via logcat
(`/tmp/claude/notif_trace.log`) and `dumpsys notification`.

Invitation redemption + cross-WASM room join over the live network worked
end-to-end and did **not** reproduce the documented Pixel-10 redemption
crash (`paste_invitation_modal.rs` TODO) on this device.

## Bug A — JNI classloader (fixed in this change)

`node_runtime::android::post_message_notification` posted nothing, every
time, even when `notify_new_messages` fired with fully-decrypted content
(`room='Very Fun Room' body='Party Man: …' tag=FUPOD6VP`):

```
WARN river_ui::node_runtime::android: post_message_notification: call_static_method failed: Java exception was thrown
```

`call_static_method("dev/dioxus/main/RiverNodeService", …)` resolves the
class via JNI `FindClass`. The call runs on a tokio worker attached with
`attach_current_thread()` — a pure native thread with no Java frames — so
`FindClass` resolves against the **system classloader**, which cannot see
app classes. The bare lookup throws `NoClassDefFoundError`; the
`exception_clear()` then swallowed it, so the channel `river_messages` was
never created and nothing reached the tray.

**Fix (applied):** resolve `RiverNodeService` through the Activity's own
`ClassLoader` (`activity.getClassLoader().loadClass(...)`) and call
`call_static_method` on the resulting `JClass`. Verified to compile for
`aarch64-linux-android`. Still needs an on-device re-test of the
foreground/other-room case.

This fix is also a prerequisite for Bug B's watcher, which posts from the
same kind of native thread.

## Bug B — the UI is dormant in the background (design below)

Even with Bug A fixed, notifications will not fire while the app is truly
backgrounded. The notification trigger chain lives entirely in the Dioxus
WebView UI:

```
node (WS) ── UpdateNotification ──▶ river_ui room_synchronizer
                                      ├─ compute new-message diff
                                      └─ notify_new_messages
                                           └─ show_notification
                                                └─ node_runtime::post_message_notification (JNI)
```

Observed: with the app backgrounded ~8 min, the embedded node (foreground
service, own tokio runtime) kept receiving room state and logged
`Sent update notification to client (shared storage) client=1000002`, but
there were **zero `river_ui::components` log lines** — the WebView was
suspended, so the WS updates queued unprocessed. The instant the app was
foregrounded, the backlog drained and `notify_new_messages` ran. So
notifications only fire when the app is (or has recently been) in the
foreground — exactly the opposite of when they are useful.

### Goal

Detect new messages and post notifications from the always-alive
node/service layer, independent of the WebView lifecycle.

### Constraints

1. Message previews **and room names** are AES-256-GCM encrypted (private
   rooms); sender nicknames are sealed too. Rendering a useful notification
   needs the per-room secret map.
2. Notify only for messages from **other** members, in rooms the user is a
   member of, that the user has **not already seen**.
3. Must not double-notify with the UI when foregrounded, nor miss-notify.
4. The state-parsing + decryption code (`river-core`/`common`,
   `unseal_bytes_with_secrets`, `ChatRoomStateV1`) is **already in the
   native binary**. The missing piece is *data* (per-room secrets, self
   identity, last-seen markers, plaintext room name), not code.
5. Event-driven, not polling (battery).

### Proposed design — native notification watcher fed by a UI snapshot

**A. Shared notification-context store (in-process, no JNI).**
A `static NOTIF_CONTEXTS: Mutex<HashMap<MemberId, RoomNotifContext>>` in
`node_runtime`. `RoomNotifContext { self_member_id, secrets:
HashMap<u32,[u8;32]>, room_name_plaintext, member_nicknames, last_seen_ts,
muted }`. The UI (river_ui) writes/updates it wherever it already has live
`RoomData` — the `repopulate_secrets_from_state` call-sites (already
pinned by a grep test), room rename, and read-marker advance. UI and
watcher are both Rust in the same binary, so this is a plain shared static,
not a JNI hop.

*Security:* secrets already live in this process (and the chat delegate
persists plaintext on disk per the existing threat model) — no new
exposure.

**B. In-process subscriber on the node runtime (always alive).**
After the node + client API are up, open a *second in-process client* to
the embedded node's existing client API
(`serve_client_api_with_listener_and_contracts`, reusing
`EMBEDDED_AUTH_TOKEN`) and `Subscribe` to each room in `NOTIF_CONTEXTS`.
This client runs on the node's tokio runtime (kept alive by the FGS), so it
receives `UpdateNotification`s even while the WebView is suspended. Keep the
subscription set in sync with the context map.
*Alternative considered:* hook freenet's internal `BroadcastStateChange`
directly — rejected as more invasive than reusing the public client API.

**C. Detection + post (native).** On each `UpdateNotification` for a
subscribed room:
1. Look up the `RoomNotifContext` (skip if absent / muted).
2. Deserialize / apply the delta to `ChatRoomStateV1` (native Rust).
3. Select messages `timestamp > last_seen_ts && author != self_member_id`.
   **Factor this filter out of `notify_new_messages` into a shared
   `river-core`/ui helper** so the UI and the watcher cannot drift (same
   discipline as `lookup_outbound_plaintext` / `build_rotation_encrypted_secrets`).
4. Decrypt preview + sender nickname + room name via `context.secrets`.
5. Post via the Bug-A-fixed `post_message_notification`.
6. Advance `last_seen_ts`.

**D. De-dup / suppression coordination.** `last_seen_ts` is the single
source of truth shared by UI and watcher; both advance it and take the
**max** on collision (mirrors the OUTBOUND_DMS "keep larger timestamp"
merge). Mirror `CURRENT_ROOM` into a native marker so the watcher can apply
the same "current room + foreground ⇒ suppress" guard the UI uses
(`android_is_foreground()` already exists).

**E. Lifecycle.** Start the watcher in `run_node` after the client API
binds; stop it on the existing `RiverNodeService.onDestroy` oneshot. The UI
fills `NOTIF_CONTEXTS` as rooms load on cold start; the watcher subscribes
as entries appear.

### Why not just keep the WebView alive?

Android throttles/suspends background WebView timers and rendering; keeping
it alive reliably needs wake-locks, fights the platform, costs battery, and
still breaks under Doze. Moving the trigger to the node layer aligns with
where the data already lives and where the process is already kept warm (the
FGS exists precisely to keep the node running). The chat delegate can't help
either — sandboxed WASM can't do JNI / post notifications.

### Phased landing

- **Phase 0 (done):** Bug A classloader fix.
- **Phase 1 (IMPLEMENTED — pending on-device verification):** context
  store + in-process subscriber + watcher posting a coarse "N new
  messages" using the UI-decrypted room name (no body decryption). Proves
  the always-alive path end-to-end.
- **Phase 2 (IMPLEMENTED — pending on-device verification):** full native
  decryption (preview + sender) and a shared read-watermark so the UI's
  `notify_new_messages` and the watcher don't double-notify. Details below.

### Phase 1 implementation map

- `ui/src/node_runtime.rs`
  - `RoomNotifContext` + `NOTIF_CONTEXTS: Mutex<Option<HashMap<MemberId,
    RoomNotifContext>>>` + `pub fn update_notif_context(..)` — cross-target
    store/setter (`last_seen_ms` is max-on-write / monotonic).
  - `run_notification_watcher()` (android) — spawned in `run_node` just
    before the event-loop `select!`, on the node's always-alive runtime.
    Connects a 2nd loopback WS client (reusing `EMBEDDED_AUTH_TOKEN`),
    subscribes to every room in `NOTIF_CONTEXTS` (re-reconciles every 5s
    via a `timeout(recv)` tick so rooms added after connect get picked
    up), and on `UpdateNotification` runs `handle_room_update`.
  - `extract_new_messages` — non-panicking ciborium decode of
    `State`/`Delta`/`StateAndDelta` → new `AuthorizedMessageV1`s. Reads
    only plaintext metadata (`message.author`, `message.time`), so it
    needs no room secrets.
  - `handle_room_update` — filters `author != self && time_ms >
    last_seen`, advances `last_seen`, and posts via the Bug-A-fixed
    `post_message_notification` **only when `!android_is_foreground()`**.
- `ui/src/components/app/notifications.rs::publish_notif_contexts()`
  (android) — reads `ROOMS`, decrypts each room name with
  `unseal_bytes_with_secrets`, computes the read-watermark, and calls
  `update_notif_context`.
- `ui/src/components/app.rs` — a `use_effect` subscribed to `ROOMS` calls
  `publish_notif_contexts()` on every change.

### Phase 2 implementation map

- `ui/src/node_runtime.rs`
  - `RoomNotifContext` gains `secrets: HashMap<u32,[u8;32]>` and
    `nicknames: HashMap<MemberId, String>` (decrypted by the UI) so the
    watcher can render "sender: preview" natively.
  - `update_notif_context` now **seeds `last_seen_ms` once** (first
    observation = history baseline) and leaves it to the notify paths
    afterwards — fixes a latent Phase 1 race where a context refresh landing
    between a message's arrival and the watcher processing it could suppress
    it.
  - `handle_room_update` decrypts the newest new message via the UI's
    `get_message_preview` + the context nicknames → "sender: preview" for a
    single message, "N new messages" for a batch (mirrors the UI).
  - `notif_watermark(&VerifyingKey)` / `notif_mark_notified(&VerifyingKey, ts)`
    — the UI's accessors into the shared watermark.
- `ui/src/components/app/notifications.rs`
  - `get_message_preview` is now `pub(crate)` (reused by the watcher).
  - `publish_notif_contexts` builds the decrypted nicknames map + passes the
    room secrets.
  - `notify_new_messages` (Android only): skips a notification whose
    messages the watcher already surfaced (`max_ts <= watermark`), else
    advances the watermark so the watcher won't re-post them. This is the
    foreground-transition double-notify fix.

### Remaining limitations (smaller follow-ups)

- **`StateAndDelta` body / `RelatedState*`:** only `State` and `Delta` (and
  the delta half of `StateAndDelta`) are mined for messages; other
  `UpdateData` shapes fall through to no-notify (safe, may miss).
- **Single WS port assumption:** the watcher hardcodes `127.0.0.1:7509`
  like `connection_manager::node_url`.
- **Batch body:** like the UI, a multi-message batch shows "N new messages"
  rather than per-message previews.

### Effort / risk

Context store + UI write-points: moderate (existing pinned call-sites).
In-process subscriber: moderate (reuses client API). Shared detection
helper: low. The fiddly part is the last-seen / current-room / foreground
coordination — needs care to avoid double- or missed-notifies.
