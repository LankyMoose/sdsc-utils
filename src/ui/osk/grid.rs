//! Grid controller keyboard: D-pad focus + Cross activate fallback mode.

use super::{Dir, Layer};

/// One grid key. Character keys commit text; the rest are control actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GridKey {
    Char(char),
    Space,
    Backspace,
    Enter,
    Layer,
    Close,
}

const ALPHA: &[&[GridKey]] = &[
    &[
        GridKey::Char('q'),
        GridKey::Char('w'),
        GridKey::Char('e'),
        GridKey::Char('r'),
        GridKey::Char('t'),
        GridKey::Char('y'),
        GridKey::Char('u'),
        GridKey::Char('i'),
        GridKey::Char('o'),
        GridKey::Char('p'),
    ],
    &[
        GridKey::Char('a'),
        GridKey::Char('s'),
        GridKey::Char('d'),
        GridKey::Char('f'),
        GridKey::Char('g'),
        GridKey::Char('h'),
        GridKey::Char('j'),
        GridKey::Char('k'),
        GridKey::Char('l'),
    ],
    &[
        GridKey::Char('z'),
        GridKey::Char('x'),
        GridKey::Char('c'),
        GridKey::Char('v'),
        GridKey::Char('b'),
        GridKey::Char('n'),
        GridKey::Char('m'),
    ],
    &[
        GridKey::Layer,
        GridKey::Space,
        GridKey::Backspace,
        GridKey::Enter,
        GridKey::Close,
    ],
];

const SYMBOL: &[&[GridKey]] = &[
    &[
        GridKey::Char('1'),
        GridKey::Char('2'),
        GridKey::Char('3'),
        GridKey::Char('4'),
        GridKey::Char('5'),
        GridKey::Char('6'),
        GridKey::Char('7'),
        GridKey::Char('8'),
        GridKey::Char('9'),
        GridKey::Char('0'),
    ],
    &[
        GridKey::Char('-'),
        GridKey::Char('/'),
        GridKey::Char(':'),
        GridKey::Char(';'),
        GridKey::Char('('),
        GridKey::Char(')'),
        GridKey::Char('$'),
        GridKey::Char('&'),
        GridKey::Char('@'),
        GridKey::Char('"'),
    ],
    &[
        GridKey::Char('.'),
        GridKey::Char(','),
        GridKey::Char('?'),
        GridKey::Char('!'),
        GridKey::Char('\''),
        GridKey::Char('#'),
        GridKey::Char('%'),
        GridKey::Char('+'),
        GridKey::Char('='),
    ],
    &[
        GridKey::Layer,
        GridKey::Space,
        GridKey::Backspace,
        GridKey::Enter,
        GridKey::Close,
    ],
];

#[derive(Debug, Clone, Default)]
pub struct GridKeyboard {
    pub focus: usize,
    pub layer: Layer,
}

impl GridKeyboard {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn reset(&mut self) {
        self.focus = 0;
    }

    pub fn rows(&self) -> &'static [&'static [GridKey]] {
        match self.layer {
            Layer::Alpha => ALPHA,
            Layer::Symbol => SYMBOL,
        }
    }

    pub fn key_count(&self) -> usize {
        self.rows().iter().map(|row| row.len()).sum()
    }

    pub fn focus_key(&self) -> GridKey {
        self.rows()
            .iter()
            .flat_map(|row| row.iter().copied())
            .nth(self.focus)
            .unwrap_or(GridKey::Close)
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

impl GridKey {
    pub fn label(self, layer: Layer) -> String {
        match self {
            GridKey::Char(ch) => ch.to_string(),
            GridKey::Space => "Space".to_string(),
            GridKey::Backspace => "Bksp".to_string(),
            GridKey::Enter => "Enter".to_string(),
            GridKey::Layer => match layer {
                Layer::Alpha => "123".to_string(),
                Layer::Symbol => "abc".to_string(),
            },
            GridKey::Close => "Close".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn focus_wraps_rows_and_layers_stay_in_range() {
        let mut grid = GridKeyboard::new();
        assert_eq!(grid.focus_key(), GridKey::Char('q'));
        grid.move_focus(Dir::Left);
        assert_eq!(grid.focus_key(), GridKey::Char('p'));
        grid.move_focus(Dir::Down);
        assert_eq!(grid.focus_key(), GridKey::Char('l'));
        grid.move_focus(Dir::Down);
        assert_eq!(grid.focus_key(), GridKey::Char('m'));
        grid.move_focus(Dir::Down);
        assert_eq!(grid.focus_key(), GridKey::Close);
        grid.move_focus(Dir::Up);
        assert_eq!(grid.focus_key(), GridKey::Char('b'));
        grid.toggle_layer();
        assert_eq!(grid.layer, Layer::Symbol);
        assert!(grid.focus < grid.key_count());
        assert_eq!(GridKey::Layer.label(grid.layer), "abc");
    }

    #[test]
    fn reset_returns_to_the_first_key() {
        let mut grid = GridKeyboard::new();
        grid.move_focus(Dir::Down);
        grid.move_focus(Dir::Right);
        assert_ne!(grid.focus_key(), GridKey::Char('q'));
        grid.reset();
        assert_eq!(grid.focus_key(), GridKey::Char('q'));
    }
}
