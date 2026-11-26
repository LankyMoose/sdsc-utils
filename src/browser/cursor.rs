//! Virtual cursor for controller-first browsing.
//!
//! The pad owns a cursor over the rendered page: the left stick moves a
//! free point with velocity + friction, D-pad jumps fly between focusables,
//! and the point docks onto whatever element it actually arrives at.
//! Docking centers the point on the element; pushing off leaves from the
//! rect edge along the push. Rendering morphs a ring between dot (free) and
//! wrap (docked) every tick — size and shape always interpolate, so no
//! frame pops. Cross clicks the attached element (or the free point).
//!
//! Focusables arrive later via the accessibility tree
//! (`EngineEvent::A11yTree`); until then the list is empty and the cursor
//! roams free. All geometry is page pixels at 1x (the engine viewport is
//! fixed), so the UI maps to card space with a single uniform scale.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use crate::domain::pad::PadSample;

/// Focusable element, in page pixels.
#[derive(Debug, Clone, PartialEq)]
pub struct Focusable {
    pub id: u64,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub kind: FocusKind,
    /// Resolved link target (hovered-link status); `None` when none.
    pub url: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusKind {
    Link,
    Button,
    Field,
    Other,
}

/// Left-stick cursor acceleration at full deflection, px/s^2.
///
/// Friction settles the glide at `accel / CURSOR_FRICTION`, so this is the
/// real base-speed knob; the cap below is only a safety ceiling (R2 turbo
/// doubles both).
pub const CURSOR_ACCEL: f32 = 10500.0;
/// Left-stick cursor top speed, px/s.
pub const CURSOR_MAX_SPEED: f32 = 2100.0;
/// Velocity decay rate (exponential friction, per second). The cursor glides
/// a little after release instead of stopping dead.
pub const CURSOR_FRICTION: f32 = 14.0;
/// Dock entry/exit pads in page pixels: the outline engages when the point
/// enters an element's rect + ACQUIRE_PAD and holds until it leaves +
/// HOLD_PAD, so edge dither never flickers the wrap.
pub const ACQUIRE_PAD: f32 = 8.0;
pub const HOLD_PAD: f32 = 24.0;
/// Full undock → travel → dock beat for a D-pad nudge, seconds. Distance
/// independent: every jump takes the same 150ms, and the outline morph
/// uses the same rate so the whole transition is one consistent beat.
pub const NUDGE_SECS: f32 = 0.150;
/// Dock-blend rate: the outline morphs free↔docked in [`NUDGE_SECS`],
/// eased every analog tick so no frame pops.
pub const DOCK_BLEND_PER_SEC: f32 = 1.0 / NUDGE_SECS;
/// Break-free stick deflection while docked: past 25% a *fresh* push
/// releases at once (detach + outward kick) instead of dragging the dock
/// along. Fresh means the stick dropped below [`BREAK_REARM_DEFLECT`]
/// since docking — landing with the stick still held never instantly
/// kicks back off (human release time is ~200ms; the old level trigger
/// broke every arrival on the very next tick). A hold sustained past
/// [`BREAK_LATCH_GRACE_SECS`] breaks anyway, so push-through traversal
/// never strands on a landing.
pub const BREAK_FREE_DEFLECT: f32 = 0.25;
/// Re-arm deflection: the stick must fall below this after docking before
/// a new push can break free again.
pub const BREAK_REARM_DEFLECT: f32 = 0.1;
/// Latch grace, seconds: a landing held past this long breaks free anyway.
/// Covers release latency with margin, then yields to a deliberate hold.
pub const BREAK_LATCH_GRACE_SECS: f32 = 0.3;
/// Latch grace, seconds: a landing held past this long breaks free anyway.
/// Covers release latency with margin, then yields to a deliberate hold.
/// Outward kick on break-free, px/s: immediate motion in the push direction.
pub const BREAK_KICK: f32 = 800.0;
/// Right-stick scroll speed at full deflection, px/s.
pub const SCROLL_SPEED: f32 = 1500.0;
/// Engine viewport the cursor space matches. The engine resizes its
/// offscreen surface to the browser card's page area (16:9), so this is the
/// startup/default size; live cursors carry their own `view_w/view_h`.
pub const VIEWPORT_W: f32 = 800.0;
pub const VIEWPORT_H: f32 = 600.0;

/// Directional jump axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JumpDir {
    Up,
    Down,
    Left,
    Right,
}

/// Page-pixel rectangle: origin + size.
type CursorRect = (f32, f32, f32, f32);

#[derive(Debug, Clone)]
pub struct Cursor {
    pub x: f32,
    pub y: f32,
    pub vx: f32,
    pub vy: f32,
    /// Attached focusable id, if docked.
    pub attached: Option<u64>,
    /// Outline morph state: 0.0 = free dot, 1.0 = docked wrap. Eased every
    /// analog tick toward the attachment so the shape always interpolates.
    pub dock_blend: f32,
    /// Last docked rect (page px), kept across undock so the wrap visibly
    /// morphs back into the dot instead of vanishing.
    pub last_dock: Option<(f32, f32, f32, f32)>,
    /// D-pad flight target (page px): jumps glide here over [`NUDGE_SECS`]
    /// instead of teleporting; stick input cancels.
    pub fly_to: Option<(f32, f32)>,
    /// Flight origin and elapsed seconds: the tween is fixed-duration, so
    /// long and short nudges read the same.
    fly_from: (f32, f32),
    fly_elapsed: f32,
    /// Break-free suppression: id that must not re-dock until the point
    /// leaves its hold pad (prevents instant re-capture after a push-off).
    pub no_dock: Option<u64>,
    /// Break-free arming: set on every dock, cleared only once the stick
    /// drops below [`BREAK_REARM_DEFLECT`]. A fresh push past
    /// [`BREAK_FREE_DEFLECT`] then breaks; arriving (or lingering) with the
    /// stick held never does — unless the hold outlasts
    /// [`BREAK_LATCH_GRACE_SECS`], which breaks through anyway.
    pub break_armed: bool,
    /// Seconds docked (accumulates while attached): bounds the latch above.
    pub docked_for: f32,
    /// Docking mode (L3 toggles): the point docks onto elements it arrives
    /// at. Off = pure free-form virtual mouse; D-pad jumping still applies.
    pub magnet: bool,
    /// A scroll released the dock and the next acquire must happen in
    /// place (no centre snap): content moved under a fixed point, and
    /// centring the first target after the scroll reads as a nudge.
    scroll_released: bool,
    /// R2 turbo: doubles cursor acceleration/cap and scroll speed for the
    /// duration of the hold. Docking is disabled while set (magnet on).
    pub turbo: bool,
    /// Previous tick's turbo, to detect the release edge and re-snap.
    was_turbo: bool,
    /// Direct nudge morph: while jumping from one docked target to another,
    /// the wrap rect interpolates old -> new and never collapses through
    /// the free dot.
    nudge_morph: Option<(CursorRect, CursorRect)>,
    pub visible: bool,
    /// Live page-area size in card pixels (dynamic engine viewport).
    pub view_w: f32,
    pub view_h: f32,
}

/// Perpendicular band metrics between a docked rect and a candidate for a
/// move along `dir`: `(forward gap, shared perpendicular extent,
/// perpendicular centre offset)`. Gap is clamped at zero when the rects
/// overlap on the movement axis; shared extent is zero when they do not
/// overlap on the perpendicular axis.
fn band_metrics(from: (f32, f32, f32, f32), to: &Focusable, dir: JumpDir) -> (f32, f32, f32) {
    let (fx0, fy0, fw, fh) = from;
    let (fx1, fy1) = (fx0 + fw, fy0 + fh);
    let (tx1, ty1) = (to.x + to.w, to.y + to.h);
    match dir {
        JumpDir::Up | JumpDir::Down => {
            let shared = fx1.min(tx1) - fx0.max(to.x);
            let gap = if dir == JumpDir::Up {
                fy0 - ty1
            } else {
                to.y - fy1
            };
            let column = ((fx0 + fw / 2.0) - (to.x + to.w / 2.0)).abs();
            (gap.max(0.0), shared.max(0.0), column)
        }
        JumpDir::Left | JumpDir::Right => {
            let shared = fy1.min(ty1) - fy0.max(to.y);
            let gap = if dir == JumpDir::Left {
                fx0 - tx1
            } else {
                to.x - fx1
            };
            let column = ((fy0 + fh / 2.0) - (to.y + to.h / 2.0)).abs();
            (gap.max(0.0), shared.max(0.0), column)
        }
    }
}

impl Cursor {
    pub fn new() -> Self {
        Self {
            x: VIEWPORT_W / 2.0,
            y: VIEWPORT_H / 2.0,
            vx: 0.0,
            vy: 0.0,
            attached: None,
            dock_blend: 0.0,
            last_dock: None,
            fly_to: None,
            no_dock: None,
            fly_from: (0.0, 0.0),
            fly_elapsed: 0.0,
            break_armed: true,
            docked_for: 0.0,
            magnet: true,
            scroll_released: false,
            turbo: false,
            was_turbo: false,
            nudge_morph: None,
            visible: true,
            view_w: VIEWPORT_W,
            view_h: VIEWPORT_H,
        }
    }

    /// Match the cursor space to the live page area (dynamic engine
    /// viewport). Clamps the point into the new bounds.
    pub fn set_viewport(&mut self, w: f32, h: f32) {
        let (w, h) = (w.max(1.0), h.max(1.0));
        if self.view_w == w && self.view_h == h {
            return;
        }
        self.view_w = w;
        self.view_h = h;
        self.x = self.x.clamp(0.0, w);
        self.y = self.y.clamp(0.0, h);
    }

    /// Reset for a fresh document: recenter in the **live** viewport and
    /// drop docks/flights/velocity. User preferences (magnet) and the live
    /// bounds survive — replacing the cursor with `Cursor::new()` would
    /// silently clamp it back to the compile-time 800x600 defaults, which is
    /// exactly the "cursor trapped in the top-left quarter" bug.
    pub fn reset_for_page(&mut self) {
        self.x = self.view_w / 2.0;
        self.y = self.view_h / 2.0;
        self.vx = 0.0;
        self.vy = 0.0;
        self.attached = None;
        self.dock_blend = 0.0;
        self.last_dock = None;
        self.fly_to = None;
        self.fly_from = (0.0, 0.0);
        self.fly_elapsed = 0.0;
        self.no_dock = None;
        self.break_armed = true;
        self.docked_for = 0.0;
        self.scroll_released = false;
        self.turbo = false;
        self.was_turbo = false;
        self.nudge_morph = None;
        self.visible = true;
    }

