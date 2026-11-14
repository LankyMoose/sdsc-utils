//! Virtual cursor for controller-first browsing.
//!
//! The pad owns a cursor over the rendered page: the left stick (and D-pad
//! nudges) move a free point, and it magnetically attaches to the nearest
//! focusable element. Rendering draws a circle while free and wraps the
//! attached element's shape once docked. Cross clicks the attached element
//! (or the free point).
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

/// Snap radius in page pixels: magnetism only engages this close to a
/// focusable. Outside it the stick drives free-form.
pub const SNAP_RADIUS: f32 = 100.0;
/// D-pad nudge step in page pixels.
pub const NUDGE_STEP: f32 = 32.0;
/// Left-stick cursor acceleration at full deflection, px/s^2.
pub const CURSOR_ACCEL: f32 = 7000.0;
/// Left-stick cursor top speed, px/s.
pub const CURSOR_MAX_SPEED: f32 = 1400.0;
/// Velocity decay rate (exponential friction, per second). The cursor glides
/// a little after release instead of stopping dead.
pub const CURSOR_FRICTION: f32 = 7.0;
/// Magnetic pull rate (exponential approach per second) once docked: the
/// cursor glides to the element's center instead of teleporting.
pub const MAGNET_PULL_PER_SEC: f32 = 14.0;
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

    /// Analog velocity step: the stick accelerates a velocity vector and
    /// friction decays it, so the cursor glides full-360° with momentum and
    /// settles instead of stopping dead. Detaches on manual input; re-snaps
    /// (magnetic mode) and glides to the dock afterwards.
    pub fn advance(&mut self, lx: f32, ly: f32, dt_secs: f32, options: &[Focusable]) {
        if lx != 0.0 || ly != 0.0 {
            self.attached = None;
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
            self.y = (self.y + self.vy * dt_secs).clamp(0.0, VIEWPORT_H);
        }
        if self.magnet {
            self.snap(options);
            self.glide(dt_secs, options);
        }
    }

    /// Glide the docked point toward its element center (magnetic pull).
    /// No-op when free: the stick already placed the point this tick.
    pub fn glide(&mut self, dt_secs: f32, options: &[Focusable]) {
        let Some(id) = self.attached else {
            return;
        };
        let Some(item) = options.iter().find(|o| o.id == id) else {
            return;
        };
        let tx = item.x + item.w / 2.0;
        let ty = item.y + item.h / 2.0;
        let t = 1.0 - (-MAGNET_PULL_PER_SEC * dt_secs).exp();
        self.x += (tx - self.x) * t;
        self.y += (ty - self.y) * t;
    }

    /// Nudge for D-pad taps, then snap. Discrete step: kills velocity so a
    /// tap never inherits drift.
    pub fn nudge(&mut self, dir: JumpDir, options: &[Focusable]) {
        let (dx, dy) = match dir {
            JumpDir::Up => (0.0, -NUDGE_STEP),
            JumpDir::Down => (0.0, NUDGE_STEP),
            JumpDir::Left => (-NUDGE_STEP, 0.0),
            JumpDir::Right => (NUDGE_STEP, 0.0),
        };
        self.move_by(dx, dy);
        self.vx = 0.0;
        self.vy = 0.0;
        self.snap(options);
    }

    /// Directional jump to the nearest *other* element roughly in `dir`;
    /// falls back to a nudge when nothing lies that way. Returns true on a
    /// jump. D-pad always snaps — this ignores the magnet toggle.
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
            self.attach_to(id, options);
            true
        } else {
            self.nudge(dir, options);
            false
        }
    }

    /// Attach when the free point rests near an element. Keeps the current
    /// attachment while the point stays inside its expanded bounds.
    pub fn snap(&mut self, options: &[Focusable]) {
        if let Some(id) = self.attached
            && let Some(item) = options.iter().find(|o| o.id == id)
            && self.inside_expanded(item, SNAP_RADIUS)
        {
            return;
        }
        self.attached = None;
        let mut best: Option<(u64, f32)> = None;
        for item in options {
            let d = self.edge_distance(item);
            if d <= SNAP_RADIUS && best.is_none_or(|(_, bd)| d < bd) {
                best = Some((item.id, d));
            }
        }
        self.attached = best.map(|(id, _)| id);
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

    /// Point to click: attached element center, else the free cursor.
    pub fn click_point(&self, options: &[Focusable]) -> (f32, f32) {
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

    fn attach_to(&mut self, id: u64, options: &[Focusable]) {
        if let Some(item) = options.iter().find(|o| o.id == id) {
            self.x = (item.x + item.w / 2.0).clamp(0.0, VIEWPORT_W);
            self.y = (item.y + item.h / 2.0).clamp(0.0, VIEWPORT_H);
            self.attached = Some(id);
            // Teleport, not flick: a jump carries no momentum.
            self.vx = 0.0;
            self.vy = 0.0;
        }
    }

    fn inside_expanded(&self, item: &Focusable, pad: f32) -> bool {
        self.x >= item.x - pad
            && self.x <= item.x + item.w + pad
            && self.y >= item.y - pad
            && self.y <= item.y + item.h + pad
    }

    fn edge_distance(&self, item: &Focusable) -> f32 {
        let dx = if self.x < item.x {
            item.x - self.x
        } else if self.x > item.x + item.w {
            self.x - (item.x + item.w)
        } else {
            0.0
        };
        let dy = if self.y < item.y {
            item.y - self.y
        } else if self.y > item.y + item.h {
            self.y - (item.y + item.h)
        } else {
            0.0
        };
        dx.hypot(dy)
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
        c.x = 235.0;
        c.y = 115.0;
        c.snap(&items);
        assert_eq!(c.attached, Some(2));
    }

    #[test]
    fn jump_goes_directionally_and_falls_back_to_nudge() {
        let mut c = Cursor::new();
        let items = [link(1, 100.0, 100.0), link(2, 400.0, 100.0)];
        c.x = 110.0;
        c.y = 115.0;
        c.snap(&items);
        assert_eq!(c.attached, Some(1));
        assert!(c.jump(JumpDir::Right, &items));
        assert_eq!(c.attached, Some(2));
        // Nothing above: nudge instead, no attach.
        let before = (c.x, c.y);
        assert!(!c.jump(JumpDir::Up, &[]));
        assert!(c.y < before.1);
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
    fn magnet_on_glides_toward_dock() {
        let mut c = Cursor::new();
        let items = [link(1, 100.0, 100.0)];
        c.x = 150.0;
        c.y = 115.0;
        c.advance(0.0, 0.0, 0.016, &items);
        assert_eq!(c.attached, Some(1));
        let before = (150.0 - c.x).abs();
        c.advance(0.0, 0.0, 0.05, &items);
        // Center is (150, 115): already centered, stays put.
        assert!((c.x - 150.0).abs() <= before);
        // From the edge: glides inward.
        c.x = 210.0;
        c.attached = None;
        c.advance(0.0, 0.0, 0.05, &items);
        assert_eq!(c.attached, Some(1));
        assert!(c.x < 210.0);
    }

    #[test]
    fn click_point_prefers_attached_center() {
        let mut c = Cursor::new();
        let items = [link(7, 100.0, 200.0)];
        c.x = 110.0;
        c.y = 210.0;
        c.snap(&items);
        assert_eq!(c.click_point(&items), (150.0, 215.0));
        c.attached = None;
        assert_eq!(c.click_point(&items), (110.0, 210.0));
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
