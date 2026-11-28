# Work order: browser security — permissions, navigation guard, ad filtering

**Status:** Phase 1 (SEC-1/2/3) and Phase 2 (SEC-4) landed on `browser/main`
after baseline `91c7d49`; Phase 3 remains deferred. See `THIRD_PARTY_NOTICES.md`
and `assets/adblock/README.md` for bundled filter-list provenance.
**Baseline:** `91c7d49` (`browser/main`).
**Scope:** Phase 1 (permission denial + cross-site navigation guard with
remembered decisions) and Phase 2 (network ad/tracker filtering). Phase 3
(cosmetic filtering, list updater, saved-prefs UI) is deferred.
**Related:** [browser-embedding.md](browser-embedding.md),
[cef-api.md](cef-api.md), [browser-tab-grid.md](browser-tab-grid.md),
[shell-feedback-transition-work-order.md](shell-feedback-transition-work-order.md).

## 1. Incident and why the popup blocker missed it

On 2026-11-28 at 10:48:49 NZDT the Browser was on
`https://wealthyexile.com/stash` and navigated, with no app-level UI
navigation action, through:

1. `https://webtransit.live/visit.php?...pub=wealthyexile.com...`
2. HTTP redirect to
   `https://giftdash.rest/bonus/all-cc/lp1/index.php?...ref_domain=wealthyexile.com...`
   (title "Customer Reward Program").

CEF History recorded the first hop as `LINK + CHAIN_START` from
`wealthyexile.com/stash`, then `LINK + SERVER_REDIRECT + CHAIN_END`. The URL
carried `et2=160x600` (ad slot), `et1=displaymanager`, `t14=eval`, a bid/click
id shared with `webtransit.live`, and geography/ISP/browser fingerprinting.
The same chain occurred three times that day with different exchanges
(`ussi-admida…`, `usfo-admida…`, `usni-voku…`).

This was a **same-tab, top-level ad redirect**. `on_before_popup` cannot see
it. `giftdash.rest` also loaded `/push/push.js`, so the page attempted web-push
registration as well as whatever the reward form does.

### Prior art (do not re-investigate)

- **uBlock Origin:** `webRequest.onBeforeRequest`; layered dynamic URL rules →
  dynamic host firewall → static lists; tokenized matching (hostname/pattern
  tries, not per-rule regex); cosmetic filters (specific early, generic after
  DOM), scriptlets, redirects, CSP.
- **Adblock Plus / AdBlock:** EasyList subscriptions with metadata
  (`Title`/`Version`/`Expires`/`Checksum`); network filters plus element hiding.
- **Brave:** native `adblock-rust` engine; EasyList/EasyPrivacy/uBO/Brave lists
  delivered by its component updater; network + cosmetic + procedural
  filtering + scriptlets; token-clustered matching.
- **MV3 / uBlock Origin Lite:** precompiled `declarativeNetRequest` JSON
  rulesets; browser enforces quotas (~30k static/dynamic, 5k session); no
  per-request JS. Eyeo’s EWE converts ABP lists to DNR rulesets.
- **Consequence:** use `adblock-rust` for the engine rather than hand-rolling
  a matcher. Network filtering first; cosmetic filtering and list updates are
  Phase 3. Do not introduce a DNS proxy, IPC process, or background webview.

## 2. Locked rules

1. **Permissions are deny-by-default.** Notifications, geolocation, media
   capture, clipboard, MIDI, and any other prompt are denied without UI. There
   is no per-site permission grant in v1.
2. **Cross-site top-level navigation without a user gesture prompts.** The
   interstitial shows the full destination and source. Default action is
   **Stay**; the user must deliberately choose **Continue**.
3. **Same-site and user-gesture navigations proceed** unless the destination is
   on the hard denylist or matches a remembered block. Existing OAuth/SSO
   redirect hosts are allowlisted and must not prompt.
4. **The policy is pure and testable.** CEF callbacks are thin adapters over a
   `NavigationPolicy`/`BrowserSecurity` core; no UI or CEF types in the core.
5. **One shared engine, no per-request UI.** The ad-filter engine is loaded
   once when the CEF backend starts and shared by every tab.
6. **No telemetry.** Filter lists are fetched only from their published HTTPS
   URLs; no browsing data leaves the machine. Blocked/denied events go to the
   local trace lane, sampled.
7. **Blocked destinations are not restored.** A blocked or remembered-block
   URL must not come back from `tabs.json` after a restart.
