//! DOM target query for the CEF OSR backend.
//!
//! CEF polls the live DOM from the page's V8 context through the renderer
//! process message bridge (see `cef_backend.rs`). The query returns a stable
//! per-document id, viewport-space boxes, visibility verdicts, overflow
//! clipping, scroll truth and document extents; Rust parses it as JSON.
//!
//! Polling is deliberate: the DOM cannot notify us without a network ping or
//! a renderer-side observer bridge, and the 350ms CEF cadence plus reseed on
//! D-pad jumps keeps targets fresh without per-mutation traffic.

use crate::browser::cursor::FocusKind;

/// One-shot event-loop probe: arms timers, rAF, and an IntersectionObserver
/// so a later read can tell whether page JS task sources are running.
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

/// Read-only DOM query. Stable per-document ids via an in-page WeakMap
/// (recreated on every navigation, so ids are only meaningful within one
/// document). Bounded to 1000 matches; reports truncation.
/// `center` is the pad cursor in viewport px (or `None`): candidates beyond
/// 500px of it are culled — except the nearest target in each direction, so
/// arrow-key jumps always have somewhere to go. `None` keeps everything.
/// `epoch` bumps whenever the DOM (or anything affecting visibility) may
/// have changed; the in-page visibility verdict cache resets on mismatch.
pub fn dom_query_js(center: Option<(f32, f32)>, epoch: u64) -> String {
    let (cx, cy, on) = match center {
        Some((x, y)) => (format!("{x:.0}"), format!("{y:.0}"), "true"),
        None => ("0".to_string(), "0".to_string(), "false"),
    };
    format!(
        "var __sdsc_cx={cx},__sdsc_cy={cy},__sdsc_cc={on},__sdsc_v={epoch};
         var __sdsc_obj = {DOM_QUERY_BODY};
JSON.stringify(__sdsc_obj);"
    )
}

const DOM_QUERY_BODY: &str = r###"(function(){
  // Top document only: no snap targets are collected from transient iframe
  // documents.
  try { if (window.top !== window.self) return {}; } catch (e) { return {}; }
  var S = window.__sdsc_ax;
  if (!S) { S = window.__sdsc_ax = { next: 1, ids: new WeakMap() }; }
  // Exact-element click map (`window.__sdsc_click`); rebuilt per pull so
  // detached nodes release.
  if (!S.byId) { S.byId = {}; }
  if (!window.__sdsc_click) {
    window.__sdsc_click = function(id) {
      try {
        var el = S.byId[id];
        if (!el) return 'missing';
        el.click();
        return 'ok';
      } catch (e) { return 'threw'; }
    };
  }
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
  if (S.visEpoch !== __sdsc_v) {
    S.vis = (typeof WeakMap !== "undefined") ? new WeakMap() : null;
    S.visSet = (typeof Set !== "undefined") ? new Set() : null;
    S.visAnc = (typeof Set !== "undefined") ? new Set() : null;
    S.visEpoch = __sdsc_v;
  }
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
    try {
      if (S.vis) S.vis.set(el, hidden);
      if (S.visSet) S.visSet.add(el);
      // Ancestors of cached elements: a mutation here can hide them.
      if (S.visAnc) {
        var a = el.parentElement, d = 0;
        while (a && d < 32) { S.visAnc.add(a); a = a.parentElement; d++; }
      }
    } catch (e2) {}
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
  // Deliberately the plural fragment call: the union below normalizes
  // multi-line/inline fragments and can never go negative (real pages have
  // negative border boxes from transformed content).
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
  // Deliberately NO visibility verdicts here: one getComputedStyle call is
  // expensive while getClientRects unions are microseconds, so verdicts run
  // only on elements that survive geometric
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
  // drives cursor docking on autofocus / JS .focus(). `ae` additionally
  // reports whether it accepts text, which drives the on-screen keyboard.
  var ax = 0;
  var aeInfo = null;
  try {
    var ae = document.activeElement;
    if (ae && ae !== document.body && ae !== document.documentElement) {
      ax = idFor(ae);
      var tag = (ae.tagName || "").toLowerCase();
      var typ = "";
      try { typ = (ae.getAttribute ? (ae.getAttribute("type") || "") : "").toLowerCase(); } catch (e5) {}
      var editable = 0;
      try {
        if (tag === "input" || tag === "textarea") {
          // Text-like inputs only: buttons, checkboxes, file pickers and
          // colour/range controls focus without accepting typed text.
          var nonText = /^(button|checkbox|color|file|hidden|image|radio|range|reset|submit)$/;
          editable = (!ae.disabled && !ae.readOnly && !nonText.test(typ)) ? 1 : 0;
        } else if (ae.isContentEditable) {
          editable = 1;
        } else {
          var arole = (ae.getAttribute ? (ae.getAttribute("role") || "") : "").toLowerCase();
          editable = (arole === "textbox" || arole === "searchbox") ? 1 : 0;
        }
      } catch (e6) {}
      aeInfo = {e: editable, t: tag, y: typ};
    }
  } catch (e) {}
  return {nodes: out, total: total, truncated: els.length > n, culled: culled, ccull: ccull, skip: [skBox, skZero, skBig, skDis, skErr], hid: skHid, clip: skClip, boxA: skBoxA, boxKid: skBoxKid, boxAWrap: skBoxAWrap, boxAEmpty: skBoxAEmpty, forced: skForce, t: [Math.round(tQ), Math.round(tmM), Math.round(tmV), Math.round(tmC), Math.round(tmT)], sx: Math.round(sx), sy: Math.round(sy), dw: Math.round(dw), dh: Math.round(dh), ax: ax, ae: aeInfo, dt: S.doc};
})()"###;

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

    #[test]
    fn query_is_a_json_string_and_covers_controls() {
        let js = dom_query_js(Some((10.0, 20.0)), 7);
        assert!(js.contains("JSON.stringify"));
        assert!(js.contains("__sdsc_click"));
        assert!(js.contains("__sdsc_cx=10"));
        assert!(js.contains("role=\"button\""));
        assert!(js.contains("role=\"option\""));
        assert!(js.contains("isContentEditable"));
        assert!(js.contains("ae: aeInfo"));
        assert_eq!(dom_kind("div", "option", ""), FocusKind::Button);
        assert_eq!(dom_kind("a", "", ""), FocusKind::Link);
        assert_eq!(dom_kind("input", "", "text"), FocusKind::Field);
    }
}
