# Browser tab grid (Share/Create)

**Status:** landed on `browser/main`.
**When to read:** changing Browser tab state, the Share grid, card FLIP,
bookmark UI, or Browser screen/back behavior.

## Shape

- `src/ui/browser/grid.rs` owns the strip geometry (`tabs_layout`,
  `tabs_card_geometry`), `GridState` (mode, focus, frozen visual order, scroll
  offsets, FLIP), card/bookmark rendering, the draw-time `CardShell` /
  `FlipPage` / `HeaderCurtain`, and the pull-model transition ticker.
- `src/app/browser.rs` owns the reducer: tab open/switch/close, grid focus
  movement/scroll, bookmark toggles, replay actions, and the
  transition-complete message.
- `src/ui/browser/state.rs` owns the tab surface (`order`/`stored`/live page),
  full-strip restore (`restore_saved_strip`), and bookmark persistence helpers.
- `src/app/launcher.rs` owns the engine-facing tab operations
  (`open_browser_tab`, `activate_browser_tab`, `close_browser_tab`).

## Contract

- Home -> Browser opens the **tabs view** (not the page). **Share/Create**
  toggles page<->tabs; **Circle** from the page returns to the tabs view, and
  Circle from the tabs view returns Home **and persists the focused card as
  the active tab**. Share/Create is page-only and is unbound in the tabs view.
- The tabs view is headerless and uses a **compact card** sized to its
  content: `tabs_card_geometry` takes the page card width × 0.70 and derives
  the height from the single card row + bookmark strip + footer island. The
  row's top inset equals its side inset (`GRID_PAD` inside the card padding).
  The page-mode and tabs-mode card rects are interpolated by `CardShell` on the
  same clock as the page FLIP, so entering/leaving the page resizes the frame
  smoothly instead of snapping.
- **One horizontal row** of page-aspect cards in a themed horizontal
  `scrollable`. Slot 0 is ALWAYS the plus card; tabs follow **newest first**
  (`order` reversed). A tab is never reordered by being entered. D-pad focus
  calls `reveal_offset` so the focused card scrolls fully into view; the right
  stick scrolls the row. A reserved 9px scrollbar lane below the cards holds a
  6px bar when the strip overflows, so showing/hiding it never reflows the
  cards or bookmark strip. There is **no pagination and no L1/R1 paging**.
- Closing a tab **keeps the selected index** (`focus_after_close`): the next
  card slides into the freed slot; closing the last card selects the new last
  card; closing the final remaining tab keeps that tab (it resets to the
  default page, zero tabs are not allowed). Closing always plays exactly one
  `play_ui_cue` (sound + haptics); `close_browser_tab` itself is silent.
- D-pad **and left analog stick** move card focus through the shared per-pad
  `NavBank` (MENU deadzone, repeat, neutral grace, and post-open arming); the
  left stick is shaped by `NavProfile::MENU` and the right stick is excluded.
  Cross opens, Square closes, Triangle bookmarks. Down enters the bookmark
  strip; Up returns to the row card that had focus. The bank arms immediately
  when the entrance FLIP lands, and a direction pressed during the FLIP is
  buffered, so the first post-transition move is never swallowed.
- A footer hint island (Launcher glass style, full **pill** radius) shows
  Open/Close/Bookmark/Back; each card has an overlaid bottom title footer with
  ellipsis.
- **One cue per action.** Opening/closing the tabs view, moving focus,
  bookmarking, closing a tab, and selecting a card each emit exactly one
  `play_ui_cue` (sound and haptics). The plus card/new-tab path and bookmark
  navigation already cue from the tab/navigation call, so their grid close is
  silent (`close_browser_grid_with(..., false)`); selecting a card cues when
  the FLIP starts and the late tab activation stays silent.
- **No engine resize.** The FLIP overlay draws the tab's existing `FrameSlot`
  texture at an animating rectangle; card aspect equals the live page viewport
  aspect. Entering shrinks the page into the selected card; selecting a card
  expands that tab's last frame to the full page. The overlay and card shell
  are sampled in `draw` (not when the view is built), so the transition gets a
  new pose for every presented frame without rebuilding the tree. The
  `GridTicker` requests display-rate redraws only; it does not publish layout
  messages (the pending initial scroll uses one one-shot frame message).
- The Browser header scales with the card during the transition. It is drawn
  as a transformed layer instead of being clipped: iced's nested `with_layer`
  calls replace (not intersect) the parent clip, so a plain clip would leak
  header glyphs outside the shrinking card.
- Never render the same tab id twice in one frame (`BrowserFramePipeline`
  stores one uniform block per tab id): the animating tab's grid card omits its
  shader until the transition lands.
- Background cards show their last painted frame. Restored tabs stay dormant
  (`StoredTab.loaded == false`) until first activation; their cards show a
  title/URL placeholder.
- Bookmarks are `bookmarks.json` entries matched by exact URL. The top-right
  star is filled gold (`theme::GOLD`) when the tab's current URL is
  bookmarked, outline (`theme::MUTED`) otherwise. Triangle on a focused
  bookmark removes it. Opening a bookmark creates a new active tab for its URL
  (it never replaces the current page).
- Reduced motion snaps between page and grid; the normal path uses the
  `GridTicker` wrapper to request display-rate redraws without a global timer.

## Tab persistence

`tabs.json` stores URL/title plus the active index. Up to 25 saved tabs are
supported (`MAX_SAVED_TABS`); the row scrolls as long as needed.
`restore_saved_strip` restores every saved tab: the active page uses
`TabId::BOOT` (the engine preloads its URL), others become dormant stored pages.
Tab open/close/switch rewrites the file; `tabs_restored` still gates the
boot-time engine events that would otherwise clobber the saved strip before
restore. Circle->Home runs `persist_grid_selection`, which activates the focused
card first, so the next Browser open focuses the selection the user left behind.

## QA

```bash
python scripts/qa_browser_grid.py
python scripts/qa_browser_grid_scroll.py
```

Synthetic replay only (no OS input): local pages, `grid.open`, screenshot the
compact card/row geometry, `grid.bookmark`, `grid.new_tab`, `grid.activate`, and
page/grid screenshots. `ReplayWait::needs_grid_idle` keeps step boundaries
after the FLIP lands. The row script seeds more tabs than fit, checks the strip
opens scrolled to the active last tab, walks focus back to the plus card, and
asserts Circle->Home wrote the focused tab as the active `tabs.json` entry.
