# Glossary

Terms used across the docs, source, and config schema. Each entry
points at the doc that explains it in depth; this page is just
for "what does X mean again" lookups.

## A

### AppStream

The freedesktop metadata format catalogued by app stores
(`gnome-software`, KDE Discover, Flathub). Our file is
[`packaging/io.github.dtg01100.llm-quota-tray.metainfo.xml`](../packaging/io.github.dtg01100.llm-quota-tray.metainfo.xml);
its contract is documented in
[`freedesktop-integration.md`](freedesktop-integration.md).

### Auth style

How the daemon authenticates against the provider's API. The
currently supported styles (`bearer`, `header`, `custom`,
`query_param`) are documented under
[`AuthConfig` in `config-schema.md`](config-schema.md#authconfig-providerrs185) —
each style maps to one set of HTTP headers / request body the
provider expects.

## B

### Bucket

The color category the chip / launcher icon is in based on the
worst window's remaining quota. Three ranks: `Normal`, `Warn`,
`Throttled`. Computed by
[`icon::bucket_for`](../src/icon.rs) from a `remaining_pct` value
and the burn projection. See
[`Thresholds` field in `config-schema.md`](config-schema.md#thresholds-configrs75)
for the configurable cutoffs.

### Burn rate

The slope of quota consumption over time, projected forward to
estimate when the window runs out. The math (least-squares
slope per minute, rolling 6-h window, day-rollover epoch
suppression) is documented in [`burn-rate.md`](burn-rate.md).

### Burn row

The "if you keep this up you'll run out in X" line that appears
in the menu for each window when burn-rate projection is
enabled and there's enough data.

## C

### Chip

The tray icon. Not to be confused with the *launcher* icon
(which is the static PNG/SVG you see in app launchers and the
autostart UI — they're a matched pair, see
[`freedesktop-integration.md`](freedesktop-integration.md)).
Visually the chip is the same arc-and-dot as the launcher, but
the arc length and color track the live remaining quota.

## D

### dbusmenu

The freedesktop menu protocol over D-Bus
(`com.canonical.dbusmenu`). We expose one at `/Menu` so the SNI
host can render context menus. Documented in
[`freedesktop-integration.md`](freedesktop-integration.md).

### dbus service

A well-known D-Bus bus name an application owns (e.g.
`org.kde.StatusNotifierItem-1234-1`). We acquire one per
daemon instance so the SNI watcher can auto-discover us.

## E

### Epoch

A daily reset boundary (00:00 UTC) used by burn-rate
calculations to forget stale samples. See the "Epoch rollover"
section of [`burn-rate.md`](burn-rate.md).

## F

### Fixed window

A quota window that resets at an absolute wall-clock time
(e.g. "resets at 2026-08-23T18:00:00Z"). Contrast with
**roll** below. See
[`config-schema.md` — WindowShape](config-schema.md).

## I

### Instance

A named variant of the daemon running concurrently with the
default. Each instance has its own config file, PID lock, keyring
entry, and tray chip. Documented in
[`multi-instance.md`](multi-instance.md).

## L

### Legacy attribute

The old keyring attribute name (`org.freedesktop.Secret.Generic`
+ matching `xdg:schema` value) used in pre-0.3.0 versions. The
current attribute is `application llm-quota-tray`. The keyring
code transparently reads both for migration. See
[`troubleshooting.md`](troubleshooting.md#llm_api_key-works-but-keyring-doesnt).

## M

### Menu shape

The structure of the dropdown menu — what rows appear, in what
order, with what separators. The default shape (header, per-
window rows, footer with status / refresh / dashboard / set-key
/ quit) is documented in
[`architecture.md` — menu tree](architecture.md).

## P

### Parser / parse plan

The data-driven descriptor that says "this provider returns a
JSON document shaped like X; extract these fields into
`Vec<Window>`". Defined in [`src/provider.rs`](../src/provider.rs)
(generators) and [`src/parse.rs`](../src/parse.rs) (plan +
driver). See [`port-guide.md`](port-guide.md) for how to add
one.

### Pixel-perfect ring

The arc + center dot composition the chip renders. The
geometry (240° outer track starting at 12 o'clock growing
clockwise, 8-o'clock terminus) is documented inline in
[`src/icon.rs`](../src/icon.rs).

### Provider template

A `config.json` (and optional sidecar files) that adapts the
daemon to a new quota API without writing Rust. The
`examples/providers/` directory ships 13 templates.

## R

### Roll window

A quota window that resets after a duration from the first use
(e.g. "5h after I started using it"). Contrast with
**fixed** above. See
[`config-schema.md` — WindowShape](config-schema.md).

## S

### Sentinel

The throwaway request the daemon sends to detect whether the
API key in the keyring is still valid (typically a single
cheap `GET` against a "whoami" or low-cost endpoint). A 401
triggers a "Set API Key…" prompt; a 200 keeps the daemon
silent. See [`src/fetch.rs`](../src/fetch.rs).

### Shape (window shape)

The combined `(plan, kind)` enum that determines how a window's
reset time is computed. Documented in
[`config-schema.md` — WindowShape](config-schema.md).

### Sidecar

An auxiliary file the provider template may load (typically for
"hard" tracks where the JSON parsing requires auxiliary state —
e.g. pricing tables). Lives next to the config file as
`<instance>.sidecar.json`. Pattern is documented in
[`port-guide.md`](port-guide.md#when-you-need-a-sidecar).

### SNI (StatusNotifierItem)

The freedesktop tray-icon protocol over D-Bus. We speak it
directly via `zbus`, no libappindicator. Spec at
<https://www.freedesktop.org/wiki/Specifications/StatusNotifierItem/>;
our integration is documented in
[`freedesktop-integration.md`](freedesktop-integration.md).

## W

### Watcher

The SNI host process that aggregates items from all apps on
the session bus and shows them in the panel. On GNOME it's
the `appindicator` extension's `gnome-shell` actor; on KDE
it's `plasma-workspace`'s `statusnotifierwatcher`. Our watcher-
recovery logic is documented in
[`freedesktop-integration.md` — SNI watcher restarts](freedesktop-integration.md#sni-watcher-restarts-are-handled-in-flight).

### Window

One quota bucket the provider returns. The daemon displays one
row per window in the menu and uses the **worst** window's
remaining percentage to drive the chip's bucket color.
Shape and structure documented in
[`config-schema.md` — Window](config-schema.md).
