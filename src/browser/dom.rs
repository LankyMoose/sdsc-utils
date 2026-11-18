//! DOM target source for snap targets (`servo-engine` only).
//!
//! Servo's accessibility tree withholds real controls (form elements,
//! `div[role=button]`, virtualized items) and freezes rects, so the live
//! DOM is the sole target source instead. This module queries the page
//! directly for actionable elements and their boxes:
//! - the page is the ground truth for *existence* (re-renders funnel into
//!   the DOM, no removal-record bookkeeping needed),
//! - [`getBoundingClientRect`] forces real boxes every poll.
//!
//! The backend polls this on load settle, scroll settle, input, and a slow
//! backstop. Polling, not pushing: the DOM cannot notify us, so we re-query
//! on every event that typically mutates it.

use crate::browser::cursor::FocusKind;

/// Viewport-space snap candidate straight from the DOM.
#[derive(Debug, Clone, PartialEq)]
pub struct DomRaw {
    pub id: u64,
    pub tag: String,
    pub role: String,
    pub typ: String,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub text: String,
    /// Resolved link target (`href`, absolute, capped; empty when none).
    pub url: String,
    /// Carries the `autofocus` attribute (ground truth for traces).
    pub autofocus: bool,
}

/// Marker shared by the dirty-ping channels (B=fetch, C=sendBeacon).
/// Rust matches resource-load URLs by substring, so the ping URL below
/// embeds it verbatim.
pub const DOM_DIRTY_MARKER: &str = "__SDSC_DOM_DIRTY";

/// Ping endpoint for the network channels (B=fetch, C=sendBeacon), with the
/// channel encoded as `?ch=`. Plain `http` (not a custom scheme) so the load
/// reaches `load_web_resource`, where the backend intercepts it with an
/// empty 200 before any traffic leaves the machine. `.invalid` can never
/// resolve if one ever leaks past the intercept.
pub const DOM_PING_URL: &str = "http://sdsc.invalid/__SDSC_DOM_DIRTY";

/// Observer install script (idempotent per document). Attaches a
/// MutationObserver (subtree: childList, attributes, characterData) plus a
/// ResizeObserver on `body` (image/font loads shift layout with no DOM
/// mutation), with in-page leading+trailing debounce so animation-heavy
/// pages collapse 60fps mutation storms to ~2 pings/s. Every ping POSTs to
/// the marker URL; the backend intercepts it into one dirty flag.
pub const DOM_INSTALL_JS: &str = r###"(function(){
  // Top document only. UserContentManager auto-installs this in every
  // frame; subframe documents can be transient and Servo's script/layout
  // state is unstable during iframe load/teardown. Never attach our
  // observers or geometry reads there.
  try { if (window.top !== window.self) return; } catch (e) { return; }
  var S = window.__sdsc_ax;
  if (!S) { S = window.__sdsc_ax = { next: 1, ids: new WeakMap() }; }
  if (S.mo) return;
  S.mo = true;
  // Document identity: a real reload gets a fresh window and therefore a
  // new token; a same-document head re-parse (Servo can re-report load
  // status when the page injects head resources) keeps it. The backend
  // uses this to tell a spurious `Complete` from an actual document swap.
  if (!S.doc) { S.doc = Date.now().toString(36) + '.' + Math.random().toString(36).slice(2); }
  // Numeric id → element map, rebuilt by every snapshot query (so detached
  // nodes release instead of leaking). Powers `__sdsc_click`: exact-element
  // activation without hit-test staleness.
  if (!S.byId) { S.byId = {}; }
  window.__sdsc_click = window.__sdsc_click || function(id) {
    try {
      var el = S.byId[id];
      if (!el) return 'missing';
      el.click();
      return 'ok';
    } catch (e) { return 'threw:' + (e && e.message ? e.message : e); }
  };
  var PING = 'http://sdsc.invalid/__SDSC_DOM_DIRTY';
  var lastPing = 0, trailing = null, pendingEv = null;
  var SUPPRESS = 500, TRAIL = 250;
  function fire(src){
    lastPing = Date.now();
    // Window truth rides along: a nested-container scroll moves only that
    // container's children, so the global offset must not absorb it — the
    // backend corrects to these coordinates on arrival instead of
    // mis-translating every outline until the next settle pull.
    // `src` tells the backend what mutated: scroll-only pings carry no DOM
    // changes, so cached per-element verdicts stay valid across them.
    var wx = 0, wy = 0;
    try { wx = Math.round(window.scrollX || window.pageXOffset || 0); wy = Math.round(window.scrollY || window.pageYOffset || 0); } catch (e) {}
    try { fetch(PING + '?wx=' + wx + '&wy=' + wy + '&ev=' + src, {method:'POST', mode:'no-cors', keepalive:true, body:'1'}).catch(function(){}); } catch (e) {}
  }
  function onMut(ev) {
    ev = ev || 'mc';
    // Escalate: a trailing 'mc' must not swallow a later 'ma' — an
    // attribute change hiding elements would otherwise keep stale
    // visible verdicts until the next invalidating ping.
    if (ev === 'ma') pendingEv = 'ma';
    else if (!pendingEv) pendingEv = ev;
    var now = Date.now();
    if (now - lastPing >= SUPPRESS && !trailing) { var e = pendingEv; pendingEv = null; fire(e); }
    else if (!trailing) {
      trailing = setTimeout(function(){ trailing = null; var e2 = pendingEv; pendingEv = null; fire(e2); }, TRAIL);
    }
  }
  function arm(){
    var root = document.documentElement;
    if (!root) { setTimeout(arm, 500); return; }
    try {
      // Classify records: only attribute mutations that can flip a
      // visibility verdict invalidate the cache (backend tag 'ma'):
      // `class`/`style`/`hidden` drive the selectors and inline styles the
      // verdict walk reads. Any other attribute (data-*, aria-*, jsaction
      // churn Google fires constantly) can't change display/visibility/
      // opacity in practice, so it re-queries for fresh boxes ('mc') without
      // invalidating survivors' verdicts. Insert/remove/text ('mc') can't
      // affect surviving nodes either — new nodes miss the cache and
      // compute fresh.
      new MutationObserver(function(recs){
        var attr = false;
        try { for (var i = 0; i < recs.length; i++) {
          if (recs[i].type !== 'attributes') continue;
          var an = recs[i].attributeName;
          if (!an || an === 'class' || an === 'style' || an === 'hidden') { attr = true; break; }
        } } catch (e) {}
        onMut(attr ? 'ma' : 'mc');
      }).observe(root, {subtree:true, childList:true, attributes:true, characterData:true});
    } catch (e) {}
    try {
      // Resize shifts layout but never flips display/visibility/opacity:
      // structural tag, verdicts kept.
      new ResizeObserver(function(){ onMut('mc'); }).observe(document.body || root);
    } catch (e) {}
  }
  arm();
  // Stylesheets applying at load can flip whole-subtree verdicts: treat
  // as attribute-class invalidation.
  try { window.addEventListener('load', function(){ onMut('ma'); }); } catch (e) {}
  // Scrolls (page or nested containers) change viewport rects with no DOM
  // mutation, so the observers alone never hear them. Capture phase: scroll
  // events don't bubble, this catches every scroller in the document.
  // Tagged 'sc' so the backend knows nothing mutated.
  try { document.addEventListener('scroll', function(){ onMut('sc'); }, {capture:true, passive:true}); } catch (e) {}
})()"###;