    /// Page targets are only reachable while any part of them intersects
    /// the viewport. The cursor clamps to the viewport, so off-screen page
    /// targets can only pin it to an edge with nothing visible beneath it.
    fn within_viewport(&self, item: &Focusable) -> bool {
        item.x < self.view_w
            && item.x + item.w > 0.0
            && item.y < self.view_h
            && item.y + item.h > 0.0
    }
}

impl Default for Cursor {
    /// A fresh cursor is a live one: centered, magnetized, visible — the
    /// same as [`Cursor::new`]. (A derived `Default` would park it at
    /// (0,0) with the magnet off, which silently disables autofocus
    /// docking on any page state built by derivation — observed on new
    /// tabs.)
    fn default() -> Self {
        Self::new()
    }
}

impl Cursor {
    /// Clamp a y value to the page surface.
    fn zone_y(&self, y: f32) -> f32 {
        y.clamp(0.0, self.view_h)
    }

    /// Free movement across the page surface. Any manual move drops the
    /// attachment first; docking re-engages on arrival.
    pub fn move_by(&mut self, dx: f32, dy: f32) {
        if dx == 0.0 && dy == 0.0 {
            return;
        }
        self.attached = None;
        self.x = (self.x + dx).clamp(0.0, self.view_w);
        self.y = (self.y + dy).clamp(0.0, self.view_h);
    }

    /// Analog velocity step, D-pad flights, and dock morph state. The stick
    /// accelerates a velocity vector and friction decays it; flights glide
    /// to jump targets (stick cancels). Attachment + `dock_blend`
    /// ease every tick, so the overlay never pops.
    pub fn advance(&mut self, lx: f32, ly: f32, dt_secs: f32, options: &[Focusable]) {
        self.advance_with(lx, ly, dt_secs, options, false);
    }

    /// [`advance`](Self::advance) with the page-scroll flag. While the
    /// scroll stick is active the dock is released and no new dock is
    /// acquired: the cursor simply holds its viewport point while content
    /// moves under it, and the normal neutral acquire pass re-docks in
    /// place once the scroll stops.
    pub fn advance_with(
        &mut self,
        lx: f32,
        ly: f32,
        dt_secs: f32,
        options: &[Focusable],
        scrolling: bool,
    ) {
        let turbo_end = self.was_turbo && !self.turbo;
        self.was_turbo = self.turbo;
        // Turbo disables every docking behavior: release the hold and drop
        // any direct-morph state so the point is a free 2x cursor.
        if self.turbo {
            self.attached = None;
            self.nudge_morph = None;
        }
        if scrolling {
            self.attached = None;
            self.scroll_released = true;
        }
        // D-pad flights glide to the target; stick input cancels back to
        // manual control.
        if self.fly_to.is_some() && (lx != 0.0 || ly != 0.0) {
            self.fly_to = None;
            self.nudge_morph = None;
        }
        // Letting go inside the hold pad lifts break-free suppression: the
        // point never left, so hovering means re-dock, not escape.
        if self.no_dock.is_some() && lx.hypot(ly) < 0.01 && self.fly_to.is_none() {
            self.no_dock = None;
        }
        // Docked: the point stays at its viewport position — content
        // scrolling beneath it must not drag it (that was the old
        // re-pin-to-center behavior). A firm push breaks free at once
        // (detach + place on the exit edge + kick outward); an element
        // scrolling out of the hold pad releases without a kick and lets
        // the neutral acquire pass re-dock whatever is now under the
        // point. Breaks are edge-triggered: docking latches until the
        // stick drops below BREAK_REARM_DEFLECT, so arriving with the
        // stick still held never kicks straight back off — but a hold
        // sustained past BREAK_LATCH_GRACE_SECS breaks through anyway, so
        // traversal never strands.
        if self.attached.is_some() {
            self.vx = 0.0;
            self.vy = 0.0;
            self.docked_for += dt_secs;
            let item = self
                .attached
                .and_then(|id| options.iter().find(|o| o.id == id));
            let deflect = lx.hypot(ly);
            if deflect < BREAK_REARM_DEFLECT {
                self.break_armed = true;
            }
            // Landed-held latches until release — but a hold sustained past
            // the grace still breaks through, so traversal never strands.
            let latched = !self.break_armed && self.docked_for < BREAK_LATCH_GRACE_SECS;
            let pushed = deflect > BREAK_FREE_DEFLECT && !latched;
            let held = item.is_some_and(|it| self.inside_expanded(it, HOLD_PAD));
            if pushed || !held {
                let was = self.attached;
                self.attached = None;
                if pushed {
                    self.no_dock = was;
                }
                if pushed && let Some(it) = item {
                    // Leave from the rect edge along the push: down exits
                    // mid-bottom, diagonals exit corners — the point starts
                    // where the velocity says it is going.
                    let (ex, ey) = Self::exit_point(it, lx, ly);
                    self.x = ex.clamp(0.0, self.view_w);
                    self.y = self.zone_y(ey);
                    self.vx = lx * BREAK_KICK;
                    self.vy = ly * BREAK_KICK;
                }
                // Scrolled out from under the point: fall through to the
                // acquire pass, which docks in place on neutral ticks.
            } else {
                self.ease_blend(dt_secs);
                return;
            }
        }
        if let Some((tx, ty)) = self.fly_to {
            // Fixed-duration nudge tween: every jump takes NUDGE_SECS,
            // regardless of distance, on a smoothstep so it starts and
            // lands without a velocity snap.
            self.fly_elapsed += dt_secs;
            let t = (self.fly_elapsed / NUDGE_SECS).clamp(0.0, 1.0);
            let e = t * t * (3.0 - 2.0 * t);
            self.x = (self.fly_from.0 + (tx - self.fly_from.0) * e).clamp(0.0, self.view_w);
            self.y = (self.fly_from.1 + (ty - self.fly_from.1) * e).clamp(0.0, self.view_h);
            self.vx = 0.0;
            self.vy = 0.0;
            if let Some((from, to)) = self.nudge_morph {
                // Direct shape change: old target rect -> new target rect,
                // no dot phase. Same smoothstep as the travel.
                let lerp = |a: f32, b: f32| a + (b - a) * e;
                self.last_dock = Some((
                    lerp(from.0, to.0),
                    lerp(from.1, to.1),
                    lerp(from.2, to.2),
                    lerp(from.3, to.3),
                ));
                self.dock_blend = 1.0;
            }
            if t >= 1.0 {
                self.fly_to = None;
                self.nudge_morph = None;
            }
        } else {
            // R2 turbo doubles the effective speed. The friction model
            // settles at accel/friction, so scale acceleration too — the
            // cap alone would never be reached.
            let (accel, max_speed) = if self.turbo {
                (CURSOR_ACCEL * 2.0, CURSOR_MAX_SPEED * 2.0)
            } else {
                (CURSOR_ACCEL, CURSOR_MAX_SPEED)
            };
            self.vx = (self.vx + lx * accel * dt_secs).clamp(-max_speed, max_speed);
            self.vy = (self.vy + ly * accel * dt_secs).clamp(-max_speed, max_speed);
        }
        let damp = (-CURSOR_FRICTION * dt_secs).exp();
        self.vx *= damp;
        self.vy *= damp;
        if self.vx.abs() < 1.0 {
            self.vx = 0.0;
        }
        if self.vy.abs() < 1.0 {
            self.vy = 0.0;
        }
        if self.vx != 0.0 || self.vy != 0.0 {
            self.x = (self.x + self.vx * dt_secs).clamp(0.0, self.view_w);
            // Analog movement stays inside the page surface.
            self.y = (self.y + self.vy * dt_secs).clamp(0.0, self.view_h);
        }
        // Docking is arrival-based (see `snap`): the outline only engages
        // once the gliding point actually reaches an element. The morph
        // state eases toward the attachment every tick — including neutral
        // ones — so shape, size, and position always interpolate.
        if self.magnet && !scrolling && !self.turbo {
            let stick = lx != 0.0 || ly != 0.0;
            // Neutral ticks keep the point fixed (content scrolling under
            // it). A deflected stick gets the magnetic centre snap, except
            // right after a break-free push (the no_dock suppression is
            // meant to prevent instant re-capture, and overlapping targets
            // must not alternate) or right after a scroll (first re-dock in
            // place, no centre snap).
            let suppression = self.no_dock.is_some() && stick;
            if !suppression {
                // Release edge: snap in place to whatever is under the
                // cursor, never centre-nudge the point after turbo.
                self.snap_with(options, stick && !self.scroll_released && !turbo_end);
            }
        }
        self.ease_blend(dt_secs);
    }

    /// Ease the outline morph toward the attachment (1 = docked, 0 = free).
    fn ease_blend(&mut self, dt_secs: f32) {
        if self.nudge_morph.is_some() {
            self.dock_blend = 1.0;
            return;
        }
        let target = if self.attached.is_some() { 1.0 } else { 0.0 };
        let step = DOCK_BLEND_PER_SEC * dt_secs;
        if self.dock_blend < target {
            self.dock_blend = (self.dock_blend + step).min(1.0);
        } else if self.dock_blend > target {
            self.dock_blend = (self.dock_blend - step).max(0.0);
        }
    }

    /// Directional jump to the nearest *other* element roughly in `dir`.
    /// Returns true on a jump; with nothing that way it returns false and
    /// the point stays put. The point flies to the target and docks on
    /// arrival — no teleport frames. D-pad always snaps — this ignores the
    /// magnet toggle.
    ///
    /// Docked ranking is band-first, not centre-distance. Targets sharing
    /// the docked item's perpendicular extent form the eligible band when
    /// any exist, so a below-right target can never win a Right press over
    /// a right target. Inside the band the nearest forward row wins first,
    /// then the widest shared perpendicular extent, then the smallest
    /// column offset. With no dock (or nothing sharing the band) it falls
    /// back to the 60-degree cone and nearest-point scoring. Targets must
    /// intersect the viewport.
    pub fn jump(&mut self, dir: JumpDir, options: &[Focusable]) -> bool {
        self.jump_with(dir, options, false)
    }

