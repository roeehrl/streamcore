# Built-in app profile presets

These files are embedded in `streamcore-input` (`include_str!` in `src/presets.rs`) and
are the presets every Pad app uses through the core. They were taken unchanged from
Pad's built-in presets, where PadWin's tests prove each one
validates; from now on this directory is the source of truth; Pad's copies go
away when its apps load the presets through the core. A byte-identical copy ships
with the fixtures in `protocol/input/fixtures/presets/builtin/` for app
conformance runners; after editing a file here, copy it there (`cp
crates/streamcore-input/presets/*.json protocol/input/fixtures/presets/builtin/`), as
`conformance.rs` fails while the two differ. `cargo test -p streamcore-input` checks that every file loads,
validates strictly, round-trips to the same JSON, carries `updatedAt`
`2000-01-01T00:00:00Z` and has a `fallbacks.redo`.

Spot checks against the official pages on 10–11 October 2026 (all matched the
files): Rebelle 8 (tools E, I, W, O, Y, F, H, [ ], ⌘0 fit, ⌘⌥0 reset rotation,
⇧F flip, ⌘⌥←/→ rotate, ⌘+ / ⌘− zoom, ⇧⌘Z redo), Krita navigation (⌘[ / ⌘] rotate,
5 reset rotation, M mirror; zoom and gestures checked again on 21 October 2026, below), Clip Studio Paint (⌘ keypad +/− zoom, − and ^ rotate in
5° steps, ⇧⌘N, ⌘T/⇧⌘T, ⌘D, ⌘0, redo ⌘Y or ⇧⌘Z), Affinity Photo 2 (B, E, I, L, V,
[ ], ⇧⌘N, ⌘0, ⌃⇧⌘R). The Goodnotes help page refused automated access (HTTP 403);
its row relies on the earlier reading recorded below.

The redo fallback of `default.json` (`"*"`) is ignored on Windows by
`redo_shortcut`: Windows apps differ (Ctrl+Y or Ctrl+Shift+Z), so apps without
their own profile use PadWin's per-app list instead.

## Format and sources (from Pad)

One preset per file; the file name is the preset id. Every file follows
Pad's profile schema. `updatedAt` is fixed at
`2000-01-01T00:00:00Z` so that any edit a user makes wins over the built-in copy.

Modifier bits: shift 1, control 2, option/alt 4, command 8, where 8 is the primary
modifier (⌘ on a Mac, Ctrl on Windows). Shortcuts are the Mac defaults; where Windows
differs by more than ⌘→Ctrl, the shortcut carries a `hosts.windows` entry with the
Windows key (noted below).

| File | App | appKey | Aliases |
|---|---|---|---|
| `default.json` | All other apps | `*` | none |
| `photoshop.json` | Adobe Photoshop | `com.adobe.Photoshop` | `photoshop.exe` |
| `illustrator.json` | Adobe Illustrator | `com.adobe.illustrator` | `illustrator.exe` |
| `goodnotes.json` | Goodnotes 5/6 (Mac App Store, Catalyst) | `com.goodnotesapp.x` | `com.goodnotesapp.goodnotesmac` (the old GoodNotes 4 Mac app) |
| `notability.json` | Notability (Mac App Store, Catalyst) | `com.gingerlabs.Notability` | none |
| `rebelle.json` | Rebelle | `com.escapemotions.rebelle` | `rebelle 8.exe`, `rebelle 7.exe`, `rebelle.exe` (unverified) |
| `krita.json` | Krita | `org.krita` (krita.org download) | `org.kde.krita` (Mac App Store), `krita.exe` |
| `clipstudiopaint.json` | Clip Studio Paint | `jp.co.celsys.CLIPSTUDIOPAINT` | `clipstudiopaint.exe` |
| `affinity-photo.json` | Affinity Photo 2 | `com.seriflabs.affinityphoto2` | `com.seriflabs.affinityphoto` (v1), `photo.exe` |
| `affinity-designer.json` | Affinity Designer 2 | `com.seriflabs.affinitydesigner2` | `com.seriflabs.affinitydesigner` (v1), `designer.exe` |
| `affinity.json` | Affinity (the free all-in-one app by Canva, 2025, v3) | `com.canva.affinity` | `affinity.exe` |

## Where the bundle IDs and exe names come from

