//! Dual QWERTY (DQ) controller keyboard.
//!
//! The keyboard is split into the left and right halves of a QWERTY layout.
//! Each half keeps a persistent cursor; the left stick moves the left cursor
//! and the right stick moves the right cursor. L2/R2 commit the selected key.
//! This is the Microsoft Research DQ interaction, without gesture thresholds
//! or release-to-type: the stick is navigation and the trigger is the key.

use super::{Dir, Layer};

pub const ROWS: usize = 3;
pub const COLS: usize = 5;

const ALPHA_LEFT: [[char; COLS]; ROWS] = [
    ['q', 'w', 'e', 'r', 't'],
    ['a', 's', 'd', 'f', 'g'],
    ['z', 'x', 'c', 'v', 'b'],
];

const ALPHA_RIGHT: [[char; COLS]; ROWS] = [
    ['y', 'u', 'i', 'o', 'p'],
    ['h', 'j', 'k', 'l', ';'],
    ['n', 'm', ',', '.', '/'],
];

const SYMBOL_LEFT: [[char; COLS]; ROWS] = [
    ['1', '2', '3', '4', '5'],
    ['-', '/', ':', ';', '('],
    ['.', ',', '?', '!', '\''],
];

const SYMBOL_RIGHT: [[char; COLS]; ROWS] = [
    ['6', '7', '8', '9', '0'],
    [')', '$', '&', '@', '"'],
    ['#', '%', '+', '=', '~'],
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Half {
    Left,
    Right,
}

/// A cursor inside one half's 3x5 key grid.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Cursor {
    pub row: usize,
    pub col: usize,
}

impl Cursor {
    /// Home row, middle column: D on the left half and K on the right.
    pub const HOME: Self = Self { row: 1, col: 2 };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DualView {
    pub left: Cursor,
    pub right: Cursor,
    pub layer: Layer,
}

/// Pure DQ state: layers and one persistent cursor per half.
#[derive(Debug, Clone)]
pub struct DualKeyboard {
    layer: Layer,
    left: Cursor,
    right: Cursor,
}

impl Default for DualKeyboard {
    fn default() -> Self {
        Self {
            layer: Layer::Alpha,
            left: Cursor::HOME,
            right: Cursor::HOME,
        }
    }
}

impl DualKeyboard {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn layer(&self) -> Layer {
        self.layer
    }

    pub fn view(&self) -> DualView {
        DualView {
            left: self.left,
            right: self.right,
            layer: self.layer,
        }
    }

    pub fn cursor(&self, half: Half) -> Cursor {
        match half {
            Half::Left => self.left,
            Half::Right => self.right,
        }
    }

    pub fn rows(&self, half: Half) -> &'static [[char; COLS]; ROWS] {
        match (self.layer, half) {
            (Layer::Alpha, Half::Left) => &ALPHA_LEFT,
            (Layer::Alpha, Half::Right) => &ALPHA_RIGHT,
            (Layer::Symbol, Half::Left) => &SYMBOL_LEFT,
            (Layer::Symbol, Half::Right) => &SYMBOL_RIGHT,
        }
    }

    /// Selected character for a half. The cursor is clamped to the grid, so
    /// indexing cannot go out of bounds.
    pub fn key(&self, half: Half) -> char {
        let cursor = self.cursor(half);
        self.rows(half)[cursor.row][cursor.col]
    }

    /// Move one half's cursor. Edges clamp; they do not wrap into another row.
    pub fn move_cursor(&mut self, half: Half, direction: Dir) {
        let cursor = match half {
            Half::Left => &mut self.left,
            Half::Right => &mut self.right,
        };
        match direction {
            Dir::Up => cursor.row = cursor.row.saturating_sub(1),
            Dir::Down => cursor.row = (cursor.row + 1).min(ROWS - 1),
            Dir::Left => cursor.col = cursor.col.saturating_sub(1),
            Dir::Right => cursor.col = (cursor.col + 1).min(COLS - 1),
        }
    }

    /// Alpha <-> symbols. Cursor positions persist across layers.
    pub fn toggle_layer(&mut self) {
        self.layer = match self.layer {
            Layer::Alpha => Layer::Symbol,
            Layer::Symbol => Layer::Alpha,
        };
    }

    pub fn reset(&mut self) {
        self.layer = Layer::Alpha;
        self.left = Cursor::HOME;
        self.right = Cursor::HOME;
    }
}

/// Rising-edge latch for the two triggers. Holding a trigger types once.
#[derive(Debug, Clone, Copy, Default)]
pub struct TriggerLatch {
    l2: bool,
    r2: bool,
}