/// Boot script: idempotent install + immediate snapshot, so one round-trip
/// both arms the observers and returns the current targets. The install is
/// explicitly semicolon-terminated: without it the two adjacent IIFEs parse
/// as a call on the install's (undefined) return value (ASI hazard).
/// `center` is the cursor point the snapshot culls around (see
/// [`dom_query_js`]); `None` keeps everything (no cursor yet).
pub fn dom_boot_js(center: Option<(f32, f32)>, epoch: u64) -> String {
    format!("{DOM_INSTALL_JS};\n{}", dom_query_js(center, epoch))
}

/// One-shot event-loop probe: arm markers for the three async task
/// sources, read them back after a beat. If timers fire but rAF never
/// does, the page's frame-gated JS (hydration, lazy handlers) is starved —
/// exactly the "clicks do nothing" signature.
pub const PROBE_ARM_JS: &str = r###"(function(){
  try {
    window.__sdsc_loop = {timer: 0, raf: 0, io: 0};
    setTimeout(function(){ window.__sdsc_loop.timer = 1; }, 100);
    requestAnimationFrame(function(){ window.__sdsc_loop.raf = 1; });
    var t = document.body || document.documentElement;
    if (t) { new IntersectionObserver(function(){ window.__sdsc_loop.io = 1; }).observe(t); }
  } catch (e) {}
  return 'armed';
})()"###;

/// Read back [`PROBE_ARM_JS`] markers: `[timer, raf, io]` as 1/0, or
/// `[-1,-1,-1]` when the arm never ran.
pub const PROBE_READ_JS: &str = r###"(function(){var l=window.__sdsc_loop;if(!l)return[-1,-1,-1];return[l.timer?1:0,l.raf?1:0,l.io?1:0]})()"###;

/// Debug-only storage spike census (see `maybe_probe_storage`): reports
/// localStorage/sessionStorage sizes, a probe-key write/read/delete
/// round-trip, and the cookie count. Key names truncated to 40 chars for
/// the log; VALUES NEVER LEAVE THE PAGE (only lengths and the probe key's
/// own marker round-trip).
pub const STORAGE_PROBE_JS: &str = r###"(function(){
  var r = {lsLen: -1, lsKeys: [], roundTrip: 'skip', ssLen: -1, cookies: -1};
  try { r.lsLen = window.localStorage.length; } catch (e) { r.lsLen = -2; }
  try {
    var ks = [];
    for (var i = 0; i < window.localStorage.length && i < 10; i++) {
      var k = window.localStorage.key(i);
      ks.push(k === null ? 'null' : String(k).slice(0, 40));
    }
    r.lsKeys = ks;
  } catch (e) {}
  try {
    window.localStorage.setItem('__sdsc_probe', '1');
    var v = window.localStorage.getItem('__sdsc_probe');
    window.localStorage.removeItem('__sdsc_probe');
    r.roundTrip = (v === '1') ? 'ok' : ('mismatch:' + v);
  } catch (e) { r.roundTrip = 'threw:' + (e && e.name ? e.name : e); }
  try { r.ssLen = window.sessionStorage.length; } catch (e2) { r.ssLen = -2; }
  try { var c = document.cookie || ''; r.cookies = c ? c.split(';').length : 0; } catch (e3) {}
  return JSON.stringify(r);
})()"###;

/// Parse a [`PROBE_READ_JS`] result. `None` on malformed shapes or when the
/// arm never ran (page navigated away mid-probe).
pub fn parse_loop_probe(value: &embedder_traits::JSValue) -> Option<[bool; 3]> {
    use embedder_traits::JSValue;
    let JSValue::Array(cols) = value else {
        return None;
    };
    if cols.len() != 3 {
        return None;
    }
    let mut out = [false; 3];
    for (i, col) in cols.iter().enumerate() {
        match col {
            JSValue::Number(n) if *n == 1.0 => out[i] = true,
            JSValue::Number(n) if *n == 0.0 => {}
            _ => return None,
        }
    }
    Some(out)
}

/// Read-only DOM query. Stable per-document ids via an in-page WeakMap
/// (recreated on every navigation, so ids are only meaningful within one
/// document). Bounded to 1000 matches; reports truncation.
/// `center` is the pad cursor in viewport px (or `None`): candidates beyond
/// 500px of it are culled — except the nearest target in each direction, so
/// arrow-key jumps always have somewhere to go. `None` keeps everything.
/// `epoch` bumps whenever the DOM (or anything affecting visibility) may
/// have changed; the in-page visibility verdict cache resets on mismatch,
/// so pure-scroll pulls reuse verdicts while mutations re-verify.
pub fn dom_query_js(center: Option<(f32, f32)>, epoch: u64) -> String {
    let (cx, cy, on) = match center {
        Some((x, y)) => (format!("{x:.0}"), format!("{y:.0}"), "true"),
        None => ("0".to_string(), "0".to_string(), "false"),
    };
    format!("var __sdsc_cx={cx},__sdsc_cy={cy},__sdsc_cc={on},__sdsc_v={epoch};\n{DOM_QUERY_BODY}")
}

