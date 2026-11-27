# Browser tab grid (Share/Create)

**Status:** landed on `browser/main`.
**When to read:** changing Browser tab state, the Share grid, card FLIP,
bookmark UI, or Browser screen/back behavior.

## Shape

- `src/ui/browser/grid.rs` owns the pure grid geometry, `GridState` (mode,
  focus, frozen visual order, scroll offsets, FLIP), card/bookmark rendering,
  and the pull-model transition ticker.
- `src/app/browser.rs` owns the reducer: tab open/switch/close, grid focus
  movement, bookmark toggles, replay actions, and the transition-complete
  message.
- `src/ui/browser/state.rs` owns the tab surface (`order`/`stored`/live page),
  full-strip restore (`restore_saved_strip`), and bookmark persistence helpers.
- `src/app/launcher.rs` owns the engine-facing tab operations
  (`open_browser_tab`, `activate_browser_tab`, `close_browser_tab`).

## Contract

- Home -> Browser opens the **tabs view** (not the page). **Share/Create**
  toggles page<->tabs; **Circle** from the page returns to the tabs view, and
  Circle from the tabs view returns Home. Create in the tabs view still exits
  to the page; the last remaining tab is never removed (closing it resets that
  tab to the default page and opens the page view).
- Tabs view is headerless, fixed **4 columns � 3 rows**. Slot 0 is ALWAYS the
  plus card, followed by up to 11 tabs (frozen visual order, active first).
  A 12th tab starts page 2; pages are swapped with the side chevrons, L1/R1,
  or replay `grid.page_next`/`grid.page_prev`.
- D-pad **and left analog stick** move card focus through the shared per-pad
  `NavBank` (MENU deadzone, repeat, neutral grace, and post-open arming); the
  left stick is shaped by `NavProfile::MENU` and the right stick is excluded.
  Cross opens, Square closes, Triangle bookmarks. Down from the bottom row
  enters the bookmark strip. Right stick scrolls bookmarks horizontally.
  The bank arms immediately when the entrance FLIP lands, and a direction
  pressed during the FLIP is buffered, so the first post-transition move is
  never swallowed.
- A footer hint island (Launcher glass style, full **pill** radius) shows
  Open/Close/Bookmark/Back and Page hints; each card has an overlaid bottom
  title footer with ellipsis.
- **One cue per action.** Opening/closing the tabs view, paging, moving focus,
  bookmarking, and selecting a card each emit exactly one `play_ui_cue` (sound
  and haptics). The plus card/new-tab path and bookmark navigation already cue
  from the tab/navigation call, so their grid close is silent
  (`close_browser_grid_with(..., false)`); selecting a card cues when the FLIP
  starts and the late tab activation stays silent.
- **No engine resize.** The FLIP overlay draws the tab's existing `FrameSlot`
  texture at an animating rectangle; card aspect equals the live page viewport
  aspect. Entering shrinks the page into the active card; selecting a card
  expands that tab's last frame to full page, then activates it. The animated
  rect is sampled in `FlipPage::draw` under a renderer transform, not when the
  view is built: the view tree is stable across the transition, so a view-time
  clock would freeze at 1–3 poses. The transform maps the child's **absolute**
  page rect to the absolute card rect (not a screen-origin scale), so the
  page's y offset interpolates for the whole FLIP instead of snapping on the
  last frame.
- The Browser header is part of the same transition. `HeaderCurtain` draws the
  normal header over the tabs view and **slides it up** into its row while
  compositing a card-background curtain that travels with it; entering (tabs)
  lifts the header out as it fades, leaving drops it back in from above. The
  curtain alpha fades faster than linear (`1 - appear²`) so the content is
  clearly gone before the animation ends. The header area sits above every grid
  card, so the curtain never hides card content.
- Never render the same tab id twice in one frame (`BrowserFramePipeline`
  stores one uniform block per tab id): the animating tab's grid card omits its
  shader until the transition lands.
- Background cards show their last painted frame. Restored tabs stay dormant
  (`StoredTab.loaded == false`) until first activation; their cards show a
  title/URL placeholder.
- Bookmarks are `bookmarks.json` entries matched by exact URL. The top-right
  star is filled/warning when the tab's current URL is bookmarked, outline
  otherwise. Triangle on a focused bookmark removes it. Opening a bookmark
  creates a new active tab for its URL (it never replaces the current page).
- Reduced motion snaps between page and grid; the normal path uses the
  `GridTicker` wrapper to request display-rate redraws without a global timer.

## Tab persistence

`tabs.json` stores URL/title plus the active index. Up to 25 saved tabs are
supported (`MAX_SAVED_TABS`); the tabs view paginates at 11 per page.
`restore_saved_strip`
restores every saved tab: the active page uses `TabId::BOOT` (the engine
preloads its URL), others become dormant stored pages. Tab open/close/switch
rewrites the file; `tabs_restored` still gates the boot-time engine events that
would otherwise clobber the saved strip before restore.

## QA

```bash
python scripts/qa_browser_grid.py
```

Synthetic replay only (no OS input): local pages, `grid.open`, screenshot the
grid geometry, `grid.bookmark`, `grid.new_tab`, `grid.activate`, and
page/grid screenshots. `ReplayWait::needs_grid_idle` keeps step boundaries
after the FLIP lands.
