//! Virtual cursor for controller-first browsing.
//!
//! The pad owns a cursor over the rendered page: the left stick (and D-pad
//! flights) move a free point, and it magnetically attaches to the nearest
//! focusable element. Rendering morphs a ring between dot (free) and wrap
//! (docked) every tick — position, size, and shape always interpolate, so
//! no frame pops. Cross clicks the attached element (or the free point).
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusKind {
    Link,
    Button,
    Field,
    Other,
}

/// Gravity radius in page pixels: magnetism only pulls this close to a
/// focusable. Outside it the stick drives free-form. Halved from 100: the
/// old range grabbed across unrelated links.
pub const SNAP_RADIUS: f32 = 50.0;
/// D-pad nudge step in page pixels.
pub const NUDGE_STEP: f32 = 32.0;
/// Left-stick cursor acceleration at full deflection, px/s^2.
pub const CURSOR_ACCEL: f32 = 7000.0;
/// Left-stick cursor top speed, px/s.
pub const CURSOR_MAX_SPEED: f32 = 1400.0;
/// Velocity decay rate (exponential friction, per second). The cursor glides
/// a little after release instead of stopping dead. Doubled from 7: the old
/// value felt slippery and let the magnet yank at high speed.
pub const CURSOR_FRICTION: f32 = 14.0;
/// Magnetic pull cap, as acceleration: exactly one quarter of stick
/// thrust, so holding the stick past ~25% deflection always overpowers the
/// magnet at any range, while lighter touches get captured. Falls off with
/// the square root of closeness to zero at the snap radius edge (soft
/// capture that never dips under the friction cutoff).
pub const PULL_MAX_ACCEL: f32 = CURSOR_ACCEL * 0.25;
/// Dock entry/exit pads in page pixels: the outline engages when the point
/// enters an element's rect + ACQUIRE_PAD and holds until it leaves +
/// HOLD_PAD, so edge dither never flickers the wrap. Gravity (SNAP_RADIUS)
/// reaches further than the outline — the dot visibly glides in and the
/// wrap only pops once it actually arrives.
pub const ACQUIRE_PAD: f32 = 8.0;
pub const HOLD_PAD: f32 = 24.0;
/// Dock-blend rate: the outline morphs free↔docked in ~140ms, eased every
/// analog tick so no frame pops.
pub const DOCK_BLEND_PER_SEC: f32 = 7.0;
/// D-pad flight speed, px/s: jumps and nudges glide to the target instead
/// of teleporting; stick input cancels back to manual control.
pub const FLY_SPEED: f32 = 2400.0;
/// Break-free stick deflection while docked: past this the dock releases at
/// once (detach + outward kick) instead of fighting gravity off gradually.
pub const BREAK_FREE_DEFLECT: f32 = 0.3;
/// Outward kick on break-free, px/s: immediate motion in the push direction.
pub const BREAK_KICK: f32 = 800.0;
/// Right-stick scroll speed at full deflection, px/s.
pub const SCROLL_SPEED: f32 = 1500.0;
/// Engine viewport the cursor space matches (see `servo_backend`).
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