const DOM_QUERY_BODY: &str = r###"(function(){
  // Top document only (see DOM_INSTALL_JS): no snap targets are collected
  // from transient iframe documents.
  try { if (window.top !== window.self) return {}; } catch (e) { return {}; }
  var S = window.__sdsc_ax;
  if (!S) { S = window.__sdsc_ax = { next: 1, ids: new WeakMap() }; }
  function idFor(el) {
    var id = S.ids.get(el);
    if (id === undefined) { id = S.next++; S.ids.set(el, id); }
    S.byId[id] = el;
    return id;
  }
  var SEL = 'a,button,input,textarea,select,[onclick],[role="button"],[role="link"],[role="checkbox"],[role="textbox"],[role="searchbox"],[role="switch"],[role="combobox"],[role="menuitem"],[role="tab"],[role="option"]';
  // Computed-style cache for the ancestor walks below: one entry per
  // distinct ancestor, rebuilt every pull (styles mutate between pulls).
  var styleCache = (typeof Map !== "undefined") ? new Map() : null;
  // Verdict cache: an element's hidden/visible verdict only changes when
  // the DOM (or its ancestors' styles) mutates, which always fires a
  // mutation ping. The backend bumps __sdsc_v on those and leaves it alone
  // for pure scrolls — so scroll-settle pulls reuse verdicts instead of
  // re-walking, and mutation pulls re-verify.
  if (S.visEpoch !== __sdsc_v) { S.vis = (typeof WeakMap !== "undefined") ? new WeakMap() : null; S.visEpoch = __sdsc_v; }
  function cstyle(n) {
    if (styleCache) {
      var hit = styleCache.get(n);
      if (hit) return hit;
    }
    var s = {d: '', v: '', o: '', ox: '', oy: ''};
    try {
      var cs = window.getComputedStyle(n);
      s = {d: cs.display, v: cs.visibility, o: cs.opacity, ox: cs.overflowX, oy: cs.overflowY};
    } catch (e) {}
    if (styleCache) styleCache.set(n, s);
    return s;
  }
  // True visibility: computed visibility is already inheritance-resolved,
  // so one call covers the whole ancestor chain for it; display:none and
  // opacity:0 need the ancestor walk (not inherited, still hiding).
  // Verdicts are cached per element for the epoch (see above): recompute
  // only on the first sighting per DOM epoch.
  function isHidden(el) {
    var cached;
    try { cached = S.vis ? S.vis.get(el) : undefined; } catch (e) {}
    if (cached !== undefined) return cached;
    var hidden = isHiddenUncached(el);
    try { if (S.vis) S.vis.set(el, hidden); } catch (e2) {}
    return hidden;
  }
  function isHiddenUncached(el) {
    try {
      var cs = window.getComputedStyle(el);
      if (cs.display === 'none' || cs.visibility === 'hidden' || cs.visibility === 'collapse') return true;
      if (parseFloat(cs.opacity) === 0) return true;
    } catch (e) { return false; }
    var n = el.parentElement, depth = 0;
    while (n && depth < 32) {
      var s = cstyle(n);
      if (s.d === 'none' || parseFloat(s.o) === 0) return true;
      n = n.parentElement;
      depth++;
    }
    return false;
  }
  // Intersect a box with every clipping ancestor (overflow != visible).
  // Returns the visible part, or null when clipped away entirely.
  // display:contents ancestors have no box and cannot clip: skipped.
  function clipRect(el, r) {
    var x0 = r.x, y0 = r.y, x1 = r.x + r.width, y1 = r.y + r.height;
    var n = el.parentElement, depth = 0;
    while (n && depth < 32) {
      var s = cstyle(n);
      if (parseFloat(s.o) === 0) return null;
      var clipX = (s.ox === 'hidden' || s.ox === 'scroll' || s.ox === 'auto' || s.ox === 'clip');
      var clipY = (s.oy === 'hidden' || s.oy === 'scroll' || s.oy === 'auto' || s.oy === 'clip');
      if ((clipX || clipY) && s.d !== 'contents') {
        var br = null;
        try {
          var rs = n.getClientRects();
          if (rs.length) {
            var bx0 = Infinity, by0 = Infinity, bx1 = -Infinity, by1 = -Infinity;
            for (var j = 0; j < rs.length; j++) {
              var q = rs[j];
              var qx0 = Math.min(q.x, q.x + q.width), qx1 = Math.max(q.x, q.x + q.width);
              var qy0 = Math.min(q.y, q.y + q.height), qy1 = Math.max(q.y, q.y + q.height);
              if (qx0 < bx0) bx0 = qx0;
              if (qy0 < by0) by0 = qy0;
              if (qx1 > bx1) bx1 = qx1;
              if (qy1 > by1) by1 = qy1;
            }
            if (bx1 > bx0 && by1 > by0) br = {x: bx0, y: by0, width: bx1 - bx0, height: by1 - by0};
          }
        } catch (e) {}
        if (br) {
          if (clipX) { if (br.x > x0) x0 = br.x; if (br.x + br.width < x1) x1 = br.x + br.width; }
          if (clipY) { if (br.y > y0) y0 = br.y; if (br.y + br.height < y1) y1 = br.y + br.height; }
          if (x1 <= x0 || y1 <= y0) return null;
        }
      }
      n = n.parentElement;
      depth++;
    }
    if (!(x1 > x0 && y1 > y0)) return null;
    return {x: x0, y: y0, width: x1 - x0, height: y1 - y0};
  }
  var out = [];
  var culled = 0;
  // Skip census: WHY matched candidates never become targets. `total` says
  // how many matched; these say where the rest went. Read the ingest log.
  // skBox = no boxes; skBoxA = of those, anchors; skBoxKid = of those, with
  // element children (the wrapper signature: boxless parent, boxed kids).
  var skBox = 0, skZero = 0, skBig = 0, skDis = 0, skErr = 0;
  var skBoxA = 0, skBoxKid = 0, skBoxAWrap = 0, skBoxAEmpty = 0, skForce = 0;
  var skHid = 0, skClip = 0;
  // Phase timings (ms, Date.now resolution): find the actual per-pull cost
  // center instead of guessing. tq = selector, tm = boxes, tv = visibility
  // walks, tc = clip walks, tt = text extraction.
  var tQ = 0, tmM = 0, tmV = 0, tmC = 0, tmT = 0;
  var __t0 = Date.now();
  S.byId = {};
  var els;
  try { els = document.querySelectorAll(SEL); } catch (e) { return {nodes: [], total: 0, truncated: false}; }
  var total = els.length;
  var n = Math.min(els.length, 1000);
  tQ = Date.now() - __t0;
  // NOTE: deliberately the plural fragment call, never the singular
  // bounding-box one. Servo's singular version carries a
  // `debug_assert!(w >= 0 && h >= 0)` (servo-script dom/element/element.rs)
  // that aborts the script thread on real pages (negative border boxes from
  // transformed content), taking the whole app down in debug builds.
  // GetClientRects shares the data source but has no assert; the union below
  // normalizes fragments and can never go negative.
  function boxOf(el) {
    var rs;
    try { rs = el.getClientRects(); } catch (e) { return null; }
    var x0 = Infinity, y0 = Infinity, x1 = -Infinity, y1 = -Infinity;
    for (var j = 0; j < rs.length; j++) {
      var q = rs[j];
      var qx0 = Math.min(q.x, q.x + q.width), qx1 = Math.max(q.x, q.x + q.width);
      var qy0 = Math.min(q.y, q.y + q.height), qy1 = Math.max(q.y, q.y + q.height);
      if (qx0 < x0) x0 = qx0;
      if (qy0 < y0) y0 = qy0;
      if (qx1 > x1) x1 = qx1;
      if (qy1 > y1) y1 = qy1;
    }
    if (!(x1 > x0 && y1 > y0)) return null;
    return {x: x0, y: y0, width: x1 - x0, height: y1 - y0};
  }
  // Pass 1: measure everything (id assignment is stable per document, so
  // culled elements keep their ids for the pull that re-admits them).
  // Deliberately NO visibility verdicts here: one getComputedStyle call
  // costs ~15ms in debug Servo while getClientRects unions are
  // microseconds, so verdicts run only on elements that survive geometric
  // culling (pass 2/3) plus boxless forcing candidates — never on all N
  // candidates up front.
  var cands = [];
  var boxless = [];
  var vw = 800, vh = 600;
  try { vw = window.innerWidth || 800; vh = window.innerHeight || 600; } catch (e0) {}
  for (var i = 0; i < n; i++) { (function(el){
    try {
      if (el.disabled) { skDis++; return; }
      var t = el.tagName ? el.tagName.toLowerCase() : "";
      var ty = "";
      if (t === "input") {
        ty = (el.getAttribute("type") || "text").toLowerCase();
        if (ty === "hidden") return;
      }
      var __a = Date.now();
      var r = boxOf(el);
      tmM += Date.now() - __a;
      // Boxless goes aside: hidden subtrees (display:none) are the common
      // case and get their own cheap triage + verdict below, never the
      // forcing path blind.
      if (!r) { boxless.push([el, t]); return; }
      if (!(r.width > 0 && r.height > 0)) { skZero++; return; }
      // Full-viewport-or-larger boxes are never snap targets: page roots and
      // app shells (often carrying global [onclick]) would otherwise become
      // screen-hogging phantoms. Only a box covering the whole viewport in
      // BOTH axes qualifies as a phantom — full-width rows (accordions, nav
      // bars) are real targets and must survive.
      if (r.width >= vw && r.height >= vh) { skBig++; return; }
      // Viewport cull (1-viewport margin): offscreen targets are dead weight
      // in transfer, merge, and magnet cost — and worse, document-order
      // truncation would keep them while dropping visible ones. Culled nodes
      // re-enter on the next scroll-settle pull; scroll clamping uses the
      // document dims below, never the live-set corners.
      if (r.x + r.width < -vw || r.x > vw + vw || r.y + r.height < -vh || r.y > vh + vh) {
        culled++;
        return;
      }
      cands.push([el, t, ty, r]);
    } catch (e) { skErr++; }
  })(els[i]); }
  // Pass 1b: boxless triage. display:none subtrees (menus, dialogs) match
  // the selector but must never reach forcing — and there can be hundreds.
  // Cheap gates first ([hidden] subtree membership, inline display:none:
  // property reads, no style resolution); the full verdict walk only for
  // the remainder. Survivors are forcing candidates (boxless-but-rendered,
  // e.g. wrapper anchors).
  var bi;
  for (bi = 0; bi < boxless.length; bi++) { (function(el, t){
    try {
      var gateHidden = false;
      try { gateHidden = !!(el.hidden || (el.style && el.style.display === 'none') || (el.closest && el.closest('[hidden]'))); } catch (e0) {}
      var __b = Date.now();
      var __h = gateHidden || isHidden(el);
      tmV += Date.now() - __b;
      if (__h) { skHid++; return; }
      // Boxless-but-rendered: candidates for forcing.
      skBox++;
      var hasKids = false;
      var hasAnyKid = false;
      try { hasKids = !!(el.children && el.children.length > 0); } catch (e3) {}
      try { hasAnyKid = !!el.firstChild; } catch (e4) {}
      try {
        if (t === "a") {
          skBoxA++;
          if (hasKids) skBoxAWrap++;
          else if (!hasAnyKid) skBoxAEmpty++;
          // Remainder (hasAnyKid && !hasKids) = text-only anchors.
        }
        if (hasKids) skBoxKid++;
      } catch (e2) {}
      // Persistent display forcing: the synchronous set-measure-restore
      // cycle recovered nothing, so keep the mutation. One shot per
      // element (repeat sets are no-ops, the guard keeps the census
      // honest). HTML only: forcing display on SVG-namespace elements
      // breaks their native layout (observed: dead SVG rendering).
      // The attribute mutation pings our own observer, which re-pulls:
      // elements that gain boxes appear as fresh targets, the rest stay
      // counted here.
      try {
        var fid = idFor(el);
        if (!S.forced) S.forced = {};
        if (!S.forced[fid]) {
          S.forced[fid] = 1;
          if (!el.namespaceURI || el.namespaceURI === 'http://www.w3.org/1999/xhtml') {
            el.style.setProperty('display', 'block', 'important');
            skForce++;
          }
        }
      } catch (e6) {}
    } catch (e) { skErr++; }
  })(boxless[bi][0], boxless[bi][1]); }
  // Pass 2: cursor cull (500px radius) + directional keep. The live set is
  // what the cursor can actually reach; the nearest target strictly in each
  // direction always survives so arrow-key jumps never strand. No cursor yet
  // (__sdsc_cc false) keeps everything. Verdicts, clip walks, and text
  // extraction all stay in the emit below so geometrically-culled rows pay
  // for none of them. Cursor-culled ids ride along in `ccull` so the
  // backend retains them with translated rects instead of churning
  // fresh/drop every pull (the set would never stabilize while scrolling).
  var R2 = 500 * 500;
  var best = {up: null, down: null, left: null, right: null};
  var ccull = [];
  var ci, cr, cdx, cdy, dirs, di, dd, score;
  if (__sdsc_cc) {
    for (ci = 0; ci < cands.length; ci++) {
      cr = cands[ci][3];
      cdx = (cr.x + cr.width / 2) - __sdsc_cx;
      cdy = (cr.y + cr.height / 2) - __sdsc_cy;
      if (cdx * cdx + cdy * cdy <= R2) { cands[ci].push(true); continue; }
      dirs = [];
      if (cdy < 0) dirs.push(['up', -cdy, Math.abs(cdx)]);
      if (cdy > 0) dirs.push(['down', cdy, Math.abs(cdx)]);
      if (cdx < 0) dirs.push(['left', -cdx, Math.abs(cdy)]);
      if (cdx > 0) dirs.push(['right', cdx, Math.abs(cdy)]);
      for (di = 0; di < dirs.length; di++) {
        dd = dirs[di];
        score = dd[1] + 2 * dd[2];
        if (!best[dd[0]] || score < best[dd[0]][1]) best[dd[0]] = [ci, score];
      }
      cands[ci].push(false);
    }
    var keepIdx = {};
    for (var bk in best) { if (best[bk]) keepIdx[best[bk][0]] = true; }
    for (ci = 0; ci < cands.length; ci++) {
      if (!cands[ci][4] && !keepIdx[ci]) {
        culled++;
        ccull.push(idFor(cands[ci][0]));
        continue;
      }
      emitCand(cands[ci]);
    }
  } else {
    for (ci = 0; ci < cands.length; ci++) emitCand(cands[ci]);
  }
  function emitCand(cc) {
    var el = cc[0];
    // Verdict LAST: only elements that survived every geometric cull pay
    // for getComputedStyle. Out-of-range rows keep no verdict either way —
    // they re-verify on the pull that re-admits them (or hit the warm
    // cache when the epoch held).
    var __c = Date.now();
    var __h = isHidden(el);
    tmV += Date.now() - __c;
    if (__h) { skHid++; return; }
    // Overflow clipping: an element inside a height:0 scroller (Wikipedia
    // menus) or any overflow:hidden ancestor has boxes but zero visible
    // area. Intersect up the ancestor chain; fully clipped rows never
    // become targets.
    __c = Date.now();
    var cr = clipRect(el, cc[3]);
    tmC += Date.now() - __c;
    if (!cr) { skClip++; return; }
    var role = el.getAttribute ? (el.getAttribute("role") || "").toLowerCase() : "";
    // textContent, not innerText: innerText forces a style-sensitive layout
    // pass per element (death by a thousand reflows in debug builds);
    // textContent is a straight tree walk and hidden subtrees never reach
    // here (the verdict above filtered them).
    var txt = "";
    var __a = Date.now();
    try { txt = (el.textContent || "").replace(/\s+/g, " ").slice(0, 40); } catch (e2) {}
    tmT += Date.now() - __a;
    var r = cr;
    // Hovered-link status needs the target: resolved href (absolute), capped
    // so pathological query strings never bloat the snapshot.
    var href = "";
    try { if (typeof el.href === "string") href = el.href.slice(0, 512); } catch (e3) {}
    // Autofocus ground truth for traces: which elements carry the attribute.
    var af = 0;
    try { if (el.hasAttribute && el.hasAttribute("autofocus")) af = 1; } catch (e4) {}
    out.push([idFor(el), cc[1], role, cc[2], Math.round(r.x), Math.round(r.y), Math.round(r.width), Math.round(r.height), txt, href, af]);
  }
  var sx = 0, sy = 0;
  try { sx = window.scrollX || window.pageXOffset || 0; sy = window.scrollY || window.pageYOffset || 0; } catch (e) {}
  var dw = 0, dh = 0;
  try {
    var de = document.documentElement;
    if (de) { dw = de.scrollWidth || 0; dh = de.scrollHeight || 0; }
  } catch (e) {}
  // Focused element in the snapshot's own id space (0 = none/body):
  // drives cursor docking on autofocus / JS .focus().
  var ax = 0;
  try { var ae = document.activeElement; if (ae && ae !== document.body && ae !== document.documentElement) { ax = idFor(ae); } } catch (e) {}
  return {nodes: out, total: total, truncated: els.length > n, culled: culled, ccull: ccull, skip: [skBox, skZero, skBig, skDis, skErr], hid: skHid, clip: skClip, boxA: skBoxA, boxKid: skBoxKid, boxAWrap: skBoxAWrap, boxAEmpty: skBoxAEmpty, forced: skForce, t: [Math.round(tQ), Math.round(tmM), Math.round(tmV), Math.round(tmC), Math.round(tmT)], sx: Math.round(sx), sy: Math.round(sy), dw: Math.round(dw), dh: Math.round(dh), ax: ax, dt: S.doc};
})()"###;