- Homebrew cask API (each cask's `zap`/`uninstall` stanzas): `krita`
  (`org.krita.savedState`), `clip-studio-paint` (`jp.co.celsys.CLIPSTUDIOPAINT.*`),
  `affinity-photo`, `affinity-designer`, `affinity-photo@1`, `affinity-designer@1`,
  `affinity` (`com.canva.affinity`). https://formulae.brew.sh/api/cask/krita.json and so on.
- App Store lookup API: `com.goodnotesapp.x` (id 1444383602), `com.gingerlabs.Notability`
  (id 360593530), `org.kde.krita` (id 1594607976).
  https://itunes.apple.com/lookup?id=1444383602
- Info.plist values: https://doesitarm.com/app/photoshop, https://doesitarm.com/app/illustrator,
  https://doesitarm.com/app/krita, https://doesitarm.com/app/clip-studio-paint;
  Rebelle 4.1.1 and GoodNotes 4 Mac: https://rosettacheck.com/apps/4p6m5ms7,
  https://rosettacheck.com/apps/com.goodnotesapp.goodnotesmac.
- Windows: `CLIPStudioPaint.exe`, https://support.clip-studio.com/en-us/faq/articles/20190086;
  `Photo.exe` and `Designer.exe` (Affinity v2 MSIX), https://lenscraft.co.uk/photo-editing-tutorials/wheres-the-affinity-photo-2-exe/;
  `Affinity.exe` (v3, `...\WindowsApps\Canva.Affinity_...\App\Affinity.exe`), from press and
  forum reports, not an official page. `photoshop.exe` and `illustrator.exe` are kept from the earlier presets.

## Where the shortcuts come from

| Preset | Official source |
|---|---|
| photoshop | https://helpx.adobe.com/photoshop/using/default-keyboard-shortcuts.html (Adobe now redirects this page to a stub; read from the Wayback Machine copy of 23 March 2025) |
| illustrator | https://helpx.adobe.com/illustrator/using/default-keyboard-shortcuts.html |
| goodnotes | https://support.goodnotes.com/hc/en-us/articles/17220521238159 |
| notability | https://intercom.help/notability/en-us/articles/16300139-keyboard-shortcuts |
| rebelle | https://escapemotions.com/products/rebelle/manual/8/keyboard-shortcuts/ |
| krita | https://docs.krita.org/en/user_manual/getting_started/navigation.html, https://docs.krita.org/en/reference_manual/tools/freehand_brush.html, and the defaults in Krita's source (`krita/krita.action`, `krita/kritamenu.action`, tool factories under `plugins/tools/`) at https://invent.kde.org/graphics/krita |
| clipstudiopaint | https://help.clip-studio.com/en-us/manual_en/780_shortcuts/Menu_Shortcuts.htm, https://help.clip-studio.com/en-us/manual_en/780_shortcuts/Tool_Shortcuts.htm, https://help.clip-studio.com/en-us/manual_en/270_canvas/Navigating_the_canvas.htm |
| affinity-photo | https://affinity.help/photo2/en-US.lproj/pages/Workspace/shortcuts.html |
| affinity-designer | https://affinity.help/designer2/en-US.lproj/pages/Workspace/shortcuts.html |
| affinity | No shortcut page for v3 yet (affinity.help covers only v1/v2). Uses the shared v2 Photo and Designer defaults. |

## Gestures (`gestures.delivery`)

- **native**: Photoshop (pinch zoom and twist-to-rotate when Preferences › Tools › Enable
  Gestures is on; from Adobe community answers, not a help page). Illustrator (pinch zoom works on
  Mac according to community answers; whether rotate works is unknown). Affinity Photo 2, Designer 2 and
  Affinity v3 (pinch zoom and rotate about the cursor, documented at
  https://affinity.help/photo2/en-US.lproj/pages/Extras/trackpads.html; assumed the same in v3).
- **shortcuts**: Rebelle. Its own multitouch handling has known macOS trackpad problems
  (Escape Motions forum), and it has documented zoom and rotate shortcuts.
  Krita (since 21 October 2026, was native). On Windows, Krita 6.0.4 handles two-finger touch
  (`[Zoom and Rotate Canvas]` TwoFingerDrag in `kritadefault.profile`) only until the pen is
  used: on Windows 11 ARM64 with PadWin 1.3 and Krita 6.0.4 x64, a native pinch zoomed
  (51.9 % to 64.9 %) and a twist rotated (86°) a freshly started Krita, and after one pen
  stroke neither did anything, even after the pen left range, while Edge still zoomed with the
  same touch input. Users report the same on real hardware
  (https://krita-artists.org/t/touchscreen-and-gestures-not-working-after-using-stylus/157877,
  https://krita-artists.org/t/touch-gesture-controls-not-working-properly-in-krita-6-0-nightlies/124583).
  Before any pen input, Krita's default "Touch painting: Auto" also paints a dot under the
  first contact. On macOS the native magnify/rotate events work (`[Zoom Canvas]`/`[Rotate
  Canvas]` native gestures since 5.1.0; "Native macOS touchpad gestures should now work
  properly" in 5.1.1, https://krita.org/en/posts/2022/krita-5-1-1-released/), but the delivery
  is one value for both platforms, so Krita uses the shortcuts that work on both: ⌘/Ctrl+= and
  ⌘/Ctrl+− zoom (about the view centre, √2 per press), ⌘/Ctrl+[ and ] rotate 15°. A Mac user
  can switch the profile to native for smooth, cursor-anchored zoom. The pan is
  `gestures.pan` `middleDrag` (since 22 October 2026): with shortcuts delivery a two-finger pan
  used to reach Windows as the mouse wheel, which Krita's default canvas profile reads as zoom.
  Krita pans with a middle-button drag or Space+drag on every platform
  (https://docs.krita.org/en/user_manual/getting_started/navigation.html), so the host drags
  with the middle button, released around each zoom or rotate shortcut (Ctrl or Shift with the
  middle button is Krita's zoom and rotate drag). On macOS the phased trackpad scroll also
  panned, and the middle-button drag pans the same way there.
- **left out (default native)**: Goodnotes, Notability (Catalyst apps; no documentation on
  trackpad pinch), Clip Studio Paint (the manual covers touch gestures but not the Mac trackpad).

## Notes and unverified items

- **default**: Undo, Redo, Copy, Paste, Cut, Select All, Save. Redo uses ⇧⌘Z; many Windows
  apps use Ctrl+Y instead, and most accept both.
- **photoshop**: The keys match the old preset, plus Brush Size [ ]. There is no rotate-view
  step shortcut (R is the Rotate View tool), so there is no rotate fallback.
- **illustrator**: Kept Pen P, Selection V, Direct Selection A and Group ⌘G. Added Paintbrush B,
  Blob Brush ⇧B, Pencil N, Eraser ⇧E and [ ] (Blob/Bristle brush size). Zoom in is ⌘= (as
  documented), not ⌘+.
- **goodnotes**: Rechecked on 21 October 2026 through the help centre's API copy
  (https://support.goodnotes.com/api/v2/help_center/en-us/articles/17220521238159.json, updated
  2026-08-14, macOS/iPadOS/Windows/Web): the tool letters below and ⌘++ / Ctrl++ zoom in match.
  Changed **Lasso from L to V**. The official list now gives L = Laser and
  V = Lasso. P, E, H, S and T are confirmed. Goodnotes for Windows has no exe alias (not found).
  The old double-tap alternated Eraser and Pen, but the schema has one `doubleTap` action, so
  double-tap is now Eraser only. Squeeze is also Eraser.
- **notability**: ⌘1–⌘9 select a tool by its **position** in the toolbox, which the user can
  rearrange. The names Text/Pen/Highlight/Eraser/Lasso/Hand come from the old preset and assume
  the default order, which the docs do not list. Double-tap and squeeze are ⌘4 (Eraser),
  reduced from the Eraser/Pen alternation as for Goodnotes.
- **rebelle**: The shortcuts are verified (Mac column). The rotate step is not documented, so
  `rotateStepDegrees` is left at the default of 15. The bundle ID is verified only for Rebelle
  4.1.1; check that Rebelle 7/8 still use it. The Windows exe names are **guesses** based on the
  installer name "Rebelle 8.exe".
- **krita**: Ctrl in Krita's defaults is ⌘ on a Mac (Krita never sets
  `AA_MacDontSwapCtrlAndMeta`). Rotate is ⌘[ / ⌘] at 15° per press (`rotateCanvasLeft15`;
  4 and 6 also work; 5 resets). E toggles eraser mode. **Zoom in is Ctrl+= (`char` "=")**:
  `kritamenu.action` binds `Ctrl+=; Ctrl++` since 5.2.11 (only `Ctrl++` in 5.2.0–5.2.6), but on
  Windows US Krita 6.0.4 ignores Ctrl+Shift+= (what `char` "+" resolves to): tested on
  21 October 2026 with SendInput directly and through PadWin, the zoom stayed put, while
  Ctrl+= zoomed in. "=" is its own key on US, UK and French layouts and Shift+0 on German,
  which Qt's shifted-key matching still reads as Ctrl+=. Krita's 4.2.3 rotate feature was touch
  rotation for Android, not a macOS trackpad gesture.
- **clipstudiopaint**: Zoom uses ⌘ + keypad +/− (documented and independent of keyboard
  layout; ⌘; and ⌘- are the other documented keys). Rotate right is `^`, which the manual shows
  for the Japanese layout. On other layouts the host types whatever produces `^` (on US, Shift+6),
  and whether CSP accepts that is **unverified**. The 5° rotate step is the documented default.
  No brush-size [ ] key is listed in the manual, so there is none in the preset.
- **affinity-photo / -designer / affinity**: Reset rotation is ⌃⇧⌘R on Mac but
  Ctrl+Alt+Shift+R on Windows, and zoom in is ⌘+ on Mac but Ctrl+= on Windows (the
  `<x-osx>`/`<x-win32>` blocks of the v2 shortcut pages, rechecked on 22 October 2026; every
  other shortcut the presets use is the same with ⌘→Ctrl). Both carry a `hosts.windows` entry
  (modifiers 13 = Ctrl+Alt+Shift, and "=") since 22 October 2026. There is no
  rotate-step shortcut by default. In v3 the single-letter tools depend on the active studio
  (Pixel, Vector, Layout), and those letters are **unverified**.
- The `photo.exe` and `designer.exe` aliases are generic names that another app could also use.
- `zoomStep` is not documented by any of these apps, so it is left at the default of 0.25.