8. **No new global timers/subscriptions.** Reuse the existing shell/browser
   frame demand; no 60 Hz watcher for security state.
9. **CEF 156 API names.** The permission hook is split:
   `on_show_permission_prompt` (notifications/geolocation/etc.) and
   `on_request_media_access_permission`; older docs call it
   `on_request_permission`. Use the 156 hooks.

## 3. Architecture

### 3.1 Shared security state

Create `src/browser/security/` (compiled only with `cef-engine`):

- `mod.rs` — `BrowserSecurity` (one per engine process): policy, decision
  store handle, denylist, optional `Arc<adblock::Engine>`.
- `navigation.rs` — pure `NavigationDecision`, registrable-domain matching,
  allowlist, reason enum; unit-tested without CEF.
- `decisions.rs` — in-memory decision map + persist/load bridge.
- `denylist.rs` — small hard-coded emergency list (`webtransit.live`,
  `giftdash.rest`, known ad-redirect hosts) checked before the filter engine.
- `adblock.rs` — engine construction, request mapping, list loading.

`TabContext` (`src/browser/cef_backend.rs:356`) gains
`security: Arc<BrowserSecurity>`. `SdscClient`
(`src/browser/cef_backend.rs:1473`) gains `request_handler` and
`permission_handler` fields plus getters, constructed in `SdscClient::new`
(`src/browser/cef_backend.rs:1767`).

### 3.2 Navigation flow

1. `on_before_browse` runs on the CEF browser-process UI thread.
2. Main frame only (`frame.is_main()`); subframe navigations are left to the
   resource filter.
3. Parse the current committed URL (source) and target URL.
4. `Security::classify(source, target, user_gesture, is_redirect)` returns
   `Allow`, `Prompt { reason }`, or `Block { reason }`.
5. `Allow` continues. `Block` cancels (`1` from `on_before_browse`) and emits
   `EngineEvent::NavigationBlocked` so the UI can show the block card.
6. `Prompt` cancels and emits `EngineEvent::NavigationBlocked` with
   `can_continue: true`; the current page stays visible behind the card.
7. **Continue** sends `EngineCommand::AllowBlockedNavigation { tab, url,
   token }`; the policy records the one-shot token, then the UI navigates again.
   `on_before_browse` sees and consumes the token, allowing exactly that URL.
8. **Remember** is a separate boolean in the event/UI. On Stay it stores a
   block; on Continue it stores an allow. Storage failure logs and does not
   block the current action.

### 3.3 Data model

New `src/persist/browser_prefs.rs`, stored in
`data_dir()/browser_prefs.json` (`src/persist/paths.rs:10`; use
`persist::store::{load_or_default,save_pretty}` at `src/persist/store.rs:21,48`
for atomic writes):

```json
{
  "version": 1,
  "navigation_decisions": [
    {
      "destination": "giftdash.rest",
      "action": "block",
      "created_unix": 1795816129
    }
  ]
}
```

- Match on the destination's registrable domain (eTLD+1), so subdomains and
  changing query strings hit the same decision. Use `addr`/`psl` (add the
  smallest maintained crate) or a documented suffix helper; no ad-hoc
  `ends_with` on arbitrary hosts.
- Cap entries (e.g. 256); drop the oldest.
- `block` is honored even with a user gesture. `allow` only suppresses the
  ungestured-cross-site prompt for that destination; the hard denylist still
  wins. This prevents a remembered allow from bypassing a known-bad entry.
- Unknown/corrupt file → defaults, never fail browser start.

## 4. Phase 1 (required)

### SEC-1 — Deny permissions

- Files: `src/browser/cef_backend.rs` (new `SdscPermissionHandler` beside
  `SdscLifeSpanHandler` at 925; wire into `SdscClient` at 1473/1767).
- Implement `on_show_permission_prompt(...)` and call
  `callback.cont(PermissionRequestResult::DENY)`. Keep `prompt_id` only for a
  trace line.
- Implement `on_request_media_access_permission(...)` and call
  `callback.cancel()`.
- Emit `browser.permission.denied` on `Lane::Browser` with
  `origin`, `permissions` (bitmask), and no user content.
- Update the `Permissions` row in `notes/cef-api.md:86` (it still names the
  older `OnRequestPermission` hook) and mark the split 156 hooks wired.
- Do not add a permission UI. The callback is synchronous; no pending state.
- Test: a local page requesting `Notification.requestPermission()` must not
  show a prompt and must log `browser.permission.denied`.

