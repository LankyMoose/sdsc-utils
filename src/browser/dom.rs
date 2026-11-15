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
  var S = window.__sdsc_ax;
  if (!S) { S = window.__sdsc_ax = { next: 1, ids: new WeakMap() }; }
  if (S.mo) return;
  S.mo = true;
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
  var lastPing = 0, trailing = null;
  var SUPPRESS = 500, TRAIL = 250;
  function fire(){
    lastPing = Date.now();
    // Window truth rides along: a nested-container scroll moves only that
    // container's children, so the global offset must not absorb it — the
    // backend corrects to these coordinates on arrival instead of
    // mis-translating every outline until the next settle pull.
    var wx = 0, wy = 0;
    try { wx = Math.round(window.scrollX || window.pageXOffset || 0); wy = Math.round(window.scrollY || window.pageYOffset || 0); } catch (e) {}
    try { fetch(PING + '?wx=' + wx + '&wy=' + wy, {method:'POST', mode:'no-cors', keepalive:true, body:'1'}).catch(function(){}); } catch (e) {}
  }
  function onMut(){
    var now = Date.now();
    if (now - lastPing >= SUPPRESS && !trailing) { fire(); }
    else if (!trailing) {
      trailing = setTimeout(function(){ trailing = null; fire(); }, TRAIL);
    }
  }
  function arm(){
    var root = document.documentElement;
    if (!root) { setTimeout(arm, 500); return; }
    try {
      new MutationObserver(onMut).observe(root, {subtree:true, childList:true, attributes:true, characterData:true});
    } catch (e) {}
    try {
      new ResizeObserver(onMut).observe(document.body || root);
    } catch (e) {}
  }
  arm();
  try { window.addEventListener('load', onMut); } catch (e) {}
  // Scrolls (page or nested containers) change viewport rects with no DOM
  // mutation, so the observers alone never hear them. Capture phase: scroll
  // events don't bubble, this catches every scroller in the document.
  try { document.addEventListener('scroll', onMut, {capture:true, passive:true}); } catch (e) {}
})()"###;