/// Parsed snapshot: viewport-space candidates plus the true page scroll at
/// query time. The backend stores document-space rects (viewport + scroll)
/// and rebases its tracked offset to this ground truth, so commanded-delta
/// drift and history-restored offsets self-heal on every pull. `doc` carries
/// the full document dims so scroll clamping survives viewport culling (the
/// live set only ever spans viewport+margin); `total`/`culled`/`skipped`
/// are diagnostics for cap pressure and skip reasons, in
/// [boxless, zero-area, oversize, disabled, threw] order. `box_anchors`
/// counts boxless <a> elements, `boxless_with_kids` counts boxless elements
/// with element children, `box_anchor_wraps` and `box_anchor_empty` split
/// the anchors into wrappers vs childless (text-only anchors are the
/// remainder: boxA - wraps - empty). `forced_boxes` counts targets
/// recovered by display forcing; `hidden_subtree` (display/visibility/
/// opacity) and `clipped_away` (overflow clip to zero) the correctly
/// invisible.
#[derive(Debug, Clone, PartialEq)]
pub struct DomSnapshot {
    pub nodes: Vec<DomRaw>,
    pub scroll: (f32, f32),
    pub doc: (f32, f32),
    pub total: usize,
    pub culled: usize,
    pub culled_ids: Vec<u64>,
    pub skipped: [usize; 5],
    pub box_anchors: usize,
    pub boxless_with_kids: usize,
    pub box_anchor_wraps: usize,
    pub box_anchor_empty: usize,
    pub forced_boxes: usize,
    pub hidden_subtree: usize,
    pub clipped_away: usize,
    /// In-page phase timings (ms): querySelectorAll, box measurement,
    /// visibility walks, clip walks, text extraction. Finds the real
    /// per-pull cost center instead of guessing.
    pub timings: [u64; 5],
    /// Focused element's snapshot id (`ax`, 0 = none): drives cursor
    /// docking on autofocus / JS `.focus()`.
    pub active: u64,
    /// Per-window document token from the install script. A real reload
    /// creates a fresh window (new token); a same-document head re-parse
    /// keeps it. `""` when the page never ran our install.
    pub doc_token: String,
}