### SEC-2 — Navigation policy and interstitial

- Files: `src/browser/security/navigation.rs`, `src/browser/engine.rs`
  (`EngineCommand` 64, `EngineEvent` 297), `src/app/launcher.rs`
  (`on_engine_event` 1426), `src/app/browser.rs` (`BrowserAppState` 143,
  `on_browser_message` 2566), `src/ui/browser/state.rs` (`BrowserPanel` 125),
  `src/ui/browser/view.rs` (`BrowserMessage` 25, `browser_view`/root near 491).
- Add `EngineEvent::NavigationBlocked { tab, source, target, reason,
  can_continue, flow }` and `EngineCommand::AllowBlockedNavigation { tab, url,
  token }`.
- Add `BrowserPanel.nav_block: Option<BlockedNavigation>`; it owns
  `target`, `source`, `reason`, `stay_focused` (default true), `remember`.
  A blocked navigation is per-tab: handle `NavigationBlocked` before the
  active/background tab split (or store it with the tab) so a background
  tab's redirect never paints a card over the active page.
- Render a Browser-card overlay/page state (same stage/card language; no new
  window). Show origin → destination, a short reason, Stay / Continue, and a
  "Remember my decision" toggle. Keep the page/tabs header state sane; while
  blocked, page input is inert.
- Controller mapping: Left/Right chooses Stay/Continue, Cross confirms,
  Triangle toggles Remember, Circle = Stay. Default focus Stay. Mouse can
  click all three. No auto-continue on timeout.
- On **Continue**, issue the one-shot token and navigate. On **Stay**, clear
  the card and keep the current page. Both emit trace
  `browser.nav.choice` with `choice=stay|continue` and `remember=bool`.
- The card must work in quiet/automated sessions (synthetic messages) and
  never create a second shell window.
- Do not restore blocked tabs: filter on `tabs.json` load and on the saved
  strip restore (`src/app/launcher.rs` restore path), dropping hard-denied
  destinations with a log.

### SEC-3 — Emergency denylist

- Add `security/denylist.rs` with a small static list. Include
  `webtransit.live` and `giftdash.rest`; match registrable domains and
  subdomains.
- Hard-denied top-level navigations emit the same block card with
  `can_continue: true` only when the user has not remembered a block; never
  auto-load. Subresource requests to these hosts are cancelled in Phase 2.
- This is intentionally tiny and independent of the filter-list update
  mechanism.

## 5. Phase 2 (required): network ad/tracker filtering

### SEC-4 — `adblock-rust` engine and CEF resource filter

- Add `adblock` (Brave `adblock-rust`, MPL-2.0) as an optional dependency
  behind `cef-engine` in `Cargo.toml`; do not pull it into UI-only builds.
  Verify the selected version works with the pinned toolchain and decide the
  `single-thread` feature from the actual CEF callback threads (resource
  handler may run off the UI thread; if so disable `single-thread` or guard the
  engine with a `Mutex`). Load lists once when the engine worker starts, off
  the UI thread, and share `Arc<BrowserSecurity>`.
- Add `SdscRequestHandler` with `get_resource_request_handler`. Return
  `None` for a main-frame navigation (`is_navigation != 0` and the frame is
  main) so SEC-2 owns it; otherwise attach `SdscResourceRequestHandler`.
  In `on_before_resource_load`:
  - skip non-http(s) schemes;
  - build `adblock::request::Request` from URL, frame/source URL, and
    `CefRequest::resource_type()`; call `check_network_request`;
  - `should_block()` → `ReturnValue::CANCEL`; trace sampled
    `browser.resource.blocked` (URL host/type/rule source, no query values);
  - otherwise `ReturnValue::CONTINUE`.
- Keep `on_before_browse` for top-level policy; do not let a generic
  `$document` filter decide screen navigation without the gesture context.
- Bundle a pinned snapshot of filter lists under `assets/adblock/` (EasyList,
  EasyPrivacy, uBO filters, URLhaus/phishing) with a README recording source
  URL, version/date, SHA-256, and licenses. Do **not** fetch lists during
  `cargo build`/CI. If list size is a concern, ship a compact generated
  domain list first and add the full engine lists in Phase 3; choose one and
  document it.