impl TriggerLatch {
    /// Returns the half whose trigger went down this sample (left first).
    pub fn update(&mut self, l2: bool, r2: bool) -> Vec<Half> {
        let mut out = Vec::new();
        if l2 && !self.l2 {
            out.push(Half::Left);
        }
        if r2 && !self.r2 {
            out.push(Half::Right);
        }
        self.l2 = l2;
        self.r2 = r2;
        out
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_on_home_keys() {
        let kb = DualKeyboard::new();
        assert_eq!(kb.key(Half::Left), 'd');
        assert_eq!(kb.key(Half::Right), 'k');
        assert_eq!(kb.view().left, Cursor { row: 1, col: 2 });
        assert_eq!(kb.view().right, Cursor { row: 1, col: 2 });
    }

    #[test]
    fn halves_move_independently_and_clamp() {
        let mut kb = DualKeyboard::new();
        kb.move_cursor(Half::Left, Dir::Left);
        kb.move_cursor(Half::Left, Dir::Up);
        kb.move_cursor(Half::Right, Dir::Right);
        kb.move_cursor(Half::Right, Dir::Down);
        assert_eq!(kb.key(Half::Left), 'w');
        assert_eq!(kb.key(Half::Right), '.');
        for _ in 0..8 {
            kb.move_cursor(Half::Left, Dir::Left);
            kb.move_cursor(Half::Left, Dir::Up);
            kb.move_cursor(Half::Right, Dir::Right);
            kb.move_cursor(Half::Right, Dir::Down);
        }
        // Clamped, not wrapped.
        assert_eq!(kb.key(Half::Left), 'q');
        assert_eq!(kb.key(Half::Right), '/');
        assert_eq!(kb.view().left, Cursor { row: 0, col: 0 });
        assert_eq!(kb.view().right, Cursor { row: 2, col: 4 });
    }

    #[test]
    fn symbol_layer_keeps_cursor_position() {
        let mut kb = DualKeyboard::new();
        kb.move_cursor(Half::Left, Dir::Up);
        kb.move_cursor(Half::Left, Dir::Right);
        kb.toggle_layer();
        assert_eq!(kb.layer(), Layer::Symbol);
        assert_eq!(kb.view().left, Cursor { row: 0, col: 3 });
        assert_eq!(kb.key(Half::Left), '4');
        assert_eq!(kb.key(Half::Right), '&');
        kb.toggle_layer();
        assert_eq!(kb.layer(), Layer::Alpha);
        assert_eq!(kb.key(Half::Left), 'r');
        assert_eq!(kb.key(Half::Right), 'k');
    }

    #[test]
    fn reset_returns_to_alpha_and_home() {
        let mut kb = DualKeyboard::new();
        kb.toggle_layer();
        kb.move_cursor(Half::Left, Dir::Down);
        kb.move_cursor(Half::Right, Dir::Right);
        kb.reset();
        assert_eq!(kb.layer(), Layer::Alpha);
        assert_eq!(kb.key(Half::Left), 'd');
        assert_eq!(kb.key(Half::Right), 'k');
    }

    #[test]
    fn trigger_latch_fires_on_press_only() {
        let mut latch = TriggerLatch::default();
        assert_eq!(latch.update(true, false), vec![Half::Left]);
        assert!(latch.update(true, false).is_empty());
        assert!(latch.update(false, false).is_empty());
        assert_eq!(latch.update(false, true), vec![Half::Right]);
        assert!(latch.update(false, true).is_empty());
        assert!(latch.update(false, false).is_empty());
    }

    #[test]
    fn simultaneous_triggers_commit_left_then_right() {
        let mut latch = TriggerLatch::default();
        assert_eq!(latch.update(true, true), vec![Half::Left, Half::Right]);
        assert!(latch.update(true, true).is_empty());
        latch.reset();
        assert_eq!(latch.update(true, true), vec![Half::Left, Half::Right]);
    }

    #[test]
    fn full_alpha_map_is_split_qwerty() {
        let kb = DualKeyboard::new();
        assert_eq!(
            kb.rows(Half::Left),
            &[
                ['q', 'w', 'e', 'r', 't'],
                ['a', 's', 'd', 'f', 'g'],
                ['z', 'x', 'c', 'v', 'b'],
            ]
        );
        assert_eq!(
            kb.rows(Half::Right),
            &[
                ['y', 'u', 'i', 'o', 'p'],
                ['h', 'j', 'k', 'l', ';'],
                ['n', 'm', ',', '.', '/'],
            ]
        );
    }
}