/// Parse a `dom_query_js` result into raw candidates. Lenient by design:
/// malformed rows are skipped, never fatal; missing scroll/doc dims default
/// to origin/zero (old cached pages can't produce them; origin is the safe
/// assumption, never a drift source — the next pull rebases; zero doc dims
/// just leave the extent to the live set).
pub fn parse_dom_snapshot(value: &embedder_traits::JSValue) -> DomSnapshot {
    use embedder_traits::JSValue;
    let JSValue::Object(root) = value else {
        return DomSnapshot {
            nodes: Vec::new(),
            scroll: (0.0, 0.0),
            doc: (0.0, 0.0),
            total: 0,
            culled: 0,
            culled_ids: Vec::new(),
            skipped: [0; 5],
            box_anchors: 0,
            boxless_with_kids: 0,
            box_anchor_wraps: 0,
            box_anchor_empty: 0,
            forced_boxes: 0,
            hidden_subtree: 0,
            clipped_away: 0,
            timings: [0; 5],
            active: 0,
            doc_token: String::new(),
        };
    };
    let num_field = |key: &str| {
        root.get(key)
            .and_then(|v| match v {
                JSValue::Number(n) => Some(*n as f32),
                _ => None,
            })
            .unwrap_or(0.0)
    };
    let scroll = (num_field("sx"), num_field("sy"));
    let doc = (num_field("dw"), num_field("dh"));
    let active = num_field("ax").max(0.0) as u64;
    let doc_token = match root.get("dt") {
        Some(JSValue::String(s)) => s.clone(),
        _ => String::new(),
    };
    let total = num_field("total") as usize;
    let culled = num_field("culled") as usize;
    let mut culled_ids = Vec::new();
    if let Some(JSValue::Array(cols)) = root.get("ccull") {
        for col in cols.iter().take(3000) {
            if let JSValue::Number(n) = col {
                let id = *n as u64;
                if (*n).is_finite() && id as f64 == *n {
                    culled_ids.push(id);
                }
            }
        }
    }
    let box_anchors = num_field("boxA") as usize;
    let boxless_with_kids = num_field("boxKid") as usize;
    let box_anchor_wraps = num_field("boxAWrap") as usize;
    let box_anchor_empty = num_field("boxAEmpty") as usize;
    let forced_boxes = num_field("forced") as usize;
    let hidden_subtree = num_field("hid") as usize;
    let clipped_away = num_field("clip") as usize;
    let mut timings = [0u64; 5];
    if let Some(JSValue::Array(cols)) = root.get("t") {
        for (i, col) in cols.iter().take(5).enumerate() {
            if let JSValue::Number(n) = col {
                timings[i] = (*n).max(0.0) as u64;
            }
        }
    }
    let mut skipped = [0usize; 5];
    if let Some(JSValue::Array(cols)) = root.get("skip") {
        for (i, col) in cols.iter().take(5).enumerate() {
            if let JSValue::Number(n) = col {
                skipped[i] = (*n).max(0.0) as usize;
            }
        }
    }
    let Some(JSValue::Array(rows)) = root.get("nodes") else {
        return DomSnapshot {
            nodes: Vec::new(),
            scroll,
            doc,
            total,
            culled,
            culled_ids,
            skipped,
            box_anchors,
            boxless_with_kids,
            box_anchor_wraps,
            box_anchor_empty,
            forced_boxes,
            hidden_subtree,
            clipped_away,
            timings,
            active,
            doc_token,
        };
    };
    let mut nodes = Vec::new();
    for row in rows {
        let JSValue::Array(cols) = row else {
            continue;
        };
        let num = |i: usize| match cols.get(i) {
            Some(JSValue::Number(n)) => *n as f32,
            _ => f32::NAN,
        };
        let str_ = |i: usize| match cols.get(i) {
            Some(JSValue::String(s)) => s.clone(),
            _ => String::new(),
        };
        let id = num(0);
        let (x, y, w, h) = (num(4), num(5), num(6), num(7));
        if !id.is_finite() || id < 0.0 {
            continue;
        }
        if !(w > 0.0 && h > 0.0) {
            continue;
        }
        if !(x.is_finite() && y.is_finite()) {
            continue;
        }
        nodes.push(DomRaw {
            id: id as u64,
            tag: str_(1),
            role: str_(2),
            typ: str_(3),
            x,
            y,
            w,
            h,
            text: str_(8),
            url: str_(9),
            autofocus: matches!(cols.get(10), Some(JSValue::Number(n)) if *n != 0.0),
        });
    }
    DomSnapshot {
        nodes,
        scroll,
        doc,
        total,
        culled,
        culled_ids,
        skipped,
        box_anchors,
        boxless_with_kids,
        box_anchor_wraps,
        box_anchor_empty,
        forced_boxes,
        hidden_subtree,
        clipped_away,
        timings,
        active,
        doc_token,
    }
}