    /// Horizontal jump constrained to the current row. The eligible set is
    /// only targets sharing the attached rect's vertical band (or, with a
    /// free point, targets containing its y). There is deliberately no
    /// cone fallback: a row end is a no-op, never a diagonal hop into
    /// another row. The dot keeps its row y on arrival (clamped into the
    /// destination rect), so a tall masonry tile morphs into shape without
    /// yanking the point to its centre.
    pub fn jump_row(&mut self, dir: JumpDir, options: &[Focusable]) -> bool {
        debug_assert!(matches!(dir, JumpDir::Left | JumpDir::Right));
        let from = self.attached_rect(options);
        let row: Vec<Focusable> = options
            .iter()
            .filter(|o| match from {
                Some((_, y, _, h)) => o.y < y + h && y < o.y + o.h,
                None => o.y <= self.y && self.y <= o.y + o.h,
            })
            .cloned()
            .collect();
        if row.is_empty() {
            return false;
        }
        self.jump_with(dir, &row, true)
    }

    fn jump_with(&mut self, dir: JumpDir, options: &[Focusable], keep_row_y: bool) -> bool {
        let (dx, dy) = match dir {
            JumpDir::Up => (0.0, -1.0),
            JumpDir::Down => (0.0, 1.0),
            JumpDir::Left => (-1.0, 0.0),
            JumpDir::Right => (1.0, 0.0),
        };
        struct Candidate<'a> {
            item: &'a Focusable,
            along: f32,
            across: f32,
            /// Forward gap between the docked rect and the candidate.
            gap: f32,
            /// Shared perpendicular extent with the docked rect.
            shared: f32,
            /// Perpendicular centre offset (last-resort tie-break).
            column: f32,
        }
        let from = self.attached_rect(options);
        let mut cands: Vec<Candidate<'_>> = Vec::new();
        for item in options {
            if Some(item.id) == self.attached {
                continue;
            }
            if !self.within_viewport(item) {
                continue;
            }
            // Nearest-point direction test: the candidate must lie forward
            // of the point, and out-of-band targets must sit inside the
            // 60-degree cone.
            let nx = self.x.clamp(item.x, item.x + item.w);
            let ny = self.y.clamp(item.y, item.y + item.h);
            let cx = nx - self.x;
            let cy = ny - self.y;
            let along = cx * dx + cy * dy;
            if along <= 0.0 {
                continue;
            }
            let across = (cx * dy - cy * dx).abs();
            let (gap, shared, column) = match from {
                Some(f) => band_metrics(f, item, dir),
                None => (along, 0.0, across),
            };
            // A candidate sharing the docked band is a legitimate row move
            // even off-axis; only diagonal (out-of-band) targets are coned.
            if shared <= 0.0 && across > along {
                continue;
            }
            cands.push(Candidate {
                item,
                along,
                across,
                gap,
                shared,
                column,
            });
        }
        let any_shared = cands.iter().any(|c| c.shared > 0.0);
        let winner = if any_shared {
            cands.iter().filter(|c| c.shared > 0.0).min_by(|a, b| {
                // Nearest forward row first (a wide bar two rows away
                // must not beat the very next row); shared extent then
                // decides inside that row.
                a.gap
                    .partial_cmp(&b.gap)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| {
                        b.shared
                            .partial_cmp(&a.shared)
                            .unwrap_or(std::cmp::Ordering::Equal)
                    })
                    .then_with(|| {
                        a.column
                            .partial_cmp(&b.column)
                            .unwrap_or(std::cmp::Ordering::Equal)
                    })
            })
        } else {
            cands.iter().min_by(|a, b| {
                let sa = a.along + a.across * 2.0;
                let sb = b.along + b.across * 2.0;
                sa.partial_cmp(&sb).unwrap_or(std::cmp::Ordering::Equal)
            })
        };
        if let Some(c) = winner {
            self.jump_to_with(c.item, keep_row_y);
            true
        } else {
            false
        }
    }

    /// Fly straight to a known target, bypassing directional scoring.
    pub fn jump_to(&mut self, item: &Focusable) {
        self.jump_to_with(item, false);
    }

    fn jump_to_with(&mut self, item: &Focusable, keep_row_y: bool) {
        // Capture the departure dock before clearing it: a jump from a
        // docked target morphs wrap->wrap directly.
        let departure = if self.attached.is_some() {
            self.last_dock
        } else {
            None
        };
        self.attached = None;
        self.vx = 0.0;
        self.vy = 0.0;
        self.scroll_released = false;
        let cy = item.y + item.h / 2.0;
        let y = if keep_row_y {
            // Horizontal nudge: keep the dot on its row, clamped into the
            // destination rect so an edge-overlap still docks. A tall tile
            // must not pull the point to its centre.
            let y = self.y.clamp(item.y, item.y + item.h);
            y.clamp(0.0, self.view_h)
        } else {
            cy.clamp(0.0, self.view_h)
        };
        self.fly_from = (self.x, self.y);
        self.fly_elapsed = 0.0;
        let target_rect = (item.x, item.y, item.w, item.h);
        self.nudge_morph = if self.turbo {
            None
        } else {
            departure.map(|from| (from, target_rect))
        };
        if self.nudge_morph.is_some() {
            self.dock_blend = 1.0;
        }
        self.fly_to = Some(((item.x + item.w / 2.0).clamp(0.0, self.view_w), y));
    }

    /// Snap and centre: the fresh-acquisition entry point used by jumps,
    /// explicit magnet calls, and tests.
    pub fn snap(&mut self, options: &[Focusable]) {
        self.snap_with(options, true);
    }

    /// Dock when the point actually arrives: attach to the element whose
    /// rect (expanded by [`ACQUIRE_PAD`]) contains the point, and center
    /// the point on it — clicks and push-offs start from the middle, not
    /// the arrival edge. Holding is stickier than acquiring ([`HOLD_PAD`])
    /// so edge dither never flickers the outline.
    ///
    /// `centre` moves the point to the target's middle (fresh stick or
    /// jump acquisition). `false` keeps the point exactly where it is —
    /// used on neutral ticks so page scrolling under a docked cursor never
    /// drags its viewport position.
    pub fn snap_with(&mut self, options: &[Focusable], centre: bool) {
        // Mid-flight the point belongs to the flight path, not to whatever
        // it passes over — docking happens on arrival, once per trip.
        if self.fly_to.is_some() {
            self.attached = None;
            return;
        }
        // Break-free suppression lifts once the point leaves the hold pad.
        if let Some(sup) = self.no_dock
            && !options
                .iter()
                .find(|o| o.id == sup)
                .is_some_and(|item| self.inside_expanded(item, HOLD_PAD))
        {
            self.no_dock = None;
        }
        if let Some(id) = self.attached
            && let Some(item) = options.iter().find(|o| o.id == id)
            && self.within_viewport(item)
            && self.inside_expanded(item, HOLD_PAD)
        {
            return;
        }
        self.attached = None;
        let mut best: Option<&Focusable> = None;
        let mut best_d = f32::INFINITY;
        for item in options {
            if !self.within_viewport(item) {
                continue;
            }
            if !self.inside_expanded(item, ACQUIRE_PAD) {
                continue;
            }
            // Suppressed (just pushed off): no instant re-capture.
            if Some(item.id) == self.no_dock {
                continue;
            }
            let d = (item.x + item.w / 2.0 - self.x).hypot(item.y + item.h / 2.0 - self.y);
            if d < best_d {
                best_d = d;
                best = Some(item);
            }
        }
        self.attached = best.map(|item| item.id);
        // New dock, instant wrap: the morph eases dot→wrap, which swooshes
        // through wrong sizes on big targets (bookmark bar to page link).
        // The flight already animated the motion; the outline cuts in.
        // Undocking keeps the morph (wrap collapses back into the dot).
        if best.is_some() {
            self.dock_blend = 1.0;
        }
        // Center on dock: the point moves to the middle of the target, and
        // any arrival velocity dies here — a dock starts from rest, and the
        // break latch engages so the arriving (still-held) stick cannot
        // instantly kick back off. Release below BREAK_REARM_DEFLECT to
        // re-arm, then push to leave.
        if let Some(item) = best {
            self.vx = 0.0;
            self.vy = 0.0;
            self.break_armed = false;
            self.docked_for = 0.0;
            self.scroll_released = false;
            if centre {
                self.x = (item.x + item.w / 2.0).clamp(0.0, self.view_w);
                // Centre snap is a stick acquire: keep it in the same zone,
                // even if the target's centre reaches across the boundary.
                let cy = item.y + item.h / 2.0;
                self.y = self.zone_y(cy);
            }
            self.last_dock = Some((item.x, item.y, item.w, item.h));
        }
    }

    /// Re-resolve the attachment after a fresh tree (ids may have shifted;
    /// a missing id detaches). Call on every `A11yTree` update.
    pub fn retarget(&mut self, options: &[Focusable]) {
        if let Some(id) = self.attached
            && !options.iter().any(|o| o.id == id)
        {
            self.attached = None;
        }
    }

    /// Point to click: the flight target mid-flight, attached element
    /// center when docked, else the free cursor. No gravity fallback: with
    /// no pull, a free point far from any element means exactly that (the
    /// old well fallback once clicked a field center 47px from the free
    /// cursor — R3 flag #6).
    pub fn click_point(&self, options: &[Focusable]) -> (f32, f32) {
        // Mid-flight: aim at the flight target (user intent).
        if let Some((tx, ty)) = self.fly_to {
            return (tx, ty);
        }
        if let Some(id) = self.attached
            && let Some(item) = options.iter().find(|o| o.id == id)
        {
            let (cx, cy) = (item.x + item.w / 2.0, item.y + item.h / 2.0);
            // Page clicks must land inside the viewport: a partially
            // visible target's centre can sit below it (observed click at
            // y=718 for a 600px viewport). The clamped centre stays within
            // the item's visible intersection.
            return (cx.clamp(0.0, self.view_w), cy.clamp(0.0, self.view_h));
        }
        (self.x, self.y)
    }

    /// Attached element's rect, if the attachment still resolves.
    pub fn attached_rect(&self, options: &[Focusable]) -> Option<(f32, f32, f32, f32)> {
        self.attached.and_then(|id| {
            options
                .iter()
                .find(|o| o.id == id)
                .map(|o| (o.x, o.y, o.w, o.h))
        })
    }

    fn inside_expanded(&self, item: &Focusable, pad: f32) -> bool {
        self.x >= item.x - pad
            && self.x <= item.x + item.w + pad
            && self.y >= item.y - pad
            && self.y <= item.y + item.h + pad
    }

    /// Rect-boundary point where a push from the center along `(lx, ly)`
    /// leaves the element: axis pushes exit mid-edge, diagonals exit
    /// corners. Ray from the center; the first rect edge it hits wins.
    fn exit_point(item: &Focusable, lx: f32, ly: f32) -> (f32, f32) {
        let cx = item.x + item.w / 2.0;
        let cy = item.y + item.h / 2.0;
        let len = lx.hypot(ly);
        if len <= 0.0 {
            return (cx, cy);
        }
        let (dx, dy) = (lx / len, ly / len);
        let mut t = f32::INFINITY;
        if dx > 0.0 {
            t = t.min((item.x + item.w - cx) / dx);
        } else if dx < 0.0 {
            t = t.min((item.x - cx) / dx);
        }
        if dy > 0.0 {
            t = t.min((item.y + item.h - cy) / dy);
        } else if dy < 0.0 {
            t = t.min((item.y - cy) / dy);
        }
        if !t.is_finite() {
            return (cx, cy);
        }
        (cx + dx * t, cy + dy * t)
    }
}