#[derive(Debug, Clone, Default)]
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
    /// D-pad flight target (page px): jumps/nudges glide here at
    /// [`FLY_SPEED`] instead of teleporting; stick input cancels.
    pub fly_to: Option<(f32, f32)>,
    /// Break-free suppression: id that must not re-dock until the point
    /// leaves its hold pad (prevents instant re-capture after a push-off).
    pub no_dock: Option<u64>,
    /// Magnetic mode (L3 toggles): snap + glide to nearby elements.
    /// Off = pure free-form virtual mouse; D-pad jumping still applies.
    pub magnet: bool,
    pub visible: bool,
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
            magnet: true,
            visible: true,
        }
    }

    /// Free movement with viewport clamping. Any manual move drops the
    /// attachment first; the caller re-snaps afterwards.
    pub fn move_by(&mut self, dx: f32, dy: f32) {
        if dx == 0.0 && dy == 0.0 {
            return;
        }
        self.attached = None;
        self.x = (self.x + dx).clamp(0.0, VIEWPORT_W);
        self.y = (self.y + dy).clamp(0.0, VIEWPORT_H);
    }

    /// Analog velocity step with magnetic gravity, D-pad flights, and dock
    /// morph state. The stick accelerates a velocity vector, friction decays
    /// it, and the nearest focusable within range pulls proportionally to
    /// closeness; flights glide to jump/nudge targets (stick cancels).
    /// Attachment + `dock_blend` ease every tick, so the overlay never pops.
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
        // Docked: pinned — no velocity, no gravity. A firm push breaks free
        // at once (detach + kick outward); page motion carrying the element
        // away releases without a kick.
        if self.attached.is_some() {
            self.vx = 0.0;
            self.vy = 0.0;
            let pushed = lx.hypot(ly) > BREAK_FREE_DEFLECT;
            let held = self
                .attached
                .and_then(|id| options.iter().find(|o| o.id == id))
                .is_some_and(|item| self.inside_expanded(item, HOLD_PAD));
            if pushed || !held {
                let was = self.attached;
                self.attached = None;
                self.no_dock = was;
                if pushed {
                    self.vx = lx * BREAK_KICK;
                    self.vy = ly * BREAK_KICK;
                }
            } else {
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
                self.y = ty.clamp(0.0, VIEWPORT_H);
                self.fly_to = None;
                self.vx = 0.0;
                self.vy = 0.0;
            } else if dist > 0.0 {
                self.x = (self.x + dx / dist * step).clamp(0.0, VIEWPORT_W);
                self.y = (self.y + dy / dist * step).clamp(0.0, VIEWPORT_H);
                self.vx = 0.0;
                self.vy = 0.0;
            }
        } else {
            self.vx =
                (self.vx + lx * CURSOR_ACCEL * dt_secs).clamp(-CURSOR_MAX_SPEED, CURSOR_MAX_SPEED);
            self.vy =
                (self.vy + ly * CURSOR_ACCEL * dt_secs).clamp(-CURSOR_MAX_SPEED, CURSOR_MAX_SPEED);
            if self.magnet
                && let Some((_, tx, ty, edge, clen)) = Self::nearest_within(self.x, self.y, options)
            {
                // Gravity, not teleport: pull toward the element center with
                // a magnitude that falls off to zero at the snap radius
                // edge — soft capture from afar, firm center. Square-root
                // falloff keeps edge pull above the friction cutoff so the
                // rim never deadlocks. Direction normalizes by *center*
                // distance; the edge distance drives the falloff only.
                if clen > 0.5 {
                    let fall = (1.0 - edge / SNAP_RADIUS).max(0.0);
                    let strength = PULL_MAX_ACCEL * fall.sqrt();
                    self.vx += ((tx - self.x) / clen) * strength * dt_secs;
                    self.vy += ((ty - self.y) / clen) * strength * dt_secs;
                }
            }
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
            self.y = (self.y + self.vy * dt_secs).clamp(0.0, VIEWPORT_H);
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

    /// Nearest focusable within snap radius of a point: its center, the edge
    /// distance (drives pull falloff + attach range), and the center distance
    /// (drives pull direction). Shared by the per-tick gravity and the snap
    /// pass.
    fn nearest_within(x: f32, y: f32, options: &[Focusable]) -> Option<(u64, f32, f32, f32, f32)> {
        let mut best: Option<(u64, f32, f32, f32, f32)> = None;
        for item in options {
            let dx = if x < item.x {
                item.x - x
            } else if x > item.x + item.w {
                x - (item.x + item.w)
            } else {
                0.0
            };
            let dy = if y < item.y {
                item.y - y
            } else if y > item.y + item.h {
                y - (item.y + item.h)
            } else {
                0.0
            };
            let edge = dx.hypot(dy);
            if edge <= SNAP_RADIUS {
                let cx = item.x + item.w / 2.0;
                let cy = item.y + item.h / 2.0;
                let clen = (cx - x).hypot(cy - y);
                if best.is_none_or(|(_, _, _, be, _)| edge < be) {
                    best = Some((item.id, cx, cy, edge, clen));
                }
            }
        }
        best
    }

    /// Nudge for D-pad taps: a short flight, not a step. Kills velocity so a
    /// tap never inherits drift; arrival docks via the next `advance`.
    pub fn nudge(&mut self, dir: JumpDir, _options: &[Focusable]) {
        let (dx, dy) = match dir {
            JumpDir::Up => (0.0, -NUDGE_STEP),
            JumpDir::Down => (0.0, NUDGE_STEP),
            JumpDir::Left => (-NUDGE_STEP, 0.0),
            JumpDir::Right => (NUDGE_STEP, 0.0),
        };
        if dx == 0.0 && dy == 0.0 {
            return;
        }
        self.attached = None;
        self.vx = 0.0;
        self.vy = 0.0;
        self.fly_to = Some((
            (self.x + dx).clamp(0.0, VIEWPORT_W),
            (self.y + dy).clamp(0.0, VIEWPORT_H),
        ));
    }

    /// Directional jump to the nearest *other* element roughly in `dir`;
    /// falls back to a nudge when nothing lies that way. Returns true on a
    /// jump. The point flies to the target and docks on arrival — no
    /// teleport frames. D-pad always snaps — this ignores the magnet toggle.
    pub fn jump(&mut self, dir: JumpDir, options: &[Focusable]) -> bool {
        let (dx, dy) = match dir {
            JumpDir::Up => (0.0, -1.0),
            JumpDir::Down => (0.0, 1.0),
            JumpDir::Left => (-1.0, 0.0),
            JumpDir::Right => (1.0, 0.0),
        };
        let mut best: Option<(u64, f32)> = None;
        for item in options {
            if Some(item.id) == self.attached {
                continue;
            }
            let cx = item.x + item.w / 2.0 - self.x;
            let cy = item.y + item.h / 2.0 - self.y;
            let along = cx * dx + cy * dy;
            if along <= 0.0 {
                continue;
            }
            let across = (cx * dy - cy * dx).abs();
            // 60-degree cone: lateral offset must stay within the forward run.
            if across > along {
                continue;
            }
            let score = along + across * 2.0;
            if best.is_none_or(|(_, s)| score < s) {
                best = Some((item.id, score));
            }
        }
        if let Some((id, _)) = best {
            if let Some(item) = options.iter().find(|o| o.id == id) {
                // Fly, don't teleport: the outline morphs on arrival.
                self.attached = None;
                self.vx = 0.0;
                self.vy = 0.0;
                self.fly_to = Some((
                    (item.x + item.w / 2.0).clamp(0.0, VIEWPORT_W),
                    (item.y + item.h / 2.0).clamp(0.0, VIEWPORT_H),
                ));
            }
            true
        } else {
            self.nudge(dir, options);
            false
        }
    }

    /// Dock when the point actually arrives: attach to the nearest element
    /// whose rect (expanded by [`ACQUIRE_PAD`]) contains the point. Holding
    /// is stickier than acquiring ([`HOLD_PAD`]) so edge dither never
    /// flickers the outline. Gravity reaches further than the outline — the
    /// dot visibly glides in before the wrap pops.
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
        let mut best: Option<(u64, f32)> = None;
        for item in options {
            if !self.inside_expanded(item, ACQUIRE_PAD) {
                continue;
            }
            // Suppressed (just pushed off): no instant re-capture.
            if Some(item.id) == self.no_dock {
                continue;
            }
            let d = (item.x + item.w / 2.0 - self.x).hypot(item.y + item.h / 2.0 - self.y);
            if best.is_none_or(|(_, bd)| d < bd) {
                best = Some((item.id, d));
            }
        }
        self.attached = best.map(|(id, _)| id);
        // Remember the docked rect: if the point leaves, the wrap morphs
        // back into the dot from here instead of vanishing a frame early.
        if let Some(id) = self.attached
            && let Some(item) = options.iter().find(|o| o.id == id)
        {
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
    /// center when docked, the gravity well's center while falling in, else
    /// the free cursor.
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
        if let Some((_, tx, ty, _, _)) = Self::nearest_within(self.x, self.y, options) {
            return (tx, ty);
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
        }
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
        // Nothing above: nudge flies upward instead, no dock.
        assert!(!c.jump(JumpDir::Up, &[]));
        c.advance(0.0, 0.0, 0.016, &[]);
        assert!(c.y < 115.0);
        assert_eq!(c.attached, None);
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
        // Dock: blend eases 0 → 1 over ticks, never jumps.
        c.x = 210.0;
        c.y = 115.0;
        c.advance(0.0, 0.0, 0.016, &items);
        assert!(c.dock_blend < 1.0);
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
    fn dock_requires_arrival_outline_follows_glide() {
        // 10px outside the rect: gravity well captured, but no outline yet.
        let mut c = Cursor::new();
        let items = [link(1, 100.0, 100.0)];
        c.x = 210.0;
        c.y = 115.0;
        c.advance(0.0, 0.0, 0.016, &items);
        assert_eq!(c.attached, None, "outline pops on arrival, not on entry");
        assert_eq!(c.click_point(&items), (150.0, 115.0));
        // Neutral ticks: the point glides in and docks on arrival.
        for _ in 0..120 {
            c.advance(0.0, 0.0, 0.016, &items);
            if c.attached.is_some() {
                break;
            }
        }
        assert_eq!(c.attached, Some(1));
        assert!(c.x <= 208.0, "docked inside acquire pad, x={}", c.x);
    }

    #[test]
    fn magnet_pull_is_gradual_not_a_teleport() {
        // 60px from center, 10px outside the rect edge: one 16ms tick must
        // drift slightly inward, never jump to the center — and the outline
        // stays off until arrival.
        let mut c = Cursor::new();
        let items = [link(1, 100.0, 100.0)];
        c.x = 210.0;
        c.y = 115.0;
        c.advance(0.0, 0.0, 0.016, &items);
        assert_eq!(c.attached, None);
        assert!(c.x < 210.0 && c.x > 200.0, "glides, not snaps: x={}", c.x);
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
    fn firm_push_breaks_free_at_once_with_kick() {
        let mut c = Cursor::new();
        let items = [link(1, 100.0, 100.0)];
        c.x = 150.0;
        c.y = 115.0;
        c.advance(0.0, 0.0, 0.016, &items);
        assert_eq!(c.attached, Some(1));
        // One firm tick: detached immediately, moving outward, suppressed
        // from instant re-capture while still inside the hold pad.
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
    fn release_to_neutral_lifts_suppression_and_redocks() {
        let mut c = Cursor::new();
        let items = [link(1, 100.0, 100.0)];
        c.x = 150.0;
        c.y = 115.0;
        c.advance(0.0, 0.0, 0.016, &items);
        assert_eq!(c.attached, Some(1));
        // Push off but don't leave: suppressed while held.
        c.advance(1.0, 0.0, 0.016, &items);
        assert_eq!(c.attached, None);
        assert_eq!(c.no_dock, Some(1));
        // Let go without leaving: suppression lifts, gravity re-docks.
        c.advance(0.0, 0.0, 0.016, &items);
        assert_eq!(c.no_dock, None);
        for _ in 0..60 {
            c.advance(0.0, 0.0, 0.016, &items);
            if c.attached.is_some() {
                break;
            }
        }
        assert_eq!(c.attached, Some(1));
    }

    #[test]
    fn well_edge_creep_never_stalls() {
        // 5px inside the well edge: pull must beat the friction cutoff so
        // the point visibly glides in instead of hovering forever.
        let mut c = Cursor::new();
        let items = [link(1, 100.0, 100.0)];
        c.x = 245.0;
        c.y = 115.0;
        let start_x = c.x;
        for _ in 0..60 {
            c.advance(0.0, 0.0, 0.016, &items);
        }
        assert!(
            c.x < start_x - 10.0,
            "edge must glide inward, x={} started={}",
            c.x,
            start_x
        );
    }

    #[test]
    fn suppression_lifts_after_leaving_and_redocks() {
        let mut c = Cursor::new();
        let items = [link(1, 100.0, 100.0)];
        c.x = 150.0;
        c.y = 115.0;
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
        // Full deflection away: breaks the hold, then outruns the well.
        for _ in 0..120 {
            c.advance(1.0, 0.0, 0.016, &items);
        }
        assert_eq!(c.attached, None, "full stick must escape the magnet");
        assert!(c.x > 250.0, "escaped past the gravity radius, x={}", c.x);
    }

    #[test]
    fn weak_stick_stays_captured() {
        // A gentle 10% push cannot beat the pull near the center.
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
    fn click_point_uses_well_before_dock_then_free_point() {
        // Falling into the well but outside the acquire pad: click aims at
        // the well center so X stays useful during the approach.
        let mut c = Cursor::new();
        let items = [link(7, 100.0, 200.0)];
        c.x = 240.0;
        c.y = 215.0;
        c.advance(0.0, 0.0, 0.016, &items);
        assert_eq!(c.attached, None);
        assert_eq!(c.click_point(&items), (150.0, 215.0));
        // Nothing in range: the free point.
        c.x = 500.0;
        c.y = 500.0;
        c.vx = 0.0;
        c.vy = 0.0;
        c.advance(0.0, 0.0, 0.016, &items);
        assert_eq!(c.click_point(&items), (500.0, 500.0));
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