/// Map a DOM node to a cursor kind. Explicit `role=` wins (ARIA authors know
/// best); otherwise the tag (and input type) decides. Unknown but queried
/// elements (they matched the actionable selector) fall back to `Other`.
pub fn dom_kind(tag: &str, role: &str, typ: &str) -> FocusKind {
    match role {
        "link" | "doc-backlink" => return FocusKind::Link,
        "button"
        | "menuitem"
        | "tab"
        | "switch"
        | "checkbox"
        | "radio"
        | "menuitemcheckbox"
        | "menuitemradio"
        | "menu-listbox-option"
        | "option" => return FocusKind::Button,
        "textbox" | "searchbox" | "combobox" => return FocusKind::Field,
        _ => {}
    }
    match tag {
        "a" => FocusKind::Link,
        "button" => FocusKind::Button,
        "textarea" | "select" => FocusKind::Field,
        "input" => match typ {
            "checkbox" | "radio" | "submit" | "button" | "reset" | "image" | "color" | "range" => {
                FocusKind::Button
            }
            _ => FocusKind::Field,
        },
        _ => FocusKind::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use embedder_traits::JSValue;
    use std::collections::HashMap;

    fn row(cols: Vec<JSValue>) -> JSValue {
        JSValue::Array(cols)
    }

    fn num(n: f64) -> JSValue {
        JSValue::Number(n)
    }

    fn str_(s: &str) -> JSValue {
        JSValue::String(s.to_string())
    }

    fn snapshot(rows: Vec<JSValue>) -> JSValue {
        let mut root = HashMap::new();
        root.insert("nodes".to_string(), JSValue::Array(rows));
        JSValue::Object(root)
    }

    #[test]
    fn parse_accepts_well_formed_rows() {
        let value = JSValue::Object({
            let mut root = HashMap::new();
            root.insert(
                "nodes".to_string(),
                JSValue::Array(vec![
                    row(vec![
                        num(7.0),
                        str_("textarea"),
                        str_(""),
                        str_(""),
                        num(10.0),
                        num(20.0),
                        num(300.0),
                        num(40.0),
                        str_("Search"),
                    ]),
                    row(vec![
                        num(9.0),
                        str_("input"),
                        str_(""),
                        str_("password"),
                        num(10.0),
                        num(70.0),
                        num(300.0),
                        num(40.0),
                        str_(""),
                    ]),
                ]),
            );
            root
        });
        let out = parse_dom_snapshot(&value);
        assert_eq!(out.nodes.len(), 2);
        assert_eq!(out.nodes[0].id, 7);
        assert_eq!(out.nodes[0].tag, "textarea");
        assert_eq!(out.nodes[0].x, 10.0);
        assert_eq!(out.nodes[1].typ, "password");
        // No link/flag columns in this fixture: empty + unset.
        assert_eq!(out.nodes[0].url, "");
        assert!(!out.nodes[0].autofocus);
        // No scroll fields in this fixture: origin, the safe assumption.
        assert_eq!(out.scroll, (0.0, 0.0));
    }

    #[test]
    fn parse_reads_href_and_autofocus() {
        let value = JSValue::Object({
            let mut root = HashMap::new();
            root.insert(
                "nodes".to_string(),
                JSValue::Array(vec![row(vec![
                    num(3.0),
                    str_("a"),
                    str_(""),
                    str_(""),
                    num(10.0),
                    num(20.0),
                    num(100.0),
                    num(30.0),
                    str_("Example"),
                    str_("https://example.com/page?q=1"),
                    num(1.0),
                ])]),
            );
            root
        });
        let out = parse_dom_snapshot(&value);
        assert_eq!(out.nodes.len(), 1);
        assert_eq!(out.nodes[0].url, "https://example.com/page?q=1");
        assert!(out.nodes[0].autofocus);
    }

    #[test]
    fn parse_reads_page_scroll_and_doc_dims() {
        let mut root = HashMap::new();
        root.insert("nodes".to_string(), JSValue::Array(vec![]));
        root.insert("sx".to_string(), num(0.0));
        root.insert("sy".to_string(), num(1240.0));
        root.insert("dw".to_string(), num(800.0));
        root.insert("dh".to_string(), num(4000.0));
        root.insert("total".to_string(), num(1500.0));
        root.insert("culled".to_string(), num(900.0));
        root.insert(
            "skip".to_string(),
            JSValue::Array(vec![num(3.0), num(0.0), num(1.0), num(0.0), num(2.0)]),
        );
        root.insert("boxA".to_string(), num(7.0));
        root.insert("boxKid".to_string(), num(5.0));
        root.insert("boxAWrap".to_string(), num(4.0));
        root.insert("boxAEmpty".to_string(), num(2.0));
        root.insert("forced".to_string(), num(6.0));
        root.insert("hid".to_string(), num(4.0));
        root.insert("clip".to_string(), num(3.0));
        root.insert(
            "t".to_string(),
            JSValue::Array(vec![num(5.0), num(40.0), num(12.0), num(3.0), num(1.0)]),
        );
        root.insert(
            "ccull".to_string(),
            JSValue::Array(vec![num(11.0), num(13.0)]),
        );
        root.insert("ax".to_string(), num(9.0));
        let out = parse_dom_snapshot(&JSValue::Object(root));
        assert!(out.nodes.is_empty());
        assert_eq!(out.scroll, (0.0, 1240.0));
        assert_eq!(out.doc, (800.0, 4000.0));
        assert_eq!(out.total, 1500);
        assert_eq!(out.culled, 900);
        assert_eq!(out.skipped, [3, 0, 1, 0, 2]);
        assert_eq!(out.box_anchors, 7);
        assert_eq!(out.boxless_with_kids, 5);
        assert_eq!(out.box_anchor_wraps, 4);
        assert_eq!(out.box_anchor_empty, 2);
        assert_eq!(out.forced_boxes, 6);
        assert_eq!(out.hidden_subtree, 4);
        assert_eq!(out.clipped_away, 3);
        assert_eq!(out.timings, [5, 40, 12, 3, 1]);
        assert_eq!(out.culled_ids, vec![11, 13]);
        assert_eq!(out.active, 9);
    }

    #[test]
    fn parse_defaults_active_to_none() {
        // Old cached pages predate the focus field: missing `ax` means
        // no focused element, never a dock.
        let mut root = HashMap::new();
        root.insert("nodes".to_string(), JSValue::Array(vec![]));
        let out = parse_dom_snapshot(&JSValue::Object(root));
        assert_eq!(out.active, 0);
    }

    #[test]
    fn parse_skips_garbage_without_failing() {
        let value = snapshot(vec![
            num(1.0),
            str_("nope"),
            row(vec![num(2.0)]),
            row(vec![
                num(f64::NAN),
                str_("a"),
                str_(""),
                str_(""),
                num(0.0),
                num(0.0),
                num(-5.0),
                num(10.0),
                str_(""),
            ]),
            // Flat numbers and short rows are skipped; the NaN/negative
            // row is skipped; the valid row still parses.
            row(vec![
                num(3.0),
                str_("button"),
                str_(""),
                str_(""),
                num(5.0),
                num(5.0),
                num(50.0),
                num(20.0),
                str_("Go"),
            ]),
        ]);
        let out = parse_dom_snapshot(&value);
        assert_eq!(out.nodes.len(), 1);
        assert_eq!(out.nodes[0].id, 3);
    }

    #[test]
    fn install_pings_fetch_only() {
        // The ping URL must embed the marker: the backend matches resource
        // loads by substring.
        assert!(DOM_PING_URL.contains(DOM_DIRTY_MARKER));
        assert!(DOM_INSTALL_JS.contains(DOM_PING_URL));
        assert!(DOM_INSTALL_JS.contains(DOM_DIRTY_MARKER));
        assert!(DOM_INSTALL_JS.contains("MutationObserver"));
        assert!(DOM_INSTALL_JS.contains("ResizeObserver"));
        assert!(DOM_INSTALL_JS.contains("addEventListener('scroll'"));
        assert!(DOM_INSTALL_JS.contains("fetch("));
        // Pings carry window truth so nested scrolls can't poison the
        // global offset.
        assert!(DOM_INSTALL_JS.contains("wx="));
        // Console and beacon channels removed: no page-log pollution, one
        // network channel to reason about.
        assert!(!DOM_INSTALL_JS.contains("console"));
        assert!(!DOM_INSTALL_JS.contains("sendBeacon"));
        // Idempotent per document: re-running never double-arms.
        assert!(DOM_INSTALL_JS.contains("S.mo"));
        // Exact-element activation helper + id map (no hit-test staleness).
        assert!(DOM_INSTALL_JS.contains("__sdsc_click"));
        assert!(DOM_INSTALL_JS.contains("byId"));
        // Union-of-fragments measurement: never the crashing call.
        let query = dom_query_js(None, 7);
        assert!(query.contains("getClientRects"));
        assert!(!query.contains("getBoundingClientRect"));
        // Boxless fallback is persistent display forcing (one shot per
        // element; our own observer re-pulls on the mutation).
        assert!(!query.contains("selectNodeContents"));
        assert!(query.contains("setProperty('display', 'block'"));
        assert!(!query.contains("removeProperty"));
        // Measure-first ordering: verdicts (getComputedStyle, ~15ms/call
        // in debug Servo) run only on geometric-cull survivors and boxless
        // triage remainders — never on all N candidates up front.
        assert!(query.contains("isHidden(el)"));
        assert!(query.contains("boxless"));
        assert!(query.contains("closest('[hidden]')"));
        // Cursor culling: radial keep + directional-next keep, uncensored
        // without a cursor.
        assert!(query.contains("__sdsc_cc=false"));
        assert!(dom_query_js(Some((100.0, 200.0)), 7).contains("__sdsc_cc=true"));
        assert!(query.contains("__sdsc_cx"));
        // Epoch-gated visibility verdicts + tagged ping sources.
        assert!(query.contains("S.visEpoch"));
        assert!(query.contains("visEpoch"));
        assert!(DOM_INSTALL_JS.contains("ev="));
        // Ping taxonomy: attribute mutations invalidate verdicts ('ma'),
        // structural changes ('mc') and scrolls ('sc') don't; trailing
        // debounce escalates mc->ma so a hide is never swallowed.
        assert!(DOM_INSTALL_JS.contains("'ma'"));
        assert!(DOM_INSTALL_JS.contains("'mc'"));
        assert!(DOM_INSTALL_JS.contains("'sc'"));
        assert!(DOM_INSTALL_JS.contains("pendingEv"));
        // Invalidation is name-filtered: only class/style/hidden (or
        // unknown) attributes escalate to 'ma'.
        assert!(DOM_INSTALL_JS.contains("attributeName"));
        assert!(dom_query_js(Some((100.0, 200.0)), 7).contains("__sdsc_cx=100"));
        assert!(dom_query_js(Some((100.0, 200.0)), 7).contains("__sdsc_v=7"));
        // Boot = install + snapshot in one round-trip.
        let boot = dom_boot_js(None, 7);
        assert!(boot.contains(DOM_DIRTY_MARKER));
        assert!(boot.contains("querySelectorAll"));
        // Both scripts refuse to run in subframes: UserContentManager
        // installs per document, and iframe documents are exactly where
        // Servo's script/layout stability asserts fire.
        assert!(DOM_INSTALL_JS.contains("window.top !== window.self"));
        assert!(DOM_QUERY_BODY.contains("window.top !== window.self"));
        // Document identity token: a same-document head re-parse keeps it,
        // a real reload gets a fresh one.
        assert!(DOM_INSTALL_JS.contains("S.doc"));
        assert!(dom_query_js(None, 7).contains("dt: S.doc"));
    }

    #[test]
    fn probe_parses_marker_triplet() {
        use embedder_traits::JSValue;
        let good = JSValue::Array(vec![
            JSValue::Number(1.0),
            JSValue::Number(0.0),
            JSValue::Number(1.0),
        ]);
        assert_eq!(parse_loop_probe(&good), Some([true, false, true]));
        let unarmed = JSValue::Array(vec![
            JSValue::Number(-1.0),
            JSValue::Number(-1.0),
            JSValue::Number(-1.0),
        ]);
        assert_eq!(parse_loop_probe(&unarmed), None);
        let short = JSValue::Array(vec![JSValue::Number(1.0)]);
        assert_eq!(parse_loop_probe(&short), None);
    }

    #[test]
    fn storage_probe_reports_counts_not_values() {
        // Census only: sizes, truncated key names, probe-key round-trip,
        // cookie count. Values must never be serialized out of the page.
        assert!(STORAGE_PROBE_JS.contains("localStorage"));
        assert!(STORAGE_PROBE_JS.contains("sessionStorage"));
        assert!(STORAGE_PROBE_JS.contains("document.cookie"));
        assert!(STORAGE_PROBE_JS.contains("__sdsc_probe"));
        assert!(STORAGE_PROBE_JS.contains("JSON.stringify"));
        // No wholesale value dump: the only getItem reads the probe's own key.
        assert_eq!(STORAGE_PROBE_JS.matches("getItem").count(), 1);
    }

    #[test]
    fn query_covers_option_role() {
        // Autocomplete rows (role=option) are real click targets.
        assert!(dom_query_js(None, 7).contains("[role=\"option\"]"));
        assert_eq!(dom_kind("div", "option", ""), FocusKind::Button);
    }

    #[test]
    fn query_covers_handler_attributes() {
        // Any anchor qualifies (href-less jsaction anchors, name anchors —
        // the size filter keeps phantoms out) plus inline handlers.
        let query = dom_query_js(None, 7);
        assert!(query.contains("'a,button,"));
        assert!(query.contains("[onclick]"));
        // Full-viewport boxes never qualify (a screen-sized box with a
        // global handler would become a screen-hogging phantom); full-width
        // rows survive because both axes are required.
        assert!(query.contains("r.width >= vw && r.height >= vh"));
    }

    #[test]
    fn kind_prefers_role_then_tag() {
        assert_eq!(dom_kind("div", "button", ""), FocusKind::Button);
        assert_eq!(dom_kind("div", "link", ""), FocusKind::Link);
        assert_eq!(dom_kind("textarea", "", ""), FocusKind::Field);
        assert_eq!(dom_kind("input", "", "password"), FocusKind::Field);
        assert_eq!(dom_kind("input", "", "checkbox"), FocusKind::Button);
        assert_eq!(dom_kind("input", "", "submit"), FocusKind::Button);
        assert_eq!(dom_kind("a", "", ""), FocusKind::Link);
        assert_eq!(dom_kind("button", "", ""), FocusKind::Button);
    }
}