/// Boot script: idempotent install + immediate snapshot, so one round-trip
/// both arms the observers and returns the current targets. The install is
/// explicitly semicolon-terminated: without it the two adjacent IIFEs parse
/// as a call on the install's (undefined) return value (ASI hazard).
pub fn dom_boot_js() -> String {
    format!("{DOM_INSTALL_JS};\n{DOM_QUERY_JS}")
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
pub const DOM_QUERY_JS: &str = r###"(function(){
  var S = window.__sdsc_ax;
  if (!S) { S = window.__sdsc_ax = { next: 1, ids: new WeakMap() }; }
  function idFor(el) {
    var id = S.ids.get(el);
    if (id === undefined) { id = S.next++; S.ids.set(el, id); }
    S.byId[id] = el;
    return id;
  }
  var SEL = 'a,button,input,textarea,select,[onclick],[role="button"],[role="link"],[role="checkbox"],[role="textbox"],[role="searchbox"],[role="switch"],[role="combobox"],[role="menuitem"],[role="tab"],[role="option"]';
  var out = [];
  var culled = 0;
  // Skip census: WHY matched candidates never become targets. `total` says
  // how many matched; these say where the rest went. Read the ingest log.
  // skBox = no boxes; skBoxA = of those, anchors; skBoxKid = of those, with
  // element children (the wrapper signature: boxless parent, boxed kids).
  var skBox = 0, skZero = 0, skBig = 0, skDis = 0, skErr = 0;
  var skBoxA = 0, skBoxKid = 0, skBoxAWrap = 0, skBoxAEmpty = 0;
  S.byId = {};
  var els;
  try { els = document.querySelectorAll(SEL); } catch (e) { return {nodes: [], total: 0, truncated: false}; }
  var total = els.length;
  var n = Math.min(els.length, 1000);
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
    var boxes = [];
    var j;
    for (j = 0; j < rs.length; j++) boxes.push(rs[j]);
    if (boxes.length === 0) {
      // No own box: wrapper inlines (text-only anchors live here in Servo —
      // no associated fragments, while Chrome synthesizes a content union).
      // Measure the *contents* via Range: spec-defined, covers text runs
      // element queries structurally miss, single native call, and the Range
      // path carries no debug assert. selectNodeContents on an empty element
      // yields a collapsed range → no boxes → still skipped.
      try {
        var rng = document.createRange();
        rng.selectNodeContents(el);
        var rr = rng.getClientRects();
        for (var k = 0; k < rr.length; k++) boxes.push(rr[k]);
      } catch (e2) {}
    }
    var x0 = Infinity, y0 = Infinity, x1 = -Infinity, y1 = -Infinity;
    for (j = 0; j < boxes.length; j++) {
      var q = boxes[j];
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
  for (var i = 0; i < n; i++) { (function(el){
    try {
      if (el.disabled) { skDis++; return; }
      var t = el.tagName ? el.tagName.toLowerCase() : "";
      var ty = "";
      if (t === "input") {
        ty = (el.getAttribute("type") || "text").toLowerCase();
        if (ty === "hidden") return;
      }
      var r = boxOf(el);
      if (!r) {
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
        return;
      }
      if (!(r.width > 0 && r.height > 0)) { skZero++; return; }
      var vw = window.innerWidth || 800, vh = window.innerHeight || 600;
      // Full-viewport-or-larger boxes are never snap targets: page roots and
      // app shells (often carrying global [onclick]) would otherwise become
      // screen-hogging phantoms. Strictly smaller in both axes to qualify.
      if (r.width >= vw || r.height >= vh) { skBig++; return; }
      // Viewport cull (1-viewport margin): offscreen targets are dead weight
      // in transfer, merge, and magnet cost — and worse, document-order
      // truncation would keep them while dropping visible ones. Culled nodes
      // re-enter on the next scroll-settle pull; scroll clamping uses the
      // document dims below, never the live-set corners.
      if (r.x + r.width < -vw || r.x > vw + vw || r.y + r.height < -vh || r.y > vh + vh) {
        culled++;
        return;
      }
      var role = el.getAttribute ? (el.getAttribute("role") || "").toLowerCase() : "";
      var txt = "";
      try { txt = (el.innerText || el.textContent || "").replace(/\s+/g, " ").slice(0, 40); } catch (e2) {}
      out.push([idFor(el), t, role, ty, Math.round(r.x), Math.round(r.y), Math.round(r.width), Math.round(r.height), txt]);
    } catch (e) { skErr++; }
  })(els[i]); }
  var sx = 0, sy = 0;
  try { sx = window.scrollX || window.pageXOffset || 0; sy = window.scrollY || window.pageYOffset || 0; } catch (e) {}
  var dw = 0, dh = 0;
  try {
    var de = document.documentElement;
    if (de) { dw = de.scrollWidth || 0; dh = de.scrollHeight || 0; }
  } catch (e) {}
  return {nodes: out, total: total, truncated: els.length > n, culled: culled, skip: [skBox, skZero, skBig, skDis, skErr], boxA: skBoxA, boxKid: skBoxKid, boxAWrap: skBoxAWrap, boxAEmpty: skBoxAEmpty, sx: Math.round(sx), sy: Math.round(sy), dw: Math.round(dw), dh: Math.round(dh)};
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
/// with element children, `box_anchor_wraps` their intersection, and
/// `box_anchor_empty` the boxless anchors with no children at all
/// (text-only anchors are the remainder: boxA - wraps - empty).
#[derive(Debug, Clone, PartialEq)]
pub struct DomSnapshot {
    pub nodes: Vec<DomRaw>,
    pub scroll: (f32, f32),
    pub doc: (f32, f32),
    pub total: usize,
    pub culled: usize,
    pub skipped: [usize; 5],
    pub box_anchors: usize,
    pub boxless_with_kids: usize,
    pub box_anchor_wraps: usize,
    pub box_anchor_empty: usize,
}

/// Parse a `DOM_QUERY_JS` result into raw candidates. Lenient by design:
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
            skipped: [0; 5],
            box_anchors: 0,
            boxless_with_kids: 0,
            box_anchor_wraps: 0,
            box_anchor_empty: 0,
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
    let total = num_field("total") as usize;
    let culled = num_field("culled") as usize;
    let box_anchors = num_field("boxA") as usize;
    let boxless_with_kids = num_field("boxKid") as usize;
    let box_anchor_wraps = num_field("boxAWrap") as usize;
    let box_anchor_empty = num_field("boxAEmpty") as usize;
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
            skipped,
            box_anchors,
            boxless_with_kids,
            box_anchor_wraps,
            box_anchor_empty,
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
        });
    }
    DomSnapshot {
        nodes,
        scroll,
        doc,
        total,
        culled,
        skipped,
        box_anchors,
        boxless_with_kids,
        box_anchor_wraps,
        box_anchor_empty,
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
        // No scroll fields in this fixture: origin, the safe assumption.
        assert_eq!(out.scroll, (0.0, 0.0));
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
        assert!(DOM_QUERY_JS.contains("getClientRects"));
        assert!(!DOM_QUERY_JS.contains("getBoundingClientRect"));
        // Boxless wrappers fall back to a content Range (text runs included).
        assert!(DOM_QUERY_JS.contains("selectNodeContents"));
        // Box-free wrappers fall back to a bounded children union.
        assert!(DOM_QUERY_JS.contains("el.children"));
        // Boot = install + snapshot in one round-trip.
        let boot = dom_boot_js();
        assert!(boot.contains(DOM_DIRTY_MARKER));
        assert!(boot.contains("querySelectorAll"));
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
    fn query_covers_option_role() {
        // Autocomplete rows (role=option) are real click targets.
        assert!(DOM_QUERY_JS.contains("[role=\"option\"]"));
        assert_eq!(dom_kind("div", "option", ""), FocusKind::Button);
    }

    #[test]
    fn query_covers_handler_attributes() {
        // Any anchor qualifies (href-less jsaction anchors, name anchors —
        // the size filter keeps phantoms out) plus inline handlers.
        assert!(DOM_QUERY_JS.contains("'a,button,"));
        assert!(DOM_QUERY_JS.contains("[onclick]"));
        // Full-viewport boxes never qualify (page roots with global
        // handlers would become screen-hogging phantoms).
        assert!(DOM_QUERY_JS.contains("r.width >= vw || r.height >= vh"));
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
