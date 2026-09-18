<div align="center">

<img src="assets/icon.svg" width="112" height="112" alt="Cota">

# Cota

Your Claude usage limits, in the Windows tray.<br>
A ring that fills as your week does, so you find out before you hit the wall.

<sub><i>cota</i> — Portuguese for <i>quota</i></sub>

</div>

A single 0.99 MB executable, no runtime dependencies. Sits in the tray using
about 5 MB and no measurable CPU.

---

## What it does

Claude Code tells you your limits when you ask it — `/usage`, in a session you
already have open. Cota is the same numbers when you *didn't* ask: a ring in
the notification area that goes amber at 75% and red at 90%, so "I have two
days of weekly left" is something you know rather than something you check.

| | |
|---|---|
| **The ring** | Fills with the limit nearest its ceiling. Green, amber, red. |
| **The panel** | Left-click: the fullest limit large and in colour, the rest as compact bars. |
| **The menu** | Right-click: the commands.  |
| **Projection** | *At this rate: full in 3h 20m*, from the slope of your own usage. |
| **Attribution** | Which project ate the week, read from the local transcripts. |
| **Toasts** | At 50%, 80% and 95%, once each per limit per window. And when a limit you were actually near resets. |

**Left button** opens the panel — the numbers. **Right button** opens the menu
— the commands. Running `cota.exe` when it is already in the tray makes the
running instance toast its current numbers instead of starting a second one, so
the exe doubles as a one-shot "where am I" command.

The panel is painted from the last reading and never goes to the network, so
opening it is instant and free however often you do it.

Cota never makes two requests within ten seconds of each other, whoever asks.
A refresh that arrives inside that window replays the last reading and says how
old it is, rather than going to the network — the limits do not move fast
enough for a second request to be able to tell anyone anything new, and the
endpoint rate-limits hard enough that asking is actively counterproductive.

## Install

```powershell
winget install kidchenko.Cota
# or
choco install cota
```