- Licensing: EasyList/EasyPrivacy are GPLv3 or CC BY-SA 3.0 (attribution),
  uBO lists are GPLv3, URLhaus/Phishing lists are CC0, Peter Lowe's is
  non-commercial. `adblock` is MPL-2.0. Add `THIRD_PARTY_NOTICES` and do not
  bundle non-commercial lists in a paid build without review.
- No cosmetic filtering in Phase 2. No list updater in Phase 2; the checked-in
  snapshot is the source of truth.

## 6. Phase 3 (deferred, do not implement now)

- Cosmetic filtering: specific CSS before DOM, generic class/id hiding after
  load, procedural operators, scriptlets, DOM collapse for blocked resources.
- HTTPS list updater with version/checksum, differential updates, and a
  “last updated” UI.
- Saved-prefs management UI (view/remove remembered navigation decisions),
  per-site filter disable, and clear-site-data for blocked origins.
- Safe-browsing/reputation feed.

## 7. Acceptance

- A page calling `Notification.requestPermission()` shows no prompt; the
  trace records a denial.
- A cross-site ungestured top-level navigation is cancelled and the
  interstitial shows the exact destination; Stay leaves the current page;
  Continue loads only after confirmation.
- Remembering Stay makes the next matching navigation block silently;
  remembering Continue lets the next matching navigation load. Both survive a
  restart.
- `webtransit.live`/`giftdash.rest` cannot load from the page or as a
  top-level navigation without an explicit Continue; blocked tabs are not
  restored.
- Network filtering cancels ad requests without blocking the main document,
  same-site resources, or allowlisted auth redirects. Page load and input
  latency are not visibly regressed.
- Existing Browser/tabs/cursor/OSK/popups/history/replay behavior is
  unchanged. No new window or global timer.
- CI passes, including release clippy and the UI-only MSRV check.

## 8. Verification

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
cargo build
bash scripts/ci.sh
python scripts/qa_browser_grid.py
python scripts/qa_browser_flows.py
python scripts/qa_browser_actions.py
# new security QA: permissions denied + synthetic cross-site redirect card
```

Unit tests must cover: same-site allow, user-gesture allow, ungestured
cross-site prompt, denylist block, remembered block/allow, eTLD+1 matching,
OAuth allowlist, non-http URL, decision-store round-trip/cap, and filter-engine
matching for the incident hosts. Use trace assertions for the CEF adapters;
do not make the unit tests construct an `App`.

## 9. Risks / do-not-regress

- **OAuth/SSO and link shorteners** are the main false positives. Keep the
  allowlist small and tested; Continue must always be available for new
  destinations. A short post-click grace is acceptable, but never turn the
  guard into a blanket cross-site block.
- **CEF threads:** verify where `get_resource_request_handler` and
  `on_before_resource_load` run before choosing the `adblock` thread mode;
  don't block the UI thread on list parsing or per-request regexes.
- **Frame/source correctness:** use the initiating frame URL, not the tab's
  last address, for first/third-party decisions.
- **Popups:** `on_before_popup` stays as-is; do not route popup URLs through
  the top-level guard or the tabs model.
- **Replay/QA:** quiet sessions must still exercise the same policy; no
  real OS input, no on-screen windows. The blocked card must be testable
  through synthetic messages.
- **Persistence:** atomic writes, corrupt-file defaults, capped entries;
  never fail browser start because `browser_prefs.json` is unreadable.
- **Licensing:** do not add a filter list without recording its license and
  attribution; MPL-2.0 `adblock` file-level obligations apply to modified
  crate files only, but get a second look before shipping.
- **Do not** add a second popup/window, a background webview, a DNS proxy, or
  any IPC process for filtering. The filter lives in the existing engine
  process.

## 10. References

- uBO network filtering engine: <https://github.com/uBlockOrigin/uBlock-issues/wiki/Overview-of-uBlock%27s-network-filtering-engine:-details>
- uBO static syntax: <https://github.com/gorhill/uBlock/wiki/Static-filter-syntax>
- EasyList / EasyPrivacy: <https://easylist.to/>; licenses:
  <https://easylist.to/pages/licence.html>
- Brave `adblock-rust`: <https://github.com/brave/adblock-rust> (MPL-2.0)
- Chrome `declarativeNetRequest` (MV3 comparison):
  <https://developer.chrome.com/docs/extensions/reference/api/declarativeNetRequest>
- CEF API map and thread notes: [cef-api.md](cef-api.md) (`Permissions` row,
  `OnBeforeBrowse`/resource-handler notes to be updated by SEC-1/SEC-4).
