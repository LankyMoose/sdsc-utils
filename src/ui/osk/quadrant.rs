//! Quadrant controller keyboard: split-stick state machine and key layouts.
//!
//! Pure input logic only. Each stick drives one half of the keyboard: crossing
//! `ARM` latches a quadrant, sub/outer hysteresis picks a key, and dropping
//! below `RELEASE` commits exactly one key. A commit, cancel, layer change, or
//! reset requires the stick to return below `NEUTRAL` before arming again.

const ARM: f32 = 0.55;
const RELEASE: f32 = 0.22;
const SUB_ENTER: f32 = 0.45;
const SUB_LEAVE: f32 = 0.32;
const OUTER_ENTER: f32 = 0.82;
const OUTER_LEAVE: f32 = 0.70;
const NEUTRAL: f32 = 0.11;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Half {
    Left,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Layer {
    #[default]
    Alpha,
    Symbol,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Quadrant {
    Left,
    Right,
    Up,
    Down,
    Home,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Output {
    Text(char),
    Space,
    Backspace,
    Enter,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HalfView {
    pub armed: bool,
    pub quadrant: Option<Quadrant>,
    pub preview: Option<char>,
}

/// One half's latched quadrant/slot and neutral gate.
#[derive(Debug, Clone, Copy, Default)]
struct HalfState {
    armed: bool,
    quadrant: Option<Quadrant>,
    slot: Slot,
    preview: Option<char>,
    gate: bool,
}

impl HalfState {
    fn gated() -> Self {
        Self {
            gate: true,
            ..Self::default()
        }
    }

    fn view(&self) -> HalfView {
        HalfView {
            armed: self.armed,
            quadrant: self.quadrant,
            preview: self.preview,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Slot {
    #[default]
    Base,
    Up,
    Down,
    Outer,
    OuterLeft,
    OuterRight,
}

#[derive(Debug, Clone, Default)]
pub struct QuadrantKeyboard {
    layer: Layer,
    left: HalfState,
    right: HalfState,
}

impl QuadrantKeyboard {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn layer(&self) -> Layer {
        self.layer
    }

    /// Switch layers and reset both halves (neutral required before re-arming).
    pub fn toggle_layer(&mut self) {
        self.layer = match self.layer {
            Layer::Alpha => Layer::Symbol,
            Layer::Symbol => Layer::Alpha,
        };
        self.reset();
    }

    /// Both halves idle; neutral required before re-arming.
    pub fn reset(&mut self) {
        self.left = HalfState::gated();
        self.right = HalfState::gated();
    }

    /// Feed one pad sample (x right+, y down+). Returns commits in half order
    /// (left first), at most one per half per call.
    pub fn update(&mut self, left: (f32, f32), right: (f32, f32)) -> Vec<Output> {
        let layer = self.layer;
        let mut out = Vec::new();
        if let Some(output) = update_half(Half::Left, &mut self.left, layer, left) {
            out.push(output);
        }
        if let Some(output) = update_half(Half::Right, &mut self.right, layer, right) {
            out.push(output);
        }
        out
    }

    /// L3/R3 home key: returns the character and resets that half.
    pub fn home_key(&mut self, half: Half) -> Output {
        let key = layer_char(self.layer, physical_key(half, Quadrant::Home, Slot::Base));
        match half {
            Half::Left => self.left = HalfState::gated(),
            Half::Right => self.right = HalfState::gated(),
        }
        Output::Text(key)
    }

    /// UI preview (no state change).
    pub fn view(&self) -> (HalfView, HalfView) {
        (self.left.view(), self.right.view())
    }
}

fn update_half(
    half: Half,
    state: &mut HalfState,
    layer: Layer,
    (x, y): (f32, f32),
) -> Option<Output> {
    let radial = x.hypot(y);

    if state.armed {
        if radial < RELEASE {
            let output = state.preview.map(Output::Text);
            *state = HalfState::gated();
            return output;
        }
        let Some(quadrant) = state.quadrant else {
            *state = HalfState::gated();
            return None;
        };
        if dominant_quadrant(x, y) != quadrant {
            // Left the latched quadrant without releasing: cancel, no commit.
            *state = HalfState::gated();
            return None;
        }
        state.slot = advance_slot(quadrant, state.slot, x, y);
        state.preview = Some(layer_char(layer, physical_key(half, quadrant, state.slot)));
        return None;
    }

    if state.gate {
        if radial < NEUTRAL {
            state.gate = false;
        }
        return None;
    }

    if radial < ARM {
        return None;
    }

    let quadrant = dominant_quadrant(x, y);
    let slot = advance_slot(quadrant, Slot::Base, x, y);
    state.armed = true;
    state.quadrant = Some(quadrant);
    state.slot = slot;
    state.preview = Some(layer_char(layer, physical_key(half, quadrant, slot)));
    None
}

fn dominant_quadrant(x: f32, y: f32) -> Quadrant {
    if x.abs() >= y.abs() {
        if x >= 0.0 {
            Quadrant::Right
        } else {
            Quadrant::Left
        }
    } else if y >= 0.0 {
        Quadrant::Down
    } else {
        Quadrant::Up
    }
}

fn advance_slot(quadrant: Quadrant, previous: Slot, x: f32, y: f32) -> Slot {
    match quadrant {
        Quadrant::Left | Quadrant::Right => {
            if y <= -SUB_ENTER {
                Slot::Up
            } else if y >= SUB_ENTER {
                Slot::Down
            } else {
                match previous {
                    Slot::Up if y < -SUB_LEAVE => Slot::Up,
                    Slot::Down if y > SUB_LEAVE => Slot::Down,
                    Slot::Outer if x.abs() >= OUTER_LEAVE => Slot::Outer,
                    _ if x.abs() >= OUTER_ENTER => Slot::Outer,
                    _ => Slot::Base,
                }
            }
        }
        Quadrant::Up | Quadrant::Down => {
            if x <= -SUB_ENTER {
                Slot::OuterLeft
            } else if x >= SUB_ENTER {
                Slot::OuterRight
            } else {
                match previous {
                    Slot::OuterLeft if x < -SUB_LEAVE => Slot::OuterLeft,
                    Slot::OuterRight if x > SUB_LEAVE => Slot::OuterRight,
                    _ => Slot::Base,
                }
            }
        }
        Quadrant::Home => Slot::Base,
    }
}

/// Physical (Alpha) key for a half/quadrant/slot. The Symbol layer maps these
/// keys through [`SYMBOLS`], so both layers share one state machine.
fn physical_key(half: Half, quadrant: Quadrant, slot: Slot) -> char {
    match (half, quadrant, slot) {
        (Half::Left, Quadrant::Right, Slot::Base) => 'F',
        (Half::Left, Quadrant::Right, Slot::Up) => 'T',
        (Half::Left, Quadrant::Right, Slot::Down) => 'B',
        (Half::Left, Quadrant::Right, Slot::Outer) => 'G',
        (Half::Left, Quadrant::Left, Slot::Base) => 'S',
        (Half::Left, Quadrant::Left, Slot::Up) => 'Q',
        (Half::Left, Quadrant::Left, Slot::Down) => 'Z',
        (Half::Left, Quadrant::Left, Slot::Outer) => 'A',
        (Half::Left, Quadrant::Up, Slot::Base) => 'E',
        (Half::Left, Quadrant::Up, Slot::OuterLeft) => 'W',
        (Half::Left, Quadrant::Up, Slot::OuterRight) => 'R',
        (Half::Left, Quadrant::Down, Slot::Base) => 'C',
        (Half::Left, Quadrant::Down, Slot::OuterLeft) => 'X',
        (Half::Left, Quadrant::Down, Slot::OuterRight) => 'V',
        (Half::Left, Quadrant::Home, _) => 'D',
        (Half::Right, Quadrant::Right, Slot::Base) => 'L',
        (Half::Right, Quadrant::Right, Slot::Up) => 'P',
        (Half::Right, Quadrant::Right, Slot::Down) => '/',
        (Half::Right, Quadrant::Right, Slot::Outer) => ';',
        (Half::Right, Quadrant::Left, Slot::Base) => 'J',
        (Half::Right, Quadrant::Left, Slot::Up) => 'Y',
        (Half::Right, Quadrant::Left, Slot::Down) => 'N',
        (Half::Right, Quadrant::Left, Slot::Outer) => 'H',
        (Half::Right, Quadrant::Up, Slot::Base) => 'I',
        (Half::Right, Quadrant::Up, Slot::OuterLeft) => 'U',
        (Half::Right, Quadrant::Up, Slot::OuterRight) => 'O',
        (Half::Right, Quadrant::Down, Slot::Base) => ',',
        (Half::Right, Quadrant::Down, Slot::OuterLeft) => 'M',
        (Half::Right, Quadrant::Down, Slot::OuterRight) => '.',
        (Half::Right, Quadrant::Home, _) => 'K',
        // advance_slot only produces slots valid for the latched quadrant.
        _ => ' ',
    }
}

/// Symbol layer, keyed by the physical QWERTY key (one table for both layers).
#[rustfmt::skip]
const SYMBOLS: &[(char, char)] = &[
    ('Q', '1'), ('W', '2'), ('E', '3'), ('R', '4'), ('T', '5'),
    ('Y', '6'), ('U', '7'), ('I', '8'), ('O', '9'), ('P', '0'),
    ('A', '-'), ('S', '/'), ('D', ':'), ('F', ';'), ('G', '('),
    ('H', ')'), ('J', '$'), ('K', '&'), ('L', '@'), (';', '"'),
    ('Z', '.'), ('X', ','), ('C', '?'), ('V', '!'), ('B', '\''),
    ('N', '#'), ('M', '%'), (',', '+'), ('.', '='), ('/', '~'),
];

fn layer_char(layer: Layer, key: char) -> char {
    if layer == Layer::Alpha {
        return key.to_ascii_lowercase();
    }
    SYMBOLS
        .iter()
        .find(|&&(alpha, _)| alpha == key)
        .map_or(key, |&(_, symbol)| symbol)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ZERO: (f32, f32) = (0.0, 0.0);

    fn feed(kb: &mut QuadrantKeyboard, left: (f32, f32), right: (f32, f32)) -> Vec<Output> {
        kb.update(left, right)
    }

    /// Settle neutral, arm one half, release, and return the commit(s).
    fn tap(kb: &mut QuadrantKeyboard, half: Half, x: f32, y: f32) -> Vec<Output> {
        let (left, right) = match half {
            Half::Left => ((x, y), ZERO),
            Half::Right => (ZERO, (x, y)),
        };
        assert!(feed(kb, ZERO, ZERO).is_empty());
        assert!(feed(kb, left, right).is_empty());
        feed(kb, ZERO, ZERO)
    }

    fn assert_tap(kb: &mut QuadrantKeyboard, half: Half, at: (f32, f32), expected: char) {
        assert_eq!(tap(kb, half, at.0, at.1), vec![Output::Text(expected)]);
    }

    #[test]
    fn left_cluster_map() {
        let mut kb = QuadrantKeyboard::new();
        assert_tap(&mut kb, Half::Left, (0.8, 0.0), 'f');
        assert_tap(&mut kb, Half::Left, (0.6, -0.5), 't');
        assert_tap(&mut kb, Half::Left, (0.6, 0.5), 'b');
        assert_tap(&mut kb, Half::Left, (0.9, 0.0), 'g');
        assert_tap(&mut kb, Half::Left, (-0.8, 0.0), 's');
        assert_tap(&mut kb, Half::Left, (-0.6, -0.5), 'q');
        assert_tap(&mut kb, Half::Left, (-0.6, 0.5), 'z');
        assert_tap(&mut kb, Half::Left, (-0.9, 0.0), 'a');
        assert_tap(&mut kb, Half::Left, (0.0, -0.8), 'e');
        assert_tap(&mut kb, Half::Left, (-0.5, -0.6), 'w');
        assert_tap(&mut kb, Half::Left, (0.5, -0.6), 'r');
        assert_tap(&mut kb, Half::Left, (0.0, 0.8), 'c');
        assert_tap(&mut kb, Half::Left, (-0.5, 0.6), 'x');
        assert_tap(&mut kb, Half::Left, (0.5, 0.6), 'v');
    }

    #[test]
    fn right_cluster_map() {
        let mut kb = QuadrantKeyboard::new();
        assert_tap(&mut kb, Half::Right, (0.8, 0.0), 'l');
        assert_tap(&mut kb, Half::Right, (0.6, -0.5), 'p');
        assert_tap(&mut kb, Half::Right, (0.6, 0.5), '/');
        assert_tap(&mut kb, Half::Right, (0.9, 0.0), ';');
        assert_tap(&mut kb, Half::Right, (-0.8, 0.0), 'j');
        assert_tap(&mut kb, Half::Right, (-0.6, -0.5), 'y');
        assert_tap(&mut kb, Half::Right, (-0.6, 0.5), 'n');
        assert_tap(&mut kb, Half::Right, (-0.9, 0.0), 'h');
        assert_tap(&mut kb, Half::Right, (0.0, -0.8), 'i');
        assert_tap(&mut kb, Half::Right, (-0.5, -0.6), 'u');
        assert_tap(&mut kb, Half::Right, (0.5, -0.6), 'o');
        assert_tap(&mut kb, Half::Right, (0.0, 0.8), ',');
        assert_tap(&mut kb, Half::Right, (-0.5, 0.6), 'm');
        assert_tap(&mut kb, Half::Right, (0.5, 0.6), '.');
    }

    #[test]
    fn neutral_only_update_is_empty() {
        let mut kb = QuadrantKeyboard::new();
        for _ in 0..3 {
            assert!(feed(&mut kb, ZERO, ZERO).is_empty());
        }
        assert_eq!(kb.view(), (HalfView::default(), HalfView::default()));
    }

    #[test]
    fn release_commits_once_and_rearm_needs_neutral() {
        let mut kb = QuadrantKeyboard::new();
        assert!(feed(&mut kb, (0.8, 0.0), ZERO).is_empty());
        let (left, _) = kb.view();
        assert!(left.armed);
        assert_eq!(left.quadrant, Some(Quadrant::Right));
        assert_eq!(left.preview, Some('f'));

        assert_eq!(feed(&mut kb, ZERO, ZERO), vec![Output::Text('f')]);
        // Still gated: a fresh push must not arm until neutral is seen.
        assert!(feed(&mut kb, (0.8, 0.0), ZERO).is_empty());
        assert!(!kb.view().0.armed);
        assert!(feed(&mut kb, ZERO, ZERO).is_empty());
        assert!(feed(&mut kb, (0.8, 0.0), ZERO).is_empty());
        assert!(kb.view().0.armed);
        assert_eq!(feed(&mut kb, ZERO, ZERO), vec![Output::Text('f')]);
    }

    #[test]
    fn slot_hysteresis_enter_and_leave() {
        let mut kb = QuadrantKeyboard::new();
        assert!(feed(&mut kb, (0.7, 0.0), ZERO).is_empty());
        assert_eq!(kb.view().0.preview, Some('f'));
        // Enters the down subkey, then holds it until below SUB_LEAVE.
        assert!(feed(&mut kb, (0.7, 0.5), ZERO).is_empty());
        assert_eq!(kb.view().0.preview, Some('b'));
        assert!(feed(&mut kb, (0.7, 0.40), ZERO).is_empty());
        assert_eq!(kb.view().0.preview, Some('b'));
        assert!(feed(&mut kb, (0.7, 0.30), ZERO).is_empty());
        assert_eq!(kb.view().0.preview, Some('f'));
        assert_eq!(feed(&mut kb, ZERO, ZERO), vec![Output::Text('f')]);
        let mut kb = QuadrantKeyboard::new();
        assert!(feed(&mut kb, (0.80, 0.0), ZERO).is_empty());
        assert_eq!(kb.view().0.preview, Some('f'));
        assert!(feed(&mut kb, (0.84, 0.0), ZERO).is_empty());
        assert_eq!(kb.view().0.preview, Some('g'));
        assert!(feed(&mut kb, (0.75, 0.0), ZERO).is_empty());
        assert_eq!(kb.view().0.preview, Some('g'));
        assert!(feed(&mut kb, (0.65, 0.0), ZERO).is_empty());
        assert_eq!(kb.view().0.preview, Some('f'));
        assert_eq!(feed(&mut kb, ZERO, ZERO), vec![Output::Text('f')]);
    }

    #[test]
    fn cross_center_change_cancels_and_needs_neutral() {
        let mut kb = QuadrantKeyboard::new();
        assert!(feed(&mut kb, (0.8, 0.0), ZERO).is_empty());
        assert!(feed(&mut kb, (-0.8, 0.0), ZERO).is_empty());
        let (left, _) = kb.view();
        assert!(!left.armed);
        assert_eq!(left.preview, None);
        assert!(feed(&mut kb, (0.8, 0.0), ZERO).is_empty());
        assert!(!kb.view().0.armed);
        assert!(feed(&mut kb, ZERO, ZERO).is_empty());
        assert!(feed(&mut kb, (0.8, 0.0), ZERO).is_empty());
        assert!(kb.view().0.armed);
        assert_eq!(feed(&mut kb, ZERO, ZERO), vec![Output::Text('f')]);
    }

    #[test]
    fn halves_commit_left_first_and_independently() {
        let mut kb = QuadrantKeyboard::new();
        assert!(feed(&mut kb, (0.8, 0.0), (0.8, 0.0)).is_empty());
        assert_eq!(
            feed(&mut kb, ZERO, ZERO),
            vec![Output::Text('f'), Output::Text('l')]
        );
        assert!(feed(&mut kb, ZERO, ZERO).is_empty());
        assert!(feed(&mut kb, (0.8, 0.0), (0.6, -0.5)).is_empty());
        assert_eq!(kb.view().1.preview, Some('p'));
        assert_eq!(feed(&mut kb, ZERO, (0.6, -0.5)), vec![Output::Text('f')]);
        assert!(kb.view().1.armed);
        assert_eq!(feed(&mut kb, ZERO, ZERO), vec![Output::Text('p')]);
    }

    #[test]
    fn home_keys_follow_layer() {
        let mut kb = QuadrantKeyboard::new();
        assert_eq!(kb.home_key(Half::Left), Output::Text('d'));
        assert_eq!(kb.home_key(Half::Right), Output::Text('k'));
        kb.toggle_layer();
        assert_eq!(kb.home_key(Half::Left), Output::Text(':'));
        assert_eq!(kb.home_key(Half::Right), Output::Text('&'));
    }

    #[test]
    fn toggle_layer_resets_and_needs_neutral() {
        let mut kb = QuadrantKeyboard::new();
        assert!(feed(&mut kb, (0.8, 0.0), (0.8, 0.0)).is_empty());
        assert!(kb.view().0.armed && kb.view().1.armed);
        kb.toggle_layer();
        assert_eq!(kb.layer(), Layer::Symbol);
        let (left, right) = kb.view();
        assert!(!left.armed && !right.armed);
        assert!(feed(&mut kb, (0.9, 0.0), (0.9, 0.0)).is_empty());
        assert!(!kb.view().0.armed);
        assert!(feed(&mut kb, ZERO, ZERO).is_empty());
        assert!(feed(&mut kb, (0.9, 0.0), (0.9, 0.0)).is_empty());
        assert!(kb.view().0.armed);
    }

    #[test]
    fn symbol_mapping_spot_checks() {
        let mut kb = QuadrantKeyboard::new();
        kb.toggle_layer();
        assert_eq!(kb.layer(), Layer::Symbol);
        assert_tap(&mut kb, Half::Left, (0.8, 0.0), ';');
        assert_tap(&mut kb, Half::Left, (0.6, -0.5), '5');
        assert_tap(&mut kb, Half::Left, (0.9, 0.0), '(');
        assert_tap(&mut kb, Half::Left, (0.0, -0.8), '3');
        assert_tap(&mut kb, Half::Left, (-0.5, -0.6), '2');
        assert_tap(&mut kb, Half::Left, (0.5, -0.6), '4');
        assert_tap(&mut kb, Half::Right, (0.8, 0.0), '@');
        assert_tap(&mut kb, Half::Right, (0.6, 0.5), '~');
        assert_tap(&mut kb, Half::Right, (0.0, 0.8), '+');
        assert_tap(&mut kb, Half::Right, (-0.5, 0.6), '%');
        assert_tap(&mut kb, Half::Right, (0.5, 0.6), '=');
    }
}