/// Dominant right-stick axis for one scroll gesture. Chosen when the stick
/// leaves neutral and held until it returns, so a diagonal hold cannot drift
/// sideways while the user scrolls vertically (or vice versa).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollAxis {
    Vertical,
    Horizontal,
}

impl ScrollAxis {
    fn other(self) -> Self {
        match self {
            Self::Vertical => Self::Horizontal,
            Self::Horizontal => Self::Vertical,
        }
    }
}

/// One cursor advance's engine-facing output.
#[derive(Debug, Default)]
pub struct CursorFrame {
    /// Quantized pointer position that changed this frame.
    pub mouse: Option<(i32, i32)>,
    /// Right-stick scroll delta and page point.
    pub scroll: Option<(f32, f32, (f32, f32))>,
    /// A D-pad flight landed; ask the engine for a fresh DOM cull.
    pub reseed: bool,
}

/// Shared cursor state for the live tab. The pull-model widget advances it at
/// draw/update time; the app only reads snapshots for discrete actions.
#[derive(Debug)]
pub struct CursorRuntime {
    pub cursor: Cursor,
    pub focusables: Vec<Focusable>,
    pub analog_at: Option<Instant>,
    pub scroll_axis: Option<ScrollAxis>,
    pub last_sent_mouse: Option<(i32, i32)>,
    pub last_stick: (f32, f32),
    pub force_mouse_sync: bool,
    pub dom_reseed_pending: bool,
    pub last_dock_change: Option<Instant>,
}

impl Default for CursorRuntime {
    fn default() -> Self {
        Self {
            cursor: Cursor::new(),
            focusables: Vec::new(),
            analog_at: None,
            scroll_axis: None,
            last_sent_mouse: None,
            last_stick: (0.0, 0.0),
            force_mouse_sync: false,
            dom_reseed_pending: false,
            last_dock_change: None,
        }
    }
}

impl CursorRuntime {
    /// Integrate one latest pad sample. Includes neutral frames so friction,
    /// flights, and dock morphs settle even when the page pixels are static.
    pub fn advance(&mut self, sample: &PadSample, now: Instant) -> CursorFrame {
        use crate::browser::cursor::SCROLL_SPEED;
        let dt = self
            .analog_at
            .map(|t| now.saturating_duration_since(t).as_secs_f32().min(0.1))
            .unwrap_or(0.0);
        self.analog_at = Some(now);
        self.cursor.turbo = sample.r2;
        self.last_stick = (sample.lstick_x, sample.lstick_y);

        let before = self.cursor.attached;
        let (bx, by) = (self.cursor.x, self.cursor.y);
        let scrolling = sample.rstick_x != 0.0 || sample.rstick_y != 0.0;
        let cursor = &mut self.cursor;
        cursor.advance_with(
            sample.lstick_x,
            sample.lstick_y,
            dt,
            &self.focusables,
            scrolling,
        );
        if before != self.cursor.attached {
            self.last_dock_change = Some(now);
            let describe = |id: Option<u64>| match id {
                None => "free".to_string(),
                Some(i) => match self.focusables.iter().find(|o| o.id == i) {
                    Some(o) => format!(
                        "#{i} {:?} ({:.0},{:.0},{:.0}x{:.0})",
                        o.kind, o.x, o.y, o.w, o.h
                    ),
                    None => format!("#{i} gone"),
                },
            };
            crate::platform::app_log::info(format!(
                "cursor dock {} -> {} stick=({:.2},{:.2}) rstick=({:.2},{:.2}) scrolling={} pos=({bx:.0},{by:.0})",
                describe(before),
                describe(self.cursor.attached),
                sample.lstick_x,
                sample.lstick_y,
                sample.rstick_x,
                sample.rstick_y,
                u8::from(scrolling),
            ));
        }

        let axis = match self.scroll_axis {
            Some(_) if sample.rstick_x == 0.0 && sample.rstick_y == 0.0 => {
                self.scroll_axis = None;
                None
            }
            Some(axis) => {
                let (locked, other) = match axis {
                    ScrollAxis::Vertical => (sample.rstick_y, sample.rstick_x),
                    ScrollAxis::Horizontal => (sample.rstick_x, sample.rstick_y),
                };
                if locked == 0.0 && other != 0.0 {
                    let next = axis.other();
                    self.scroll_axis = Some(next);
                    Some(next)
                } else {
                    Some(axis)
                }
            }
            None if sample.rstick_x == 0.0 && sample.rstick_y == 0.0 => None,
            None => {
                let axis = if sample.rstick_y.abs() >= sample.rstick_x.abs() {
                    ScrollAxis::Vertical
                } else {
                    ScrollAxis::Horizontal
                };
                self.scroll_axis = Some(axis);
                Some(axis)
            }
        };
        let scale = if sample.r2 { 2.0 } else { 1.0 };
        let scroll = match axis {
            Some(ScrollAxis::Vertical) => {
                let dy = sample.rstick_y * SCROLL_SPEED * dt * scale;
                (dy != 0.0).then_some((0.0, dy, (self.cursor.x, self.cursor.y)))
            }
            Some(ScrollAxis::Horizontal) => {
                let dx = sample.rstick_x * SCROLL_SPEED * dt * scale;
                (dx != 0.0).then_some((dx, 0.0, (self.cursor.x, self.cursor.y)))
            }
            None => None,
        };

        let (qx, qy) = (self.cursor.x as i32, self.cursor.y.max(0.0) as i32);
        let mouse = if self.force_mouse_sync || self.last_sent_mouse != Some((qx, qy)) {
            self.force_mouse_sync = false;
            self.last_sent_mouse = Some((qx, qy));
            Some((qx, qy))
        } else {
            None
        };

        let reseed = self.dom_reseed_pending && self.cursor.fly_to.is_none();
        if reseed {
            self.dom_reseed_pending = false;
        }

        CursorFrame {
            mouse,
            scroll,
            reseed,
        }
    }
}

/// Shared handle to one tab's cursor runtime. Clones share the same state;
/// dormant tabs each own their own handle.
#[derive(Debug, Clone, Default)]
pub struct CursorHandle(Arc<Mutex<CursorRuntime>>);

