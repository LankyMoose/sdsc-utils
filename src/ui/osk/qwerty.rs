//! QWERTY controller keyboard: D-pad focus + Cross activate.

use super::{Dir, Layer};

/// One QWERTY key. Character keys commit text; the rest are control actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OskKey {
    Char(char),
    Space,
    Backspace,
    Enter,
    Layer,
    Close,
}

const ALPHA: &[&[OskKey]] = &[
    &[
        OskKey::Char('q'),
        OskKey::Char('w'),
        OskKey::Char('e'),
        OskKey::Char('r'),
        OskKey::Char('t'),
        OskKey::Char('y'),
        OskKey::Char('u'),
        OskKey::Char('i'),
        OskKey::Char('o'),
        OskKey::Char('p'),
    ],
    &[
        OskKey::Char('a'),
        OskKey::Char('s'),
        OskKey::Char('d'),
        OskKey::Char('f'),
        OskKey::Char('g'),
        OskKey::Char('h'),
        OskKey::Char('j'),
        OskKey::Char('k'),
        OskKey::Char('l'),
    ],
    &[
        OskKey::Char('z'),
        OskKey::Char('x'),
        OskKey::Char('c'),
        OskKey::Char('v'),
        OskKey::Char('b'),
        OskKey::Char('n'),
        OskKey::Char('m'),
    ],
    &[
        OskKey::Layer,
        OskKey::Space,
        OskKey::Backspace,
        OskKey::Enter,
        OskKey::Close,
    ],
];

const SYMBOL: &[&[OskKey]] = &[
    &[
        OskKey::Char('1'),
        OskKey::Char('2'),
        OskKey::Char('3'),
        OskKey::Char('4'),
        OskKey::Char('5'),
        OskKey::Char('6'),
        OskKey::Char('7'),
        OskKey::Char('8'),
        OskKey::Char('9'),
        OskKey::Char('0'),
    ],
    &[
        OskKey::Char('-'),
        OskKey::Char('/'),
        OskKey::Char(':'),
        OskKey::Char(';'),
        OskKey::Char('('),
        OskKey::Char(')'),
        OskKey::Char('$'),
        OskKey::Char('&'),
        OskKey::Char('@'),
        OskKey::Char('"'),
    ],
    &[
        OskKey::Char('.'),
        OskKey::Char(','),
        OskKey::Char('?'),
        OskKey::Char('!'),
        OskKey::Char('\''),
        OskKey::Char('#'),
        OskKey::Char('%'),
        OskKey::Char('+'),
        OskKey::Char('='),
    ],
    &[
        OskKey::Layer,
        OskKey::Space,
        OskKey::Backspace,
        OskKey::Enter,
        OskKey::Close,
    ],
];

#[derive(Debug, Clone, Default)]
pub struct QwertyKeyboard {
    pub focus: usize,
    pub layer: Layer,
}

impl QwertyKeyboard {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn reset(&mut self) {
        self.focus = 0;
    }

    pub fn rows(&self) -> &'static [&'static [OskKey]] {
        match self.layer {
            Layer::Alpha => ALPHA,
            Layer::Symbol => SYMBOL,
        }
    }

    pub fn key_count(&self) -> usize {
        self.rows().iter().map(|row| row.len()).sum()
    }

    pub fn focus_key(&self) -> OskKey {
        self.rows()
            .iter()
            .flat_map(|row| row.iter().copied())
            .nth(self.focus)
            .unwrap_or(OskKey::Close)
    }

    pub fn move_focus(&mut self, direction: Dir) {
        let rows = self.rows();
        let mut start = 0;
        let mut position = (0usize, 0usize);
        for (r, row) in rows.iter().enumerate() {
            if self.focus < start + row.len() {
                position = (r, self.focus - start);
                break;
            }
            start += row.len();
        }
        let (mut row, mut col) = position;
        match direction {
            Dir::Left => {
                let len = rows[row].len();
                col = if col == 0 { len - 1 } else { col - 1 };
            }
            Dir::Right => {
                let len = rows[row].len();
                col = (col + 1) % len;
            }
            Dir::Up => {
                row = row.saturating_sub(1);
                col = col.min(rows[row].len() - 1);
            }
            Dir::Down => {
                row = (row + 1).min(rows.len() - 1);
                col = col.min(rows[row].len() - 1);
            }
        }
        self.focus = rows.iter().take(row).map(|r| r.len()).sum::<usize>() + col;
    }

    pub fn toggle_layer(&mut self) {
        self.layer = match self.layer {
            Layer::Alpha => Layer::Symbol,
            Layer::Symbol => Layer::Alpha,
        };
        self.focus = self.focus.min(self.key_count().saturating_sub(1));
    }
}

impl OskKey {
    pub fn label(self, layer: Layer) -> String {
        match self {
            OskKey::Char(ch) => ch.to_string(),
            OskKey::Space => "Space".to_string(),
            OskKey::Backspace => "Bksp".to_string(),
            OskKey::Enter => "Enter".to_string(),
            OskKey::Layer => match layer {
                Layer::Alpha => "123".to_string(),
                Layer::Symbol => "abc".to_string(),
            },
            OskKey::Close => "Close".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn focus_wraps_rows_and_layers_stay_in_range() {
        let mut qwerty = QwertyKeyboard::new();
        assert_eq!(qwerty.focus_key(), OskKey::Char('q'));
        qwerty.move_focus(Dir::Left);
        assert_eq!(qwerty.focus_key(), OskKey::Char('p'));
        qwerty.move_focus(Dir::Down);
        assert_eq!(qwerty.focus_key(), OskKey::Char('l'));
        qwerty.move_focus(Dir::Down);
        assert_eq!(qwerty.focus_key(), OskKey::Char('m'));
        qwerty.move_focus(Dir::Down);
        assert_eq!(qwerty.focus_key(), OskKey::Close);
        qwerty.move_focus(Dir::Up);
        assert_eq!(qwerty.focus_key(), OskKey::Char('b'));
        qwerty.toggle_layer();
        assert_eq!(qwerty.layer, Layer::Symbol);
        assert!(qwerty.focus < qwerty.key_count());
        assert_eq!(OskKey::Layer.label(qwerty.layer), "abc");
    }

    #[test]
    fn reset_returns_to_the_first_key() {
        let mut qwerty = QwertyKeyboard::new();
        qwerty.move_focus(Dir::Down);
        qwerty.move_focus(Dir::Right);
        assert_ne!(qwerty.focus_key(), OskKey::Char('q'));
        qwerty.reset();
        assert_eq!(qwerty.focus_key(), OskKey::Char('q'));
    }
}