Or download it directly:
[**Cota-Setup.exe**](https://github.com/kidchenko/cota/releases/latest/download/Cota-Setup.exe).
Every release also carries a versioned copy and `cota-x.y.z-x64.exe`, which runs
without installing — see [Releases](https://github.com/kidchenko/cota/releases).

`Cota-Setup.exe` is an unversioned alias of the same installer, published in
every release so that `/releases/latest/download/` keeps resolving. `build.ps1`
writes it alongside the versioned one.

The binary is not code-signed, so SmartScreen warns on first run. Click
**More info**, then **Run anyway**.

**Prefer the installer if you want toasts to say "Cota".** Windows resolves a
notification's name and icon from an `AppUserModelID` registered against a Start
Menu shortcut, and only the installer creates one. A portable copy falls back to
borrowing PowerShell's registration — the toast still appears, but under
PowerShell's name. The app checks for the shortcut rather than guessing, because
Windows offers no way to ask whether an id is known: it simply declines to show
the toast, silently.

### Requirements

Claude Code, signed in at least once on this machine. Cota reads the token
Claude Code already stores and never asks you for a credential of its own — see
[Credentials](#credentials).

## Where the numbers come from

Two different places, and the difference matters.

**The limits** come from `GET https://api.anthropic.com/api/oauth/usage`, the
endpoint Claude Code's own `/usage` reads. It reports a percentage and a reset
time per bucket. This endpoint is **undocumented** and will change without
notice — see [When it breaks](#when-it-breaks).

**The projection** comes from Cota's own samples of that percentage over time,
fitted with least squares. It deliberately does *not* come from counting local
tokens. Subscription buckets are opaque server-side counters — cache reads,
model tier and effort all weigh differently and none of those weights are
published — so any local token total is a proxy whose error can't be bounded.
Sampling the percentage sidesteps the question entirely: the slope is right
regardless of how the weighting works, and it keeps being right on the day the
weighting changes.

**The attribution** is the one estimate in the app, and it is labelled as one.
It reads `~/.claude/projects/**/*.jsonl`, weights tokens by the published Opus
price ratios, and reports a *share between projects* — never a share of a
limit. Treat it as "where did the week go", not as a number that adds up to
anything.

## Cost

The transcript corpus grows forever and is already tens of megabytes. Cota
treats the files as what they are — append-only logs — and reads each from a
remembered byte offset, so only the first scan after a start reads everything:

```
transcript baseline: 73 files, 72 MB, 93 ms
```

That is a warm file cache. Cold it is around 3.3 seconds — the scan is
dominated by disk, not by parsing, so treat the warm number as the steady state
and the cold one as what a login costs once. Either way it is on the background
thread and the tray is live throughout.

After the baseline a scan reads only what was appended, every five minutes. The
limit poll is one request a minute. Idle cost is a rounding error.

Lines are parsed into a four-field struct rather than a `serde_json::Value`: a
single assistant line can carry megabytes of `content`, and building an owned
tree for all of it only to drop it is a lot of allocator traffic for four
numbers. It does not make the cold scan meaningfully faster — that is I/O — but
it keeps the steady-state cost near nothing.

## Configuration

`%APPDATA%\Cota\config.json`, created on first save. Edits are picked up
within a poll — no restart.

```json
{
  "enabled": true,
  "pollSeconds": 60,
  "thresholds": [50, 80, 95],
  "notify": true,
  "userAgent": "claude-code/2.1.236",
  "projection": true,
  "attribution": true
}
```

`pollSeconds` is clamped to 20–900. `userAgent` is not cosmetic: the endpoint
rate-limits aggressively without a `claude-code/...` agent string, and this is
here so it can be bumped without a rebuild.

## Credentials

On Windows, Claude Code stores its OAuth token as plain JSON at
`~/.claude/.credentials.json`. Cota reads that file and sends the token to
`api.anthropic.com` — the service that issued it — and to nowhere else. It is
the only host Cota contacts.

Cota does **not** cache the token and does **not** implement OAuth refresh,
though the file has everything needed to. Claude Code already refreshes it, so
re-reading the file each poll gets the same answer for none of the code and
none of the risk of two processes racing to spend one single-use refresh
token.

`CLAUDE_CONFIG_DIR` is honoured if you have moved that directory.

TLS goes through schannel and the Windows certificate store rather than a
bundled root set, and the system proxy is respected. On a managed work machine
where the proxy intercepts TLS, bundled roots fail with an opaque handshake
error; the machine's own store knows about that root.

## Two surfaces

The data and the commands live apart, and that was not the first design.

Originally the menu held both: the limits as disabled items above a separator,
the commands below. It worked, and it was wrong — Windows paints a disabled
menu item grey, so the numbers, which are the entire product, rendered dimmer
than "Quit Cota". A menu offers exactly one text colour and one weight; there
is no way to make anything in it stand out.

So the numbers moved to a panel that can have colour, weight and bars, and the
menu kept what menus are good at. The panel is plain GDI on a borderless popup
— Cantos pays for WebView2 because configuring twenty-one actions across four
corners really is an HTML problem, but this is a few rows of text and some
rectangles, and a browser engine to draw them would cost more than the rest of
the app put together.

The panel leads with one number: the limit nearest its ceiling, set large and in
its severity colour, with the others below it as compact rows. An earlier draft
gave all three limits equal weight and the eye had nowhere to land — three
identical clusters, none of them answering "am I about to be stopped".

Two details worth the trouble:

- **Text sits on baselines taken from the font**, not centred in a box with a
  hand-tuned nudge per size. The nudge approach cannot work: the correction
  depends on metrics that change with the font and the DPI, and a 34-pixel
  number next to a 13-pixel label visibly floated above it.
- **One `Layout` pass produces every coordinate**, and both the height
  calculation and the paint consume it. They were separate arithmetic once, and
  drifted — every tweak had to be made twice or the panel grew dead space at the
  bottom. A test now asserts the height always clears the lowest element and
  never by more than one padding.

## Notification policy

Cota interrupts rarely and on purpose.

**Thresholds** fire once per limit per window, on the crossing only — sitting
above 80% for two days is one toast, not two thousand.

**Resets** are only announced for a limit that was at 50% or more when it
rolled over. The session bucket resets every few hours all day; being told your
allowance is back when you had used 9% of it is noise, and noise is what makes
people turn notifications off. If it was blocking you, you hear about it.

**Failures** are said once. A repeat of the same error is suppressed — the ring
goes grey and the menu says why, and neither of those interrupts.

One other thing worth knowing about, because it caused a bug worth not
repeating: the server computes `resets_at` per request, so its sub-second part
drifts between polls — `08:00:00.956` on one, `08:00:01.241` on the next. Cota
compares reset times with a two-minute tolerance and only treats a *forward*
move as a new window. Comparing them for equality, as an earlier version did,
reported a rollover on roughly every other poll: a false "limits reset" toast,
the projection's samples wiped before they could ever span ten minutes, and
every threshold re-armed.

## When it breaks

The endpoint is a live experiment surface. Alongside the documented-by-nobody
real fields it currently returns keys called `nimbus_quill`,
`iguana_necktie` and `cedar_ember`, all null. Cota is written for that:

- every field is optional, and unknown fields are ignored — a new key must
  never break the app;
- `limits[]` is primary, with the older `five_hour` / `seven_day` objects as a
  fallback if it disappears;
- the codename keys are never read, by anything;
- the server's `severity` is trusted but not relied on — Cota takes the worse
  of what the server says and what the percentage implies, so a new grading
  scheme can only ever make it more cautious.

When it does break, the icon goes to a grey dashed ring and the menu says why.
Set `COTA_LOG=debug` and the full response body lands in
`%APPDATA%\Cota\cota.log`, which is the one thing you need to fix it.

## Build

```powershell
.\build.ps1              # test + release binary
.\build.ps1 -Run         # ... then launch it
.\build.ps1 -Icons       # ... render the icon faces to a contact sheet
.\build.ps1 -Panel       # ... hold the panel on screen to look at it
.\build.ps1 -Panel -Theme light
.\build.ps1 -Shot        # ... re-render docs\img\panel.png
```

Needs the Rust MSVC toolchain. Nothing else — no WebView2, no installer
tooling.

The `-Icons` flag exists because the ring is only ever visible at 16×16 in the
corner of a taskbar, which is a poor place to notice that an arc runs the wrong
way. It dumps every face from the real renderer — including the Claude mark —
and composites them over both taskbar colours.

`-Panel` is the same idea for the popup: it fills the panel with representative
content and pins it on screen for twenty seconds, suppressing the
dismiss-on-focus-loss that otherwise makes a popup impossible to inspect.

`-Shot` renders the landing page's picture of the panel. It does **not** take a
screenshot, and that is the point. A real window sits over a real desktop, DWM
draws a soft shadow around it, and `GetWindowRect` returns a rectangle that
includes that shadow margin — so whatever was behind the window bleeds through
and the crop arrives with faint rectangles of other applications ghosted around
the edges. Masking the corners hides some of it and none of the rest.

Instead the same `draw` that paints the live panel runs against a DIB section:
no screen involved, nothing behind to bleed, reproducible, and rendered at 3x
where a capture is stuck at whatever DPI the machine happens to run. The corners
are cut to transparent afterwards at the radius DWM would have used, so the
page's shadow can follow the alpha rather than tracing a box around it.

Both the ring and the mark are drawn in code rather than shipped as sprites.
The ring because it is a continuous readout and a hundred PNGs would be silly;
the mark because it is sized from `SM_CXMENUCHECK` at startup, so it is sharp
at whatever DPI the machine happens to be running rather than at 100% only.

## Not yet

- **macOS.** The port is mostly free — `tray-icon`, `muda` and `tao` are all
  cross-platform and Cota needs almost no Win32 — but credentials live in the
  Keychain there, not a file, and autostart is a LaunchAgent rather than a
  registry key. Two files split; the rest is shared.
- **A history view.** The samples are already on disk in `state.json`.

## License

MIT.