impl CursorHandle {
    pub fn lock(&self) -> MutexGuard<'_, CursorRuntime> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn snapshot(&self) -> Cursor {
        self.lock().cursor.clone()
    }

    pub fn set_viewport(&self, w: f32, h: f32) {
        self.lock().cursor.set_viewport(w, h);
    }

    pub fn reset_for_page(&self) {
        let mut rt = self.lock();
        rt.cursor.reset_for_page();
        rt.focusables.clear();
        rt.analog_at = None;
        rt.scroll_axis = None;
        rt.last_sent_mouse = None;
        rt.last_stick = (0.0, 0.0);
        rt.force_mouse_sync = true;
        rt.dom_reseed_pending = false;
        rt.last_dock_change = None;
    }

    pub fn reset_analog(&self) {
        let mut rt = self.lock();
        rt.analog_at = None;
        rt.scroll_axis = None;
        rt.last_stick = (0.0, 0.0);
    }

    pub fn set_focusables(&self, focusables: Vec<Focusable>) {
        self.lock().focusables = focusables;
    }

    pub fn retarget(&self, focusables: &[Focusable]) {
        let mut rt = self.lock();
        rt.focusables = focusables.to_vec();
        rt.cursor.retarget(focusables);
    }

    pub fn jump(&self, dir: JumpDir) -> bool {
        let mut rt = self.lock();
        let was = rt.cursor.attached;
        let targets = rt.focusables.clone();
        rt.cursor.jump(dir, &targets);
        rt.dom_reseed_pending = true;
        was != rt.cursor.attached || rt.cursor.fly_to.is_some()
    }

    pub fn set_magnet(&self, magnet: bool) {
        let mut rt = self.lock();
        rt.cursor.magnet = magnet;
        if !magnet {
            rt.cursor.attached = None;
            rt.cursor.no_dock = None;
        }
    }

    pub fn request_pointer_sync(&self) {
        self.lock().force_mouse_sync = true;
    }

    pub fn mark_mouse_sent(&self, position: (i32, i32)) {
        let mut rt = self.lock();
        rt.last_sent_mouse = Some(position);
        rt.force_mouse_sync = false;
    }

    pub fn last_stick(&self) -> (f32, f32) {
        self.lock().last_stick
    }

    pub fn take_dock_change(&self) -> Option<Instant> {
        self.lock().last_dock_change.take()
    }

    pub fn dock_change_at(&self) -> Option<Instant> {
        self.lock().last_dock_change
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(id: u64, x: f32, y: f32) -> Focusable {
        Focusable {
            id,
            x,
            y,
            w: 100.0,
            h: 30.0,
            kind: FocusKind::Link,
            url: None,
        }
    }

    #[test]
    fn default_cursor_is_live() {
        // Fresh page states derive their cursor: it must be centered,
        // magnetized and visible — a derived (0,0)/magnet-off default
        // silently disabled autofocus docking on new tabs.
        let c = Cursor::default();
        assert_eq!((c.x, c.y), (VIEWPORT_W / 2.0, VIEWPORT_H / 2.0));
        assert!(c.magnet);
        assert!(c.visible);
        assert!(c.attached.is_none());
        assert!(c.fly_to.is_none());
    }

    #[test]
    fn reset_for_page_keeps_live_viewport_bounds() {
        let mut c = Cursor::new();
        c.set_viewport(1734.0, 860.0);
        c.x = 10.0;
        c.y = 20.0;
        c.attached = Some(7);
        c.reset_for_page();
        assert_eq!((c.view_w, c.view_h), (1734.0, 860.0));
        assert_eq!((c.x, c.y), (867.0, 430.0));
        assert_eq!(c.attached, None);
        assert!(c.fly_to.is_none());
        assert!(c.magnet, "navigation must not reset the magnet preference");
    }

    #[test]
    fn free_cursor_clamps_to_viewport() {
        let mut c = Cursor::new();
        c.move_by(-9999.0, 9999.0);
        assert_eq!((c.x, c.y), (0.0, VIEWPORT_H));
        assert!(c.attached.is_none());
    }

    #[test]
    fn snap_attaches_nearby_and_releases_when_leaving() {
        let mut c = Cursor::new();
        let items = [link(1, 100.0, 100.0)];
        c.x = 110.0;
        c.y = 115.0;
        c.snap(&items);
        assert_eq!(c.attached, Some(1));
        // Stays attached while inside the expanded rect.
        c.x = 215.0;
        c.snap(&items);
        assert_eq!(c.attached, Some(1));
        // Leaves: detaches, no re-snap out of range.
        c.x = 500.0;
        c.y = 500.0;
        c.snap(&items);
        assert_eq!(c.attached, None);
    }

    #[test]
    fn snap_picks_nearest_on_overlap() {
        let mut c = Cursor::new();
        let items = [link(1, 100.0, 100.0), link(2, 140.0, 100.0)];
        // Inside both rects (+pad): docks the nearer center (190 vs 150).
        c.x = 180.0;
        c.y = 115.0;
        c.snap(&items);
        assert_eq!(c.attached, Some(2));
    }

    #[test]
    fn jump_flies_to_target_and_docks_on_arrival() {
        let mut c = Cursor::new();
        let items = [link(1, 100.0, 100.0), link(2, 400.0, 100.0)];
        c.x = 110.0;
        c.y = 115.0;
        c.snap(&items);
        assert_eq!(c.attached, Some(1));
        assert!(c.jump(JumpDir::Right, &items));
        // Flight, not teleport: leaves the dock, aims at the target.
        assert_eq!(c.attached, None);
        assert_eq!(c.click_point(&items), (450.0, 115.0));
        let mut last_x = c.x;
        for _ in 0..60 {
            c.advance(0.0, 0.0, 0.016, &items);
            assert!(c.x >= last_x, "flight never reverses, x={}", c.x);
            last_x = c.x;
            if c.attached.is_some() {
                break;
            }
        }
        assert_eq!(c.attached, Some(2), "docked on arrival");
        assert!((c.x - 450.0).abs() < 1.0 && (c.y - 115.0).abs() < 1.0);
        // Nothing above: the jump is a no-op that stays docked instead of
        // flying off into empty space. (Advancing against an empty tree
        // still detaches — page gone.)
        assert!(!c.jump(JumpDir::Up, &[]));
        assert_eq!(c.attached, Some(2));
        assert!((c.x - 450.0).abs() < 1.0 && (c.y - 115.0).abs() < 1.0);
    }

    #[test]
    fn jump_travel_is_a_fixed_150ms_regardless_of_distance() {
        let items = [link(2, 500.0, 100.0)];
        let mut c = Cursor::new();
        c.x = 110.0;
        c.y = 115.0;
        assert!(c.jump(JumpDir::Right, &items));
        // 9 × 16ms = 144ms: still in flight, nearly arrived.
        for _ in 0..9 {
            c.advance(0.0, 0.0, 0.016, &items);
        }
        assert!(c.fly_to.is_some(), "nudge must not finish early");
        // One 6ms tick closes the fixed 150ms beat.
        c.advance(0.0, 0.0, 0.006, &items);
        assert!(c.fly_to.is_none(), "nudge lands at 150ms");
        assert!((c.x - 550.0).abs() < 1.0 && (c.y - 115.0).abs() < 1.0);
    }

    #[test]
    fn turbo_doubles_the_cursor_speed() {
        let mut slow = Cursor::new();
        let mut fast = Cursor::new();
        slow.view_w = 10_000.0;
        fast.view_w = 10_000.0;
        slow.x = 100.0;
        fast.x = 100.0;
        fast.turbo = true;
        for _ in 0..60 {
            slow.advance(1.0, 0.0, 0.016, &[]);
            fast.advance(1.0, 0.0, 0.016, &[]);
        }
        let ratio = fast.vx.abs() / slow.vx.abs();
        assert!((ratio - 2.0).abs() < 0.05, "turbo ratio={ratio}");
        assert!(fast.x > slow.x * 1.5);
    }

    #[test]
    fn nudge_morphs_directly_between_target_shapes() {
        let items = [link(1, 100.0, 100.0), link(2, 500.0, 100.0)];
        let mut c = Cursor::new();
        c.x = 150.0;
        c.y = 115.0;
        c.snap(&items);
        assert_eq!(c.attached, Some(1));
        assert!(c.jump(JumpDir::Right, &items));
        // Mid-flight the outline stays a wrap and its rect moves from the
        // old target toward the new one: no collapse through the dot.
        for _ in 0..4 {
            c.advance(0.0, 0.0, 0.016, &items);
        }
        assert_eq!(c.dock_blend, 1.0);
        let mid = c.last_dock.expect("interpolated rect");
        assert!(mid.0 > 100.0 && mid.0 < 500.0, "mid rect={mid:?}");
        for _ in 0..10 {
            c.advance(0.0, 0.0, 0.016, &items);
            if c.attached == Some(2) {
                break;
            }
        }
        assert_eq!(c.attached, Some(2));
        assert_eq!(c.dock_blend, 1.0);
        assert_eq!(c.last_dock, Some((500.0, 100.0, 100.0, 30.0)));
    }

    #[test]
    fn turbo_disables_docking_and_resnaps_in_place_on_release() {
        let items = [link(1, 100.0, 100.0)];
        let mut c = Cursor::new();
        c.x = 150.0;
        c.y = 115.0;
        c.snap(&items);
        assert_eq!(c.attached, Some(1));
        c.turbo = true;
        c.advance(0.0, 0.0, 0.016, &items);
        assert_eq!(c.attached, None, "turbo releases the dock");
        c.turbo = false;
        c.advance(0.0, 0.0, 0.016, &items);
        assert_eq!(c.attached, Some(1), "release re-snaps under the cursor");
        assert_eq!((c.x, c.y), (150.0, 115.0), "in place, no centre nudge");
    }

    #[test]
    fn analog_stick_clamps_at_the_page_edges() {
        // The page surface starts at y=0; no browser chrome above it.
        let mut c = Cursor::new();
        c.x = 400.0;
        c.y = 20.0;
        for _ in 0..60 {
            c.advance(0.0, -1.0, 0.016, &[]);
        }
        assert_eq!(c.y, 0.0, "top clamp");
        for _ in 0..300 {
            c.advance(0.0, 1.0, 0.016, &[]);
        }
        assert_eq!(c.y, c.view_h, "bottom clamp");
    }

    #[test]
    fn horizontal_row_jump_stops_at_the_row_end() {
        // A row edge is a no-op: no cone fallback into the next row.
        let row = link(1, 100.0, 300.0);
        let off_row = link(2, 500.0, 500.0);
        let mut c = Cursor::new();
        c.x = 150.0;
        c.y = 315.0;
        c.attached = Some(1);
        assert!(!c.jump_row(JumpDir::Right, &[row, off_row]));
        assert!(c.fly_to.is_none());
        assert_eq!(c.attached, Some(1), "row end keeps the dock");
    }

    #[test]
    fn horizontal_row_jump_ignores_off_row_targets() {
        let row = link(1, 100.0, 300.0);
        let same = link(2, 300.0, 300.0);
        let off = link(3, 500.0, 100.0);
        let mut c = Cursor::new();
        c.x = 150.0;
        c.y = 315.0;
        c.attached = Some(1);
        assert!(c.jump_row(JumpDir::Right, &[row, same, off]));
        assert_eq!(c.fly_to, Some((350.0, 315.0)));
    }

    #[test]
    fn horizontal_row_jump_keeps_the_row_y_on_a_tall_tile() {
        // Masonry case: a portrait tile overlaps the row by only a few px.
        // The dot must stay on the row, not fly to the tile's centre.
        let row = link(1, 100.0, 300.0);
        let tall = Focusable {
            id: 2,
            x: 300.0,
            y: -30.0,
            w: 100.0,
            h: 360.0,
            kind: FocusKind::Link,
            url: None,
        };
        let items = [row, tall];
        let mut c = Cursor::new();
        c.x = 150.0;
        c.y = 315.0;
        c.attached = Some(1);
        assert!(c.jump_row(JumpDir::Right, &items));
        assert_eq!(c.fly_to, Some((350.0, 315.0)), "row y preserved");
        for _ in 0..60 {
            c.advance(0.0, 0.0, 0.016, &items);
        }
        assert_eq!(c.attached, Some(2));
        assert_eq!(c.y, 315.0, "no centre pull");
    }

    #[test]
    fn free_horizontal_row_jump_uses_the_point_row() {
        let same = link(1, 300.0, 300.0);
        let off = link(2, 500.0, 100.0);
        let mut c = Cursor::new();
        c.x = 150.0;
        c.y = 315.0;
        assert!(c.jump_row(JumpDir::Right, &[same, off.clone()]));
        assert_eq!(c.fly_to, Some((350.0, 315.0)));
        // No target containing the point's row: no-op, not a diagonal hop.
        let mut c2 = Cursor::new();
        c2.x = 150.0;
        c2.y = 315.0;
        assert!(!c2.jump_row(JumpDir::Right, &[off]));
        assert!(c2.fly_to.is_none());
    }

    #[test]
    fn stick_input_cancels_flight() {
        let mut c = Cursor::new();
        let items = [link(2, 400.0, 100.0)];
        c.x = 110.0;
        c.y = 115.0;
        assert!(c.jump(JumpDir::Right, &items));
        assert!(c.fly_to.is_some());
        c.advance(1.0, 0.0, 0.016, &items);
        assert_eq!(c.fly_to, None, "stick takes back control");
        assert!(c.x > 110.0);
    }

    #[test]
    fn dock_blend_morphs_both_directions() {
        let mut c = Cursor::new();
        let items = [link(1, 100.0, 100.0)];
        // Dock inside the pad: blend eases 0 → 1 over ticks, never jumps.
        c.x = 205.0;
        c.y = 115.0;
        c.advance(0.0, 0.0, 0.016, &items);
        assert_eq!(c.attached, Some(1));
        // Neutral docking keeps the point where it already is.
        assert_eq!((c.x, c.y), (205.0, 115.0));
        let mut prev = c.dock_blend;
        for _ in 0..60 {
            c.advance(0.0, 0.0, 0.016, &items);
            assert!(c.dock_blend >= prev, "morph never reverses");
            prev = c.dock_blend;
            if c.dock_blend >= 1.0 {
                break;
            }
        }
        assert_eq!(c.attached, Some(1));
        assert_eq!(c.dock_blend, 1.0);
        assert_eq!(c.last_dock, Some((100.0, 100.0, 100.0, 30.0)));
        // Undock: blend eases back out from the remembered rect.
        c.x = 500.0;
        c.y = 500.0;
        c.advance(0.0, 0.0, 0.016, &items);
        assert_eq!(c.attached, None);
        assert!(c.dock_blend < 1.0, "blend={}", c.dock_blend);
        for _ in 0..60 {
            c.advance(0.0, 0.0, 0.016, &items);
            if c.dock_blend <= 0.0 {
                break;
            }
        }
        assert_eq!(c.dock_blend, 0.0);
    }

    #[test]
    fn diagonal_stick_moves_both_axes_proportionally() {
        let mut c = Cursor::new();
        c.advance(0.5, -0.5, 0.1, &[]);
        assert!(c.x > 400.0, "right, x={}", c.x);
        assert!(c.y < 300.0, "up, y={}", c.y);
        // Roughly equal magnitude: no axis snapping.
        let dx = c.x - 400.0;
        let dy = 300.0 - c.y;
        assert!(
            (dx - dy).abs() < dx * 0.2,
            "diagonal should stay diagonal: dx={dx} dy={dy}"
        );
    }

    #[test]
    fn velocity_builds_and_friction_settles() {
        let mut c = Cursor::new();
        c.advance(1.0, 0.0, 0.05, &[]);
        let v1 = c.vx;
        assert!(v1 > 0.0);
        c.advance(1.0, 0.0, 0.05, &[]);
        assert!(c.vx > v1, "holding the stick keeps accelerating");
        // Release: friction bleeds velocity without input.
        let mut last_x = c.x;
        for _ in 0..60 {
            c.advance(0.0, 0.0, 0.016, &[]);
            assert!(c.x >= last_x, "glide never reverses");
            last_x = c.x;
        }
        assert_eq!(c.vx, 0.0, "friction must fully settle");
    }

    #[test]
    fn neutral_ticks_decay_velocity_so_reversal_is_immediate() {
        // Regression: velocity used to freeze on neutral ticks (the UI
        // skipped integration), so pushing left after releasing right kept
        // drifting right until the stale velocity bled off.
        let mut c = Cursor::new();
        c.magnet = false;
        for _ in 0..10 {
            c.advance(1.0, 0.0, 0.016, &[]);
        }
        assert!(c.vx > 0.0);
        let released_x = c.x;
        // Neutral ticks: friction decays to a stop; glide carries forward
        // while decaying, never reverses.
        for _ in 0..120 {
            c.advance(0.0, 0.0, 0.016, &[]);
            assert!(c.x >= released_x - 0.001);
        }
        assert_eq!(c.vx, 0.0, "friction must fully settle");
        let settled_x = c.x;
        assert!(
            settled_x > released_x,
            "glide carries forward while decaying"
        );
        // Opposite push moves the correct way on the first tick.
        c.advance(-1.0, 0.0, 0.016, &[]);
        assert!(c.x < settled_x, "x={} settled={}", c.x, settled_x);
    }

    #[test]
    fn advance_respects_magnet_flag() {
        let mut c = Cursor::new();
        c.magnet = false;
        let items = [link(1, 100.0, 100.0)];
        c.x = 110.0;
        c.y = 115.0;
        c.advance(0.5, 0.0, 0.016, &items);
        assert_eq!(c.attached, None);
        assert!(c.x > 110.0);
    }

    #[test]
    fn dock_engages_on_arrival_and_click_stays_free_until_then() {
        // 10px outside the rect (+8px pad): no outline, and the click is
        // the free point — there is no gravity well to fall into or yank
        // toward.
        let mut c = Cursor::new();
        let items = [link(1, 100.0, 100.0)];
        c.x = 210.0;
        c.y = 115.0;
        c.advance(0.0, 0.0, 0.016, &items);
        assert_eq!(c.attached, None, "outline pops on arrival, not on entry");
        assert_eq!(c.click_point(&items), (210.0, 115.0));
        // Drive into the pad: docks in place (no viewport jump).
        c.x = 205.0;
        c.advance(0.0, 0.0, 0.016, &items);
        assert_eq!(c.attached, Some(1));
        assert_eq!((c.x, c.y), (205.0, 115.0));
    }

    #[test]
    fn jump_flies_to_top_page_target_and_docks() {
        // D-pad Up from a lower target lands on the target above it.
        let mut c = Cursor::new();
        let top = link(1, 300.0, 10.0);
        let bottom = link(2, 300.0, 100.0);
        let items = [top, bottom];
        c.x = 350.0;
        c.y = 115.0;
        c.advance(0.0, 0.0, 0.016, &items);
        assert_eq!(c.attached, Some(2));
        assert!(c.jump(JumpDir::Up, &items));
        // Fly until arrival (jump glides, docks on arrival).
        for _ in 0..60 {
            c.advance(0.0, 0.0, 0.016, &items);
            if c.attached == Some(1) {
                break;
            }
        }
        assert_eq!(c.attached, Some(1));
        assert!(c.y >= 0.0);
    }

    #[test]
    fn docked_jump_stays_in_the_docked_band() {
        // Direction is the gate; shared row extent decides eligibility
        // before any distance scoring.
        let mut c = Cursor::new();
        let omni = Focusable {
            id: 1,
            x: 228.0,
            y: 100.0,
            w: 456.0,
            h: 36.0,
            kind: FocusKind::Button,
            url: None,
        };
        let reload = Focusable {
            id: 2,
            x: 114.0,
            y: 100.0,
            w: 57.0,
            h: 36.0,
            kind: FocusKind::Button,
            url: None,
        };
        let strip = Focusable {
            id: 3,
            x: 246.0,
            y: 50.0,
            w: 185.0,
            h: 36.0,
            kind: FocusKind::Button,
            url: None,
        };
        let items = [omni, reload, strip];
        c.x = 456.0;
        c.y = 118.0;
        c.advance(0.0, 0.0, 0.016, &items);
        assert_eq!(c.attached, Some(1));
        assert!(c.jump(JumpDir::Left, &items));
        for _ in 0..60 {
            c.advance(0.0, 0.0, 0.016, &items);
            if c.attached == Some(2) {
                break;
            }
        }
        assert_eq!(c.attached, Some(2));
    }

    #[test]
    fn vertical_jump_ranks_shared_width_within_the_row() {
        // Two content rows: a(200) b(100) over c(150) d(80). Up from
        // either row-2 slot must pick a: it shares 150px with c and 50px
        // with d, while b shares 0 and 30. Center distance would pick b
        // from d; shared extent picks a.
        let a = page_btn(1, 0.0, 100.0, 200.0, 40.0);
        let b = page_btn(2, 200.0, 100.0, 100.0, 40.0);
        let c = page_btn(3, 0.0, 200.0, 150.0, 40.0);
        let d = page_btn(4, 150.0, 200.0, 80.0, 40.0);
        let items = [a, b, c, d];
        for (from, x) in [(3u64, 75.0f32), (4, 190.0)] {
            let mut cur = Cursor::new();
            cur.attached = Some(from);
            cur.x = x;
            cur.y = 220.0;
            assert!(
                cur.jump(JumpDir::Up, &items),
                "Up from item {from} must find a target"
            );
            assert_eq!(
                cur.fly_to,
                Some((100.0, 120.0)),
                "Up from {from} lands on a (shared width), not b"
            );
        }
    }

    #[test]
    fn right_never_picks_below_right_over_right() {
        // The below-right target is nearer in x and would win any
        // distance/centre score, but shares none of the docked row's
        // vertical extent. Direction gates it out entirely.
        let current = page_btn(1, 100.0, 100.0, 100.0, 100.0);
        let right = page_btn(2, 250.0, 100.0, 100.0, 50.0);
        let below_right = page_btn(3, 220.0, 220.0, 100.0, 50.0);
        let items = [current, right, below_right];
        let mut c = Cursor::new();
        c.attached = Some(1);
        c.x = 150.0;
        c.y = 150.0;
        assert!(c.jump(JumpDir::Right, &items));
        assert_eq!(
            c.fly_to,
            Some((300.0, 125.0)),
            "must land on the right target, never below-right"
        );
    }

    #[test]
    fn near_page_logo_beats_a_wide_far_target() {
        // R3 trace: Up from a content tile skipped the brand logo and
        // jumped to a wide banner. The logo shares the tile's band and is
        // nearer; the wide banner is farther forward. Nearest row wins
        // inside the band.
        let logo = page_btn(1, 8.0, 12.0, 180.0, 26.0);
        let tile = page_btn(6, 141.0, 50.0, 141.0, 36.0);
        let banner = page_btn(7, 0.0, 0.0, 400.0, 18.0);
        let items = [logo, tile, banner];
        let mut c = Cursor::new();
        c.attached = Some(6);
        c.x = 211.5;
        c.y = 68.0;
        assert!(c.jump(JumpDir::Up, &items));
        assert_eq!(
            c.fly_to,
            Some((98.0, 25.0)),
            "the brand logo directly above wins, not the wide banner"
        );
    }

    #[test]
    fn nearest_forward_row_beats_a_wide_far_target() {
        // R3 trace: Down from a top strip docked a wide banner far below
        // instead of the nearer link. The nearer row must win before width.
        let top = page_btn(9, 0.0, 0.0, 400.0, 20.0);
        let near = link(1, 141.0, 50.0);
        let far = link(2, 14.0, 188.0);
        let items = [top, near, far];
        let mut c = Cursor::new();
        c.attached = Some(9);
        c.x = 200.0;
        c.y = 10.0;
        assert!(c.jump(JumpDir::Down, &items));
        assert_eq!(
            c.fly_to,
            Some((191.0, 65.0)),
            "nearest row (y=50) wins, not the wide banner at y=188"
        );
    }

    #[test]
    fn offscreen_page_target_is_not_a_jump_target() {
        // R3 trace: Down from the last visible link flew to a link at
        // y=607 (entirely below the 600px viewport), pinning the dot to
        // the bottom edge with nothing under it.
        let last = link(1, 16.0, 552.0);
        let below = link(2, 16.0, 607.0);
        let items = [last, below];
        let mut c = Cursor::new();
        c.attached = Some(1);
        c.x = 66.0;
        c.y = 567.0;
        assert!(!c.jump(JumpDir::Down, &items));
        assert!(c.fly_to.is_none());
    }

    #[test]
    fn snap_ignores_offscreen_page_targets() {
        let below = link(2, 16.0, 607.0);
        let items = [below];
        let mut c = Cursor::new();
        c.x = 66.0;
        c.y = 600.0;
        c.snap(&items);
        assert_eq!(c.attached, None);
    }

    #[test]
    fn click_point_stays_inside_viewport_for_partial_target() {
        // R3 trace: docked on a 480x270 link whose centre is y=718 in a
        // 600px viewport; the click must clamp to the visible edge area.
        let tall = page_btn(2, 420.0, 583.0, 480.0, 270.0);
        let items = [tall];
        let mut c = Cursor::new();
        c.attached = Some(2);
        c.x = 660.0;
        c.y = 600.0;
        assert_eq!(c.click_point(&items), (660.0, 600.0));
    }

    fn page_btn(id: u64, x: f32, y: f32, w: f32, h: f32) -> Focusable {
        Focusable {
            id,
            x,
            y,
            w,
            h,
            kind: FocusKind::Button,
            url: None,
        }
    }

    #[test]
    fn tiny_off_axis_button_does_not_intercept() {
        // Trace case: Right from a page link hopped through a tiny 13x15
        // button instead of the wide same-row neighbor. Rect-nearest
        // scoring fixes the interception.
        let mut c = Cursor::new();
        let tiny = Focusable {
            id: 2,
            x: 674.0,
            y: 23.0,
            w: 13.0,
            h: 15.0,
            kind: FocusKind::Link,
            url: None,
        };
        let wide = link(3, 707.0, 50.0);
        let items = [tiny, wide];
        c.x = 636.0;
        c.y = 68.0;
        c.advance(0.0, 0.0, 0.016, &items);
        assert!(c.jump(JumpDir::Right, &items));
        for _ in 0..60 {
            c.advance(0.0, 0.0, 0.016, &items);
            if c.attached == Some(3) {
                break;
            }
        }
        assert_eq!(c.attached, Some(3));
    }

    #[test]
    fn jump_into_void_stays_put() {
        // D-pad past the top element is a no-op: jump-only movement stays
        // exactly put.
        let mut c = Cursor::new();
        let top = link(1, 200.0, 0.0);
        let items = [top];
        c.x = 250.0;
        c.y = 15.0;
        c.advance(0.0, 0.0, 0.016, &items);
        assert_eq!(c.attached, Some(1));
        assert!(!c.jump(JumpDir::Up, &items));
        assert_eq!(c.attached, Some(1));
        // The blocked press moves nothing; the point keeps its position.
        assert_eq!((c.x, c.y), (250.0, 15.0));
        assert!(c.fly_to.is_none());
    }

    #[test]
    fn docked_last_link_does_not_nudge_off_bottom() {
        // Observed: docked on the last visible link, Down nudged the dot
        // 32px into the viewport's bottom edge and off the element. The
        // attached link's center sits a fraction below the point, which is
        // what used to authorize the nudge.
        let mut c = Cursor::new();
        let last = link(61, 115.0, 554.0);
        let items = [last];
        c.x = 165.0;
        c.y = 568.0;
        c.advance(0.0, 0.0, 0.016, &items);
        assert_eq!(c.attached, Some(61));
        assert!(!c.jump(JumpDir::Down, &items));
        assert_eq!(c.attached, Some(61), "still docked");
        assert_eq!(c.y, 568.0, "viewport point preserved");
        assert!(c.fly_to.is_none());
    }

    #[test]
    fn docked_cursor_is_pinned_until_pushed() {
        let mut c = Cursor::new();
        let items = [link(1, 100.0, 100.0)];
        c.x = 150.0;
        c.y = 115.0;
        c.advance(0.0, 0.0, 0.016, &items);
        assert_eq!(c.attached, Some(1));
        // Neutral ticks: frozen, no drift, no velocity.
        for _ in 0..10 {
            c.advance(0.0, 0.0, 0.016, &items);
        }
        assert_eq!((c.x, c.y), (150.0, 115.0));
        assert_eq!((c.vx, c.vy), (0.0, 0.0));
        assert_eq!(c.attached, Some(1));
        // Weak push below threshold: stays pinned and put.
        c.advance(0.1, 0.0, 0.016, &items);
        assert_eq!(c.attached, Some(1));
        assert_eq!((c.x, c.y), (150.0, 115.0));
    }

    #[test]
    fn landing_with_stick_held_never_breaks() {
        // The t=4311 trace: pushing onto a target with the stick still
        // deflected must NOT kick straight back off on the next tick.
        // Only a release-then-repush leaves.
        let mut c = Cursor::new();
        let items = [link(1, 100.0, 100.0)];
        c.x = 150.0;
        c.y = 115.0;
        c.advance(0.0, 0.0, 0.016, &items);
        assert_eq!(c.attached, Some(1));
        // Still holding hard: latched, stays docked.
        for _ in 0..10 {
            c.advance(0.6, 0.0, 0.016, &items);
            assert_eq!(c.attached, Some(1));
        }
        // Relax but not below the re-arm line: still latched.
        c.advance(0.2, 0.0, 0.016, &items);
        assert_eq!(c.attached, Some(1));
        c.advance(0.26, 0.0, 0.016, &items);
        assert_eq!(c.attached, Some(1), "no release since dock: no break");
        // Release, then the same push breaks at once.
        c.advance(0.0, 0.0, 0.016, &items);
        c.advance(0.26, 0.0, 0.016, &items);
        assert_eq!(c.attached, None);
        assert_eq!(c.no_dock, Some(1));
    }

    #[test]
    fn firm_push_breaks_free_at_once_with_kick() {
        let mut c = Cursor::new();
        let items = [link(1, 100.0, 100.0)];
        c.x = 150.0;
        c.y = 115.0;
        c.advance(0.0, 0.0, 0.016, &items);
        assert_eq!(c.attached, Some(1));
        // Release to re-arm, then one firm tick: detached immediately,
        // moving outward, suppressed from instant re-capture while still
        // inside the hold pad.
        c.advance(0.0, 0.0, 0.016, &items);
        c.advance(1.0, 0.0, 0.016, &items);
        assert_eq!(c.attached, None);
        assert_eq!(c.no_dock, Some(1));
        assert!(c.x > 150.0, "kick moves out at once, x={}", c.x);
        // Keeps escaping while held; no re-dock inside the pad.
        for _ in 0..5 {
            c.advance(1.0, 0.0, 0.016, &items);
        }
        assert_eq!(c.attached, None);
        assert!(c.x > 200.0, "outward progress, x={}", c.x);
    }

    #[test]
    fn release_after_break_free_coasts_out_without_redock() {
        // A firm push detaches with a kick from the rect edge; letting go
        // then coasts out of the hold pad on momentum (no gravity to drag
        // it back), so suppression lifts outside and nothing re-docks.
        let mut c = Cursor::new();
        let items = [link(1, 100.0, 100.0)];
        c.x = 150.0;
        c.y = 115.0;
        c.advance(0.0, 0.0, 0.016, &items);
        assert_eq!(c.attached, Some(1));
        // Release to re-arm, then push off: detaches immediately,
        // suppressed while held.
        c.advance(0.0, 0.0, 0.016, &items);
        c.advance(1.0, 0.0, 0.016, &items);
        assert_eq!(c.attached, None);
        assert_eq!(c.no_dock, Some(1));
        // Let go: the kick coasts the point clear of the hold pad.
        for _ in 0..60 {
            c.advance(0.0, 0.0, 0.016, &items);
        }
        assert!(c.x > 224.0, "kick coasts clear of the hold pad, x={}", c.x);
        assert_eq!(c.no_dock, None);
        assert_eq!(c.attached, None);
    }

    #[test]
    fn suppression_lifts_after_leaving_and_redocks() {
        let mut c = Cursor::new();
        let items = [link(1, 100.0, 100.0)];
        c.x = 150.0;
        c.y = 115.0;
        c.advance(0.0, 0.0, 0.016, &items);
        // Release to re-arm, then push off: suppressed while held.
        c.advance(0.0, 0.0, 0.016, &items);
        c.advance(1.0, 0.0, 0.016, &items);
        assert_eq!(c.no_dock, Some(1));
        // Flown clear of the hold pad: suppression lifts.
        c.x = 500.0;
        c.y = 500.0;
        c.advance(0.0, 0.0, 0.016, &items);
        assert_eq!(c.no_dock, None);
        assert_eq!(c.attached, None);
        // Back inside: docks again.
        c.x = 150.0;
        c.y = 115.0;
        c.advance(0.0, 0.0, 0.016, &items);
        assert_eq!(c.attached, Some(1));
    }

    #[test]
    fn element_carried_away_releases_without_kick() {
        // Page motion (scroll) moves the rect off the frozen point: the
        // dock releases, and with no stick there is no kick.
        let mut c = Cursor::new();
        let items = [link(1, 100.0, 100.0)];
        c.x = 150.0;
        c.y = 115.0;
        c.advance(0.0, 0.0, 0.016, &items);
        assert_eq!(c.attached, Some(1));
        let moved = [link(1, 100.0, 500.0)];
        c.advance(0.0, 0.0, 0.016, &moved);
        assert_eq!(c.attached, None);
        assert_eq!((c.x, c.y), (150.0, 115.0));
    }

    #[test]
    fn full_stick_overpowers_magnet_and_escapes() {
        // Docked at the center of link(1) (center 150,115).
        let mut c = Cursor::new();
        let items = [link(1, 100.0, 100.0)];
        c.x = 150.0;
        c.y = 115.0;
        c.advance(0.0, 0.0, 0.016, &items);
        assert_eq!(c.attached, Some(1));
        // Release to re-arm, then full deflection away: breaks the hold
        // and outruns the old well range.
        c.advance(0.0, 0.0, 0.016, &items);
        for _ in 0..120 {
            c.advance(1.0, 0.0, 0.016, &items);
        }
        assert_eq!(c.attached, None, "full stick must escape the dock");
        assert!(c.x > 250.0, "escaped outward, x={}", c.x);
    }

    #[test]
    fn weak_stick_stays_captured() {
        // A gentle 10% push holds the dock (and never reaches the
        // quarter-stick break line either).
        let mut c = Cursor::new();
        let items = [link(1, 100.0, 100.0)];
        c.x = 150.0;
        c.y = 115.0;
        c.advance(0.0, 0.0, 0.016, &items);
        assert_eq!(c.attached, Some(1));
        for _ in 0..60 {
            c.advance(0.1, 0.0, 0.016, &items);
            if c.attached.is_none() {
                break;
            }
        }
        assert_eq!(c.attached, Some(1), "10% deflection must not escape");
    }

    #[test]
    fn click_point_prefers_attached_center() {
        let mut c = Cursor::new();
        let items = [link(7, 100.0, 200.0)];
        c.x = 110.0;
        c.y = 210.0;
        c.snap(&items);
        assert_eq!(c.click_point(&items), (150.0, 215.0));
    }

    #[test]
    fn break_free_needs_quarter_stick() {
        // Below 25% deflection a dock holds; a fresh push past it detaches
        // in one tick — but only after a release re-arms the latch.
        let mut c = Cursor::new();
        let items = [link(1, 100.0, 100.0)];
        c.x = 150.0;
        c.y = 115.0;
        c.advance(0.0, 0.0, 0.016, &items);
        assert_eq!(c.attached, Some(1));
        c.advance(0.2, 0.0, 0.016, &items);
        assert_eq!(c.attached, Some(1));
        // Past 25% but never released since docking: latched, stays.
        c.advance(0.26, 0.0, 0.016, &items);
        assert_eq!(c.attached, Some(1));
        // Release, then the same push breaks at once.
        c.advance(0.0, 0.0, 0.016, &items);
        c.advance(0.26, 0.0, 0.016, &items);
        assert_eq!(c.attached, None);
    }

    #[test]
    fn sustained_hold_breaks_through_after_grace() {
        // The latch is not a lock: holding past the grace still breaks
        // free, so push-through traversal never strands on a landing.
        let mut c = Cursor::new();
        let items = [link(1, 100.0, 100.0)];
        c.x = 150.0;
        c.y = 115.0;
        c.advance(0.0, 0.0, 0.016, &items);
        assert_eq!(c.attached, Some(1));
        for _ in 0..10 {
            c.advance(0.6, 0.0, 0.016, &items);
            assert_eq!(c.attached, Some(1), "grace holds the landing");
        }
        for _ in 0..30 {
            c.advance(0.6, 0.0, 0.016, &items);
            if c.attached.is_none() {
                break;
            }
        }
        assert_eq!(c.attached, None, "sustained hold breaks through");
        assert_eq!(c.no_dock, Some(1));
    }

    #[test]
    fn dock_kills_arrival_velocity() {
        // Collision zeroes momentum at once: the dock starts from rest, and
        // any later push-off kick is the only velocity it ever carries.
        let mut c = Cursor::new();
        let items = [link(1, 100.0, 100.0)];
        c.x = 195.0;
        c.y = 115.0;
        c.vx = 500.0;
        c.advance(0.0, 0.0, 0.016, &items);
        assert_eq!(c.attached, Some(1));
        assert_eq!((c.vx, c.vy), (0.0, 0.0));
    }

    #[test]
    fn break_free_down_exits_mid_bottom() {
        // LS down from a dock: the point starts at the bottom side, 50%
        // width, with an outward kick (exit placement is exact; the kick
        // integrates onward in the same tick).
        let mut c = Cursor::new();
        let items = [link(1, 100.0, 100.0)];
        c.x = 150.0;
        c.y = 115.0;
        c.advance(0.0, 0.0, 0.016, &items);
        assert_eq!(c.attached, Some(1));
        // Release to re-arm, then down exits mid-bottom with a kick.
        c.advance(0.0, 0.0, 0.016, &items);
        c.advance(0.0, 1.0, 0.016, &items);
        assert_eq!(c.attached, None);
        assert_eq!(c.x, 150.0);
        assert!(c.y > 130.0, "kick carries past the exit edge, y={}", c.y);
        assert!(c.vy > 0.0);
        assert_eq!(c.vx, 0.0);
    }

    #[test]
    fn break_free_diagonal_heads_to_corner() {
        // LS diagonal bottom-right: leaves toward the corner with an
        // outward kick on both axes.
        let mut c = Cursor::new();
        let items = [link(1, 100.0, 100.0)];
        c.x = 150.0;
        c.y = 115.0;
        c.advance(0.0, 0.0, 0.016, &items);
        assert_eq!(c.attached, Some(1));
        // Release to re-arm, then the diagonal heads to the corner.
        c.advance(0.0, 0.0, 0.016, &items);
        c.advance(1.0, 1.0, 0.016, &items);
        assert_eq!(c.attached, None);
        assert!(c.x > 150.0 && c.y > 115.0);
        assert!(c.vx > 0.0 && c.vy > 0.0);
    }

    #[test]
    fn exit_point_leaves_along_push_direction() {
        let item = link(1, 100.0, 100.0); // 100x30, center (150,115)
        assert_eq!(Cursor::exit_point(&item, 0.0, 1.0), (150.0, 130.0));
        assert_eq!(Cursor::exit_point(&item, 0.0, -1.0), (150.0, 100.0));
        assert_eq!(Cursor::exit_point(&item, -1.0, 0.0), (100.0, 115.0));
        assert_eq!(Cursor::exit_point(&item, 0.0, 0.0), (150.0, 115.0));
        // Diagonal on a wide element hits the nearer edge, not the corner.
        let (ex, ey) = Cursor::exit_point(&item, 1.0, 1.0);
        assert!(
            (ex - 165.0).abs() < 0.01 && (ey - 130.0).abs() < 0.01,
            "wide exit=({ex},{ey})"
        );
        // Diagonal on a square element exits the corner.
        let square = Focusable {
            id: 2,
            x: 100.0,
            y: 100.0,
            w: 100.0,
            h: 100.0,
            kind: FocusKind::Link,
            url: None,
        };
        let (cx, cy) = Cursor::exit_point(&square, 1.0, 1.0);
        assert!(
            (cx - 200.0).abs() < 0.01 && (cy - 200.0).abs() < 0.01,
            "square exit=({cx},{cy})"
        );
    }

    #[test]
    fn scrolling_releases_dock_and_holds_the_viewport_point() {
        let mut c = Cursor::new();
        let items = [link(1, 100.0, 100.0)];
        c.x = 150.0;
        c.y = 115.0;
        c.advance(0.0, 0.0, 0.016, &items);
        assert_eq!(c.attached, Some(1));
        // Scrolling starts: the dock releases immediately and no new dock
        // is acquired while the scroll stick is active. The point holds.
        let replacement = [link(2, 100.0, 90.0)];
        for _ in 0..10 {
            c.advance_with(0.0, 0.0, 0.016, &replacement, true);
            assert_eq!(c.attached, None, "no docking while scrolling");
            assert_eq!((c.x, c.y), (150.0, 115.0), "viewport point held");
        }
        // Scroll stops: the neutral acquire pass docks in place.
        c.advance(0.0, 0.0, 0.016, &replacement);
        assert_eq!(c.attached, Some(2));
        assert_eq!(
            (c.x, c.y),
            (150.0, 115.0),
            "re-dock must not move the point"
        );
        assert_eq!(c.last_dock, Some((100.0, 90.0, 100.0, 30.0)));
    }

    #[test]
    fn scrolling_never_acquires_a_fresh_dock() {
        let mut c = Cursor::new();
        let items = [link(1, 100.0, 100.0)];
        c.x = 150.0;
        c.y = 115.0;
        c.advance_with(0.0, 0.0, 0.016, &items, true);
        assert_eq!(c.attached, None);
        assert_eq!((c.x, c.y), (150.0, 115.0));
        // Once the stick is released the element under the point docks.
        c.advance(0.0, 0.0, 0.016, &items);
        assert_eq!(c.attached, Some(1));
        assert_eq!((c.x, c.y), (150.0, 115.0));
    }

    #[test]
    fn click_point_free_until_docked() {
        // No well fallback: the free point clicks where it is until the
        // outline actually engages.
        let mut c = Cursor::new();
        let items = [link(7, 100.0, 200.0)];
        c.x = 240.0;
        c.y = 215.0;
        c.advance(0.0, 0.0, 0.016, &items);
        assert_eq!(c.attached, None);
        assert_eq!(c.click_point(&items), (240.0, 215.0));
        // Nothing in range: the free point.
        c.x = 500.0;
        c.y = 500.0;
        c.vx = 0.0;
        c.vy = 0.0;
        c.advance(0.0, 0.0, 0.016, &items);
        assert_eq!(c.click_point(&items), (500.0, 500.0));
    }

    #[test]
    fn click_point_magnet_off_means_free_point() {
        // R3 flag #6: magnet-off cursor at (299,115) must not yank to the
        // search field's center 47px away.
        let mut c = Cursor::new();
        c.magnet = false;
        let items = [link(7, 100.0, 200.0)];
        c.x = 240.0;
        c.y = 215.0;
        assert_eq!(c.attached, None);
        assert_eq!(c.click_point(&items), (240.0, 215.0));
    }

    #[test]
    fn retarget_drops_stale_attachments() {
        let mut c = Cursor::new();
        c.attached = Some(9);
        c.retarget(&[link(1, 0.0, 0.0)]);
        assert_eq!(c.attached, None);
        c.attached = Some(1);
        c.retarget(&[link(1, 0.0, 0.0)]);
        assert_eq!(c.attached, Some(1));
    }
}
