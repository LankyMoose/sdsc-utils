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
pub const CURSOR_ACCEL: f32 = 7000.0;
/// Left-stick cursor top speed, px/s.
pub const CURSOR_MAX_SPEED: f32 = 1400.0;
/// Velocity decay rate (exponential friction, per second). The cursor glides
/// a little after release instead of stopping dead.
pub const CURSOR_FRICTION: f32 = 14.0;
/// Dock entry/exit pads in page pixels: the outline engages when the point
/// enters an element's rect + ACQUIRE_PAD and holds until it leaves +
/// HOLD_PAD, so edge dither never flickers the wrap.
pub const ACQUIRE_PAD: f32 = 8.0;
pub const HOLD_PAD: f32 = 24.0;
/// Dock-blend rate: the outline morphs free↔docked in ~140ms, eased every
/// analog tick so no frame pops.
pub const DOCK_BLEND_PER_SEC: f32 = 7.0;
/// D-pad flight speed, px/s: jumps glide to the target instead of
/// teleporting; stick input cancels back to manual control.
pub const FLY_SPEED: f32 = 2400.0;
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
/// Engine viewport the cursor space matches (see `servo_backend`).
pub const VIEWPORT_W: f32 = 800.0;
pub const VIEWPORT_H: f32 = 600.0;
/// Top of the unified cursor space: the header lives above the viewport as
/// synthetic snap targets (tab strip, toolbar, bookmarks), so the pad
/// cursor treats chrome and page as one continuous surface — pushing past
/// the viewport top walks into the header, pushing back down returns.
/// The overlay clips to the page image, so the dot hides while docked up
/// there and the header chip carries the focus instead.
pub const CURSOR_MIN_Y: f32 = -140.0;
/// Synthetic chrome ids live far above any engine-assigned page id, so a
/// cursor attachment unambiguously addresses chrome (pad action, never an
/// engine click) or page (engine click path).
pub const CHROME_ID_BASE: u64 = 1 << 62;

/// Directional jump axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JumpDir {
    Up,
    Down,
    Left,
    Right,
}

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
    /// D-pad flight target (page px): jumps glide here at
    /// [`FLY_SPEED`] instead of teleporting; stick input cancels.
    pub fly_to: Option<(f32, f32)>,
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
    pub visible: bool,
}

