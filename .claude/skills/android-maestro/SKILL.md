---
name: android-maestro
description: Build, install, run, and UI-test River's Android app on a physical device with maestro. Load this WHENEVER the user asks to "use the app", "open the app", "run the app", "test the app", "click around the app", record/screenshot the app on a phone, or otherwise exercise River on an Android device. Covers building the two APK variants, side-loading them on a plugged-in phone, launching them, driving the WebView UI with maestro, and capturing device screenshots / screen recordings / logcat.
---

# Android + maestro device testing

Build River's two Android variants, install them side-by-side on a plugged-in
device, and drive the in-app WebView with [maestro](https://maestro.mobile.dev/).

River's Android UI is the Dioxus app running in a wry/Android WebView. The
WebView **exposes its DOM text to accessibility**, so maestro can `tapOn` rooms,
buttons, and inputs by their on-screen text — not just by pixel coordinates.

## Environment

maestro needs a JDK; this repo's is managed by asdf (openjdk-25). The Android
build needs the SDK + NDK. Export these in every shell that runs `maestro` or
`cargo make build-android*`:

```bash
export JAVA_HOME="$(asdf where java)"          # /Users/<you>/.asdf/installs/java/openjdk-25
export PATH="$JAVA_HOME/bin:$PATH"
export ANDROID_HOME="$HOME/Library/Android/sdk"
export ANDROID_NDK_HOME="$(ls -d "$ANDROID_HOME"/ndk/* | sort -V | tail -1)"
```

Sanity check: `adb devices -l` (one `device` line), `maestro --version`,
`which dx b3sum`.

## Build both variants

The two variants install side-by-side (distinct applicationIds), so you can run
production and debug at once. They share the cargo target dir — **build them
sequentially, not concurrently.**

```bash
cargo make build-android         # PRODUCTION "River": release profile, no example
                                 #   data, applicationId org.freenet.river
cargo make build-android-debug   # "River Debug": dev profile, example data,
                                 #   applicationId org.freenet.river.debug
```

APK output paths (both are `assembleDebug`-signed; the variant differs by
profile/applicationId, not by signing config):

```
target/dx/river-ui/release/android/app/app/build/outputs/apk/debug/app-debug.apk   # production
target/dx/river-ui/debug/android/app/app/build/outputs/apk/debug/app-debug.apk     # debug
```

A cold production build is ~3 min; incremental and the debug build are much
faster. If you only changed UI source, that's enough; do NOT run `cargo make
sync-wasm` or touch WASMs unless the delegate/contract actually changed (see
`.claude/rules/wasm-safety.md`).

## Install and launch

```bash
adb install -r target/dx/river-ui/release/android/app/app/build/outputs/apk/debug/app-debug.apk
adb install -r target/dx/river-ui/debug/android/app/app/build/outputs/apk/debug/app-debug.apk
adb shell pm list packages | grep river        # org.freenet.river + org.freenet.river.debug

adb shell monkey -p org.freenet.river       -c android.intent.category.LAUNCHER 1   # production
adb shell monkey -p org.freenet.river.debug -c android.intent.category.LAUNCHER 1   # debug
```

## Verify the embedded node connected

Both variants boot an in-process Network-mode freenet node. Within ~10s of
launch logcat should show peers joining the ring:

```bash
adb logcat -d -t 200 | grep -iE "NAT traversal connection established|Outbound connection established|Connection progress"
```

A healthy boot reaches ~10/25 connections in 10s. In the UI, the home screen
shows a green **"Connected"** badge.

## Drive the WebView with maestro

Confirm maestro sees the WebView text before writing a flow:

```bash
maestro hierarchy | grep -oE '"(text|hintText)" : "[^"]+"' | sort -u
```

You should see UI strings like `"Welcome to River"`, `"ROOMS"`, room names, and
the composer hint `"Type your message..."`. Run a flow with:

```bash
maestro test .claude/skills/android-maestro/river_webview_demo.yaml
# screenshots land wherever the flow's takeScreenshot paths point
```

### Gotchas (learned the hard way)

- **Wait for the WebView to paint before the first tap.** Put
  `assertVisible: "Welcome to River"` immediately after `launchApp`. maestro's
  assert blocks until visible (default ~10s); tapping before that races the
  WebView load and silently misses.
- **The hamburger is an icon-only `Button` with no a11y text** → tap by point.
  Its position differs by view: welcome-screen header ≈ `4%,5%`; room
  conversation header ≈ `7%,9%`. (The room view's header is taller because it
  carries the room title + description line.) Verify on a new device by dumping
  clickable header bounds from `maestro hierarchy`.
- **Room labels carry a live unread count** (e.g. `"Public Discussion Room 56
  unread messages"`) that changes between launches. Match with a regex prefix:
  `tapOn: { text: ".*Public Discussion Room.*" }`.
- **Not every room has a composer.** Example-data rooms you are NOT a member of
  show a "You're not a member of this room yet" notice with a Copy-key button
  instead of an input. **"Your Private Room"** is owned by the example identity
  and has the `"Type your message..."` composer + `Send` button — use it to
  demo typing.
- **Example data regenerates each launch** (debug build), so author names and
  message bodies are not stable assertion targets. Assert on stable chrome
  (room title, `Send`, `Copy`) or on a string you typed yourself.

## Screen-record a click-through (for sharing)

Record the device screen with `adb shell screenrecord` while a maestro flow
drives the UI, then pull the mp4. **Keep the video out of the repo** — write it
to `/tmp/claude/` (or `$TMPDIR`), never under the working tree, and never
`git add` it.

```bash
# 1. Start recording on the device (background). --time-limit caps it; we stop early.
adb shell screenrecord --bit-rate 8000000 --time-limit 120 /sdcard/river_demo.mp4 &

# 2. Drive the UI so there's something to watch.
maestro test .claude/skills/android-maestro/river_webview_demo.yaml

# 3. Stop screenrecord cleanly (SIGINT finalizes the mp4 — a kill -9 corrupts it).
adb shell pkill -INT screenrecord
sleep 2

# 4. Pull it off the device, then clean up.
adb pull /sdcard/river_demo.mp4 /tmp/claude/river_demo.mp4
adb shell rm /sdcard/river_demo.mp4
```

`screenrecord` records the raw framebuffer, so maestro's tap overlays are NOT
in the video — it shows the app reacting on its own, which is what you want for
an external share.

## Open a URL in the device's browser

`127.0.0.1` means the **phone's** localhost (e.g. the embedded node's local
ports), not the dev machine's. Open it in Chrome on the device and screenshot:

```bash
adb shell am start -a android.intent.action.VIEW -d 'http://127.0.0.1:7509' com.android.chrome
adb exec-out screencap -p > /tmp/claude/device_browser.png
```

## See also

- `river_webview_demo.yaml` (this dir) — the reference flow: launch → open
  drawer → read a public room → open the owned room → type and send a message.
- `AGENTS.md` "Android (Mobile) Builds" — how the port, overlay, and bundled
  node fit together.
- `openspec/changes/android-bundled-node/` — the change this workflow validates
  (§8 acceptance scenarios).