/// Perpendicular band metrics between a docked rect and a candidate for a
/// move along `dir`: `(forward gap, shared perpendicular extent,
/// perpendicular centre offset, centre line crossed)`. Gap is clamped at
/// zero when the rects overlap on the movement axis; shared extent is zero
/// when they do not overlap on the perpendicular axis. Centre line crossed
/// means the docked rect's perpendicular centre falls inside the
/// candidate's span — the target sits in the same visual row/column.
fn band_metrics(from: (f32, f32, f32, f32), to: &Focusable, dir: JumpDir) -> (f32, f32, f32, bool) {
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
            let centre = fx0 + fw / 2.0;
            let on_axis = to.x <= centre && centre <= tx1;
            (gap.max(0.0), shared.max(0.0), column, on_axis)
        }
        JumpDir::Left | JumpDir::Right => {
            let shared = fy1.min(ty1) - fy0.max(to.y);
            let gap = if dir == JumpDir::Left {
                fx0 - tx1
            } else {
                to.x - fx1
            };
            let column = ((fy0 + fh / 2.0) - (to.y + to.h / 2.0)).abs();
            let centre = fy0 + fh / 2.0;
            let on_axis = to.y <= centre && centre <= ty1;
            (gap.max(0.0), shared.max(0.0), column, on_axis)
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
            break_armed: true,
            docked_for: 0.0,
            magnet: true,
            visible: true,
        }
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
    /// Free movement with viewport clamping. Any manual move drops the
    /// attachment first; docking re-engages on arrival.
    pub fn move_by(&mut self, dx: f32, dy: f32) {
        if dx == 0.0 && dy == 0.0 {
            return;
        }
        self.attached = None;
        self.x = (self.x + dx).clamp(0.0, VIEWPORT_W);
        self.y = (self.y + dy).clamp(0.0, VIEWPORT_H);
    }

    /// Analog velocity step, D-pad flights, and dock morph state. The stick
    /// accelerates a velocity vector and friction decays it; flights glide
    /// to jump targets (stick cancels). Attachment + `dock_blend`
    /// ease every tick, so the overlay never pops.
    pub fn advance(&mut self, lx: f32, ly: f32, dt_secs: f32, options: &[Focusable]) {
        // D-pad flights glide to the target; stick input cancels back to
        // manual control.
        if self.fly_to.is_some() && (lx != 0.0 || ly != 0.0) {
            self.fly_to = None;
        }
        // Letting go inside the hold pad lifts break-free suppression: the
        // point never left, so hovering means re-dock, not escape.
        if self.no_dock.is_some() && lx.hypot(ly) < 0.01 && self.fly_to.is_none() {
            self.no_dock = None;
        }
        // Docked: pinned to the element center — no velocity. A firm push
        // breaks free at once (detach + place on the exit edge + kick
        // outward); page motion carrying the element away releases without
        // a kick. The hold check runs against the previous point so a
        // carried-away element actually releases instead of re-centering
        // onto its new rect forever. Breaks are edge-triggered: docking
        // latches until the stick drops below BREAK_REARM_DEFLECT, so
        // arriving with the stick still held never kicks straight back
        // off — but a hold sustained past BREAK_LATCH_GRACE_SECS breaks
        // through anyway, so traversal never strands.
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
                self.no_dock = was;
                if pushed && let Some(it) = item {
                    // Leave from the rect edge along the push: down exits
                    // mid-bottom, diagonals exit corners — the point starts
                    // where the velocity says it is going.
                    let (ex, ey) = Self::exit_point(it, lx, ly);
                    self.x = ex.clamp(0.0, VIEWPORT_W);
                    self.y = ey.clamp(CURSOR_MIN_Y, VIEWPORT_H);
                    self.vx = lx * BREAK_KICK;
                    self.vy = ly * BREAK_KICK;
                }
            } else {
                // Still held and unpushed: re-pin to the center so page
                // motion drags the point along instead of stranding it.
                if let Some(it) = item {
                    self.x = (it.x + it.w / 2.0).clamp(0.0, VIEWPORT_W);
                    self.y = (it.y + it.h / 2.0).clamp(CURSOR_MIN_Y, VIEWPORT_H);
                }
                self.ease_blend(dt_secs);
                return;
            }
        }
        if let Some((tx, ty)) = self.fly_to {
            let dx = tx - self.x;
            let dy = ty - self.y;
            let dist = dx.hypot(dy);
            let step = FLY_SPEED * dt_secs;
            if dist <= step.max(1.0) {
                self.x = tx.clamp(0.0, VIEWPORT_W);
                self.y = ty.clamp(CURSOR_MIN_Y, VIEWPORT_H);
                self.fly_to = None;
                self.vx = 0.0;
                self.vy = 0.0;
            } else if dist > 0.0 {
                self.x = (self.x + dx / dist * step).clamp(0.0, VIEWPORT_W);
                self.y = (self.y + dy / dist * step).clamp(CURSOR_MIN_Y, VIEWPORT_H);
                self.vx = 0.0;
                self.vy = 0.0;
            }
        } else {
            self.vx =
                (self.vx + lx * CURSOR_ACCEL * dt_secs).clamp(-CURSOR_MAX_SPEED, CURSOR_MAX_SPEED);
            self.vy =
                (self.vy + ly * CURSOR_ACCEL * dt_secs).clamp(-CURSOR_MAX_SPEED, CURSOR_MAX_SPEED);
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
            self.x = (self.x + self.vx * dt_secs).clamp(0.0, VIEWPORT_W);
            self.y = (self.y + self.vy * dt_secs).clamp(CURSOR_MIN_Y, VIEWPORT_H);
        }
        // Docking is arrival-based (see `snap`): the outline only engages
        // once the gliding point actually reaches an element. The morph
        // state eases toward the attachment every tick — including neutral
        // ones — so shape, size, and position always interpolate.
        if self.magnet {
            self.snap(options);
        }
        self.ease_blend(dt_secs);
    }

    /// Ease the outline morph toward the attachment (1 = docked, 0 = free).
    fn ease_blend(&mut self, dt_secs: f32) {
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
    /// Docked ranking is band-first, not centre-distance. Targets crossed
    /// by the docked item's perpendicular centre line (same row/column)
    /// outrank every partial overlap, and partial overlaps outrank purely
    /// diagonal targets — so a below-right target can never win a Right
    /// press over a right target. Within a tier the widest shared
    /// perpendicular extent wins, then the nearest forward row, then the
    /// smallest column offset. With no dock (or nothing sharing the band)
    /// it falls back to the 60-degree cone and nearest-point scoring.
    pub fn jump(&mut self, dir: JumpDir, options: &[Focusable]) -> bool {
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
            /// 2 = docked centre line crosses this target (same row or
            /// column), 1 = partial band overlap, 0 = diagonal fallback.
            tier: u8,
        }
        let from = self.attached_rect(options);
        let mut cands: Vec<Candidate<'_>> = Vec::new();
        for item in options {
            if Some(item.id) == self.attached {
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
            let (gap, shared, column, on_axis) = match from {
                Some(f) => band_metrics(f, item, dir),
                None => (along, 0.0, across, false),
            };
            // A candidate sharing the docked band is a legitimate row move
            // even off-axis; only diagonal (out-of-band) targets are coned.
            if shared <= 0.0 && across > along {
                continue;
            }
            let tier = if on_axis {
                2
            } else if shared > 0.0 {
                1
            } else {
                0
            };
            cands.push(Candidate {
                item,
                along,
                across,
                gap,
                shared,
                column,
                tier,
            });
        }
        let top_tier = cands.iter().map(|c| c.tier).max().unwrap_or(0);
        let winner = cands.iter().filter(|c| c.tier == top_tier).min_by(|a, b| {
            if top_tier == 0 {
                // Free cursor or fully diagonal: nearest point in the cone.
                let sa = a.along + a.across * 2.0;
                let sb = b.along + b.across * 2.0;
                sa.partial_cmp(&sb).unwrap_or(std::cmp::Ordering::Equal)
            } else {
                // Same row/column first: widest shared extent, then the
                // nearest forward row, then the closest column.
                b.shared
                    .partial_cmp(&a.shared)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| {
                        a.gap
                            .partial_cmp(&b.gap)
                            .unwrap_or(std::cmp::Ordering::Equal)
                    })
                    .then_with(|| {
                        a.column
                            .partial_cmp(&b.column)
                            .unwrap_or(std::cmp::Ordering::Equal)
                    })
            }
        });
        if let Some(c) = winner {
            self.jump_to(c.item);
            true
        } else {
            false
        }
    }

    /// Fly straight to a known target, bypassing directional scoring. Row-
    /// stepped header navigation picks the slot itself (column-preserved),
    /// and a pick can sit outside the 60-degree cone the spatial jump uses.
    pub fn jump_to(&mut self, item: &Focusable) {
        self.attached = None;
        self.vx = 0.0;
        self.vy = 0.0;
        self.fly_to = Some((
            (item.x + item.w / 2.0).clamp(0.0, VIEWPORT_W),
            (item.y + item.h / 2.0).clamp(CURSOR_MIN_Y, VIEWPORT_H),
        ));
    }

    /// Dock when the point actually arrives: attach to the element whose
    /// rect (expanded by [`ACQUIRE_PAD`]) contains the point, and center
    /// the point on it — clicks and push-offs start from the middle, not
    /// the arrival edge. Holding is stickier than acquiring ([`HOLD_PAD`])
    /// so edge dither never flickers the outline.
    pub fn snap(&mut self, options: &[Focusable]) {
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
            && self.inside_expanded(item, HOLD_PAD)
        {
            return;
        }
        self.attached = None;
        let mut best: Option<&Focusable> = None;
        let mut best_d = f32::INFINITY;
        // Zone rule: above the viewport only header chrome competes, below
        // it only page targets do. Without this a glide across the sparse
        // header keeps getting yanked down by dense page links (and back
        // up) — the bounce. Jumps don't need it: they score directionally.
        let in_header = self.y < 0.0;
        for item in options {
            if (item.id >= CHROME_ID_BASE) != in_header {
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
            self.x = (item.x + item.w / 2.0).clamp(0.0, VIEWPORT_W);
            self.y = (item.y + item.h / 2.0).clamp(CURSOR_MIN_Y, VIEWPORT_H);
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
            return (item.x + item.w / 2.0, item.y + item.h / 2.0);
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
        // Docking centers the point.
        assert_eq!((c.x, c.y), (150.0, 115.0));
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
        // Drive into the pad: docks and centers at once.
        c.x = 205.0;
        c.advance(0.0, 0.0, 0.016, &items);
        assert_eq!(c.attached, Some(1));
        assert_eq!((c.x, c.y), (150.0, 115.0));
    }

    #[test]
    fn jump_flies_into_header_space_and_docks() {
        // Unified cursor space: a D-pad Up from the top page target lands
        // on the synthetic header slot above the viewport (negative y).
        let mut c = Cursor::new();
        let chrome = Focusable {
            id: 1 << 62,
            x: 300.0,
            y: -72.0,
            w: 100.0,
            h: 36.0,
            kind: FocusKind::Button,
            url: None,
        };
        let page = link(1, 300.0, 100.0);
        let items = [page, chrome];
        c.x = 350.0;
        c.y = 115.0;
        c.advance(0.0, 0.0, 0.016, &items);
        assert_eq!(c.attached, Some(1));
        // Push up past the page target: jump targets the header slot.
        assert!(c.jump(JumpDir::Up, &items));
        // Fly until arrival (jump glides, docks on arrival).
        for _ in 0..60 {
            c.advance(0.0, 0.0, 0.016, &items);
            if c.attached == Some(1 << 62) {
                break;
            }
        }
        assert_eq!(c.attached, Some(1 << 62));
        assert!(c.y < 0.0);
    }

    #[test]
    fn magnet_is_zone_gated() {
        // While roaming the header (y < 0) only chrome competes for the
        // magnet: a dense page link directly below never yanks the cursor
        // back down mid-travel (and vice versa at y >= 0).
        let mut c = Cursor::new();
        let chrome = Focusable {
            id: 1 << 62,
            x: 300.0,
            y: -72.0,
            w: 100.0,
            h: 36.0,
            kind: FocusKind::Button,
            url: None,
        };
        // Page link directly below the chrome slot.
        let page = link(1, 300.0, 10.0);
        let items = [page, chrome];
        // Roaming free in the header band docks chrome, not the page link
        // a few dozen px below.
        c.x = 350.0;
        c.y = -54.0;
        c.advance(0.0, 0.0, 0.016, &items);
        assert_eq!(c.attached, Some(1 << 62));
        // Roaming free just inside the viewport ignores the chrome above.
        let mut c2 = Cursor::new();
        c2.x = 350.0;
        c2.y = 20.0;
        c2.advance(0.0, 0.0, 0.016, &items);
        assert_eq!(c2.attached, Some(1));
    }

    #[test]
    fn docked_jump_stays_in_the_docked_band() {
        // Trace case: Left from the wide omnibox must land on the adjacent
        // toolbar button, not a strip slot two rows up whose nearest edge
        // is closer in x. Direction is the gate; shared row extent decides
        // eligibility before any distance scoring.
        let mut c = Cursor::new();
        let omni = Focusable {
            id: CHROME_ID_BASE,
            x: 228.0,
            y: -72.0,
            w: 456.0,
            h: 36.0,
            kind: FocusKind::Button,
            url: None,
        };
        let reload = Focusable {
            id: CHROME_ID_BASE + 1,
            x: 114.0,
            y: -72.0,
            w: 57.0,
            h: 36.0,
            kind: FocusKind::Button,
            url: None,
        };
        let strip = Focusable {
            id: CHROME_ID_BASE + 2,
            x: 246.0,
            y: -112.0,
            w: 185.0,
            h: 36.0,
            kind: FocusKind::Button,
            url: None,
        };
        let items = [omni, reload, strip];
        c.x = 456.0;
        c.y = -54.0;
        c.advance(0.0, 0.0, 0.016, &items);
        assert_eq!(c.attached, Some(CHROME_ID_BASE));
        assert!(c.jump(JumpDir::Left, &items));
        for _ in 0..60 {
            c.advance(0.0, 0.0, 0.016, &items);
            if c.attached == Some(CHROME_ID_BASE + 1) {
                break;
            }
        }
        assert_eq!(c.attached, Some(CHROME_ID_BASE + 1));
    }

    #[test]
    fn vertical_jump_ranks_shared_width_within_the_row() {
        // Two content rows: a(200) b(100) over c(150) d(80). Up from
        // either row-2 slot must pick a: it shares 150px with c and 50px
        // with d, while b shares 0 and 30. Center distance would pick b
        // from d; shared extent picks a.
        let a = chrome_btn(1, 0.0, 100.0, 200.0, 40.0);
        let b = chrome_btn(2, 200.0, 100.0, 100.0, 40.0);
        let c = chrome_btn(3, 0.0, 200.0, 150.0, 40.0);
        let d = chrome_btn(4, 150.0, 200.0, 80.0, 40.0);
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
        let current = chrome_btn(1, 100.0, 100.0, 100.0, 100.0);
        let right = chrome_btn(2, 250.0, 100.0, 100.0, 50.0);
        let below_right = chrome_btn(3, 220.0, 220.0, 100.0, 50.0);
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

    fn chrome_btn(id: u64, x: f32, y: f32, w: f32, h: f32) -> Focusable {
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
    fn header_down_picks_slot_below_over_aligned_page_link() {
        // Trace case: Down from a strip tab landed on a page link two rows
        // down, skipping the toolbar. Nearest-point scoring prefers the
        // slot directly below.
        let mut c = Cursor::new();
        let omni = chrome_btn(CHROME_ID_BASE, 228.0, -72.0, 457.0, 36.0);
        let page = link(1, 283.0, 50.0);
        let items = [page, omni];
        c.x = 338.0;
        c.y = -94.0;
        assert!(c.jump(JumpDir::Down, &items));
        for _ in 0..60 {
            c.advance(0.0, 0.0, 0.016, &items);
            if c.attached == Some(CHROME_ID_BASE) {
                break;
            }
        }
        assert_eq!(c.attached, Some(CHROME_ID_BASE));
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
        // D-pad past the last element is a no-op: it used to nudge the
        // cursor into empty space above the header (invisible, nothing to
        // dock to). Jump-only movement stays exactly put.
        let mut c = Cursor::new();
        let tab = Focusable {
            id: CHROME_ID_BASE,
            x: 0.0,
            y: -112.0,
            w: 185.0,
            h: 36.0,
            kind: FocusKind::Button,
            url: None,
        };
        let items = [tab];
        c.x = 92.0;
        c.y = -94.0;
        c.advance(0.0, 0.0, 0.016, &items);
        assert_eq!(c.attached, Some(CHROME_ID_BASE));
        assert!(!c.jump(JumpDir::Up, &items));
        assert_eq!(c.attached, Some(CHROME_ID_BASE));
        // Dock re-pins to the slot center; the blocked press moves nothing.
        assert_eq!((c.x, c.y), (92.5, -94.0));
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
        assert_eq!(c.y, 569.0, "pinned to the element center, not the edge");
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
    fn docked_pin_follows_page_motion() {
        // The element scrolls under a docked cursor: still held, so the
        // point re-pins to the new center instead of stranding.
        let mut c = Cursor::new();
        let items = [link(1, 100.0, 100.0)];
        c.x = 150.0;
        c.y = 115.0;
        c.advance(0.0, 0.0, 0.016, &items);
        assert_eq!(c.attached, Some(1));
        let moved = [link(1, 100.0, 110.0)];
        c.advance(0.0, 0.0, 0.016, &moved);
        assert_eq!(c.attached, Some(1));
        assert_eq!((c.x, c.y), (150.0, 125.0));
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
