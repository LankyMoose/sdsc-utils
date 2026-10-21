//! Compact browse lists (Games + Controllers) + section/sticky/scroll-id helpers.
//!
//! Behavior-preserving split from [`crate::ui::start::view`]: list bodies live
//! here, shared scroll math / chrome / state stay in `view` and are imported.

use crate::ui::color::BatterySpectrum;
use crate::ui::start::row::{controller_row, game_row};
use crate::ui::start::view::{
    PINNED_SCROLL_GUTTER, ROW_GAP, ROW_HEIGHT, RowHintState, SECTION_SEP_H, SECTION_SEP_TEXT,
    StartListId, StartMessage, State, empty_games_browse_prompt,
};
use crate::ui::theme;
use iced::font::Weight;
use iced::mouse;
use iced::widget::canvas::{self, Frame, Geometry};
use iced::widget::{column, container, keyed_column, row, scrollable, space, text};
use iced::{
    Alignment, Color, Element, Fill, Font, Length, Padding, Point, Rectangle, Renderer, Size, Theme,
};

pub fn games_scroll_id() -> iced::widget::Id {
    StartListId::Games.scroll_id()
}

pub fn controllers_scroll_id() -> iced::widget::Id {
    StartListId::Controllers.scroll_id()
}

/// Stable `keyed_column` key for a Start list row.
///
/// iced 0.14 keyed columns require `Copy` keys, so the `play_key` / serial
/// string is hashed (deterministic FNV-1a 64) instead of used directly.
/// Equal strings map to equal keys across rebuilds, so a Steam-scan reorder
/// keeps per-row widget state instead of rebuilding all nodes positionally.
pub(crate) fn list_key(s: &str) -> u64 {
    const OFFSET: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x100000001b3;
    let mut hash = OFFSET;
    for byte in s.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

/// Section index for each games row in list order under the current section
/// mode (same [`games_section_mode`](crate::ui::start::view::State::games_section_mode) the immersive rail uses).
pub(crate) fn games_row_sections(state: &State) -> Vec<usize> {
    let mode = state.games_section_mode();
    let now_ms = crate::ui::start::position::now_ms();
    let utc_offset = crate::ui::start::position::local_utc_offset_secs();
    state
        .rows
        .iter()
        .map(|row| {
            crate::ui::start::position::section_index(
                mode,
                &row.title,
                row.played_at_ms,
                now_ms,
                utc_offset,
            )
        })
        .collect()
}

/// Extra stacked height above row `index` from section separators: one
/// [`SECTION_SEP_H`] block (+ column gap) per section header at or before it.
/// Every section opens with a header — including the first — so the first row
/// already sits below one separator.
pub(crate) fn separator_offset_above(sections: &[usize], index: usize) -> f32 {
    if sections.is_empty() {
        return 0.0;
    }
    let end = index.min(sections.len().saturating_sub(1));
    let mut headers = 1;
    for j in 1..=end {
        if sections[j] != sections[j - 1] {
            headers += 1;
        }
    }
    headers as f32 * (SECTION_SEP_H + ROW_GAP)
}

/// True when row `index` opens its section (always true for the first row).
pub(crate) fn is_section_start(sections: &[usize], index: usize) -> bool {
    match sections.get(index) {
        None => false,
        Some(section) => index == 0 || sections.get(index - 1) != Some(section),
    }
}

/// Content y of the header that opens the section of row `index`
/// (the row sits one [`SECTION_SEP_H`] block + gap below it).
pub(crate) fn header_top_for_row(sections: &[usize], index: usize) -> f32 {
    let row_top = index as f32 * (ROW_HEIGHT + ROW_GAP) + separator_offset_above(sections, index);
    (row_top - (SECTION_SEP_H + ROW_GAP)).max(0.0)
}

/// First row index of `sticky` section (`None` when empty / unknown).
pub(crate) fn sticky_start_index(sections: &[usize], sticky: Option<usize>) -> Option<usize> {
    let sticky = sticky?;
    sections.iter().position(|&s| s == sticky)
}

/// True when the collapsed sticky header sits at or before `row_index`, so the
/// actual content row sits one [`SECTION_SEP_H`] above its math position.
/// Exactly one header is collapsed whenever the list is non-empty, so actual
/// content height is always math height minus one header (stable); rows before
/// the sticky start are unaffected, rows at/after it shift up by the header.
pub(crate) fn collapsed_before_row(
    sections: &[usize],
    sticky: Option<usize>,
    row_index: usize,
) -> bool {
    sticky_start_index(sections, sticky).is_some_and(|start| start <= row_index)
}

/// Actual content y of row `row_index` given the math top and current sticky.
/// Math positions assume every header keeps its slot; the layout collapses the
/// stuck header to zero height, saving exactly [`SECTION_SEP_H`] for rows at
/// or after the sticky start (gaps are unchanged — the collapsed entry keeps
/// its `keyed_column` spacing neighbors).
pub(crate) fn actual_row_top_for_sticky(
    math_row_top: f32,
    sections: &[usize],
    sticky: Option<usize>,
    row_index: usize,
) -> f32 {
    if collapsed_before_row(sections, sticky, row_index) {
        (math_row_top - SECTION_SEP_H).max(0.0)
    } else {
        math_row_top
    }
}

/// Section pinned at the top of the viewport for `offset_y`: the last section
/// whose header starts at or above the offset. The pinned bar shows this
/// section until the next section's header reaches the top and supersedes it;
/// the stuck section's in-list header collapses to zero height so the pinned
/// bar carries its label without doubling layout height (fix B: collapse the
/// reserved bounds instead of keeping a blank `SECTION_SEP_H` slot, which left
/// a 38px pinned+blank+gap stack above an anchored row vs the normal 8px gap).
/// Scroll math still resolves stickiness against the stable math frame
/// ([`separator_offset_above`] / [`header_top_for_row`]); actual content
/// positions subtract the single collapsed header via
/// [`collapsed_before_row`] / [`actual_row_top_for_sticky`].
pub(crate) fn sticky_section_for_offset(sections: &[usize], offset_y: f32) -> Option<usize> {
    if sections.is_empty() {
        return None;
    }
    let mut current = sections[0];
    for (index, &section) in sections.iter().enumerate() {
        if !is_section_start(sections, index) {
            continue;
        }
        if header_top_for_row(sections, index) <= offset_y + 0.5 {
            current = section;
        } else {
            break;
        }
    }
    Some(current)
}

/// Full-width section separator: centered label with a hairline on either
/// side, vertically aligned. Each rule fades from [`theme::LINE`] at the text
/// to transparent at the far edge (same fade language as the immersive rail).
/// Emitted for every section — including the first — so the compact list
/// always shows its current section title.
pub(crate) fn section_separator(label: &'static str) -> Element<'static, StartMessage> {
    container(
        row![
            iced::widget::canvas(SeparatorFade { solid_right: true })
                .width(Fill)
                .height(Fill),
            text(label)
                .size(SECTION_SEP_TEXT)
                .font(Font {
                    weight: Weight::Semibold,
                    ..Font::DEFAULT
                })
                .color(theme::DIM),
            iced::widget::canvas(SeparatorFade { solid_right: false })
                .width(Fill)
                .height(Fill),
        ]
        .spacing(10)
        .align_y(Alignment::Center),
    )
    .padding([0, 8])
    .width(Fill)
    .height(Length::Fixed(SECTION_SEP_H))
    .into()
}

/// One side of a [`section_separator`]: a 1.5px rule (immersive-rail weight)
/// with a linear alpha fade. `solid_right` puts the solid end by the label.
struct SeparatorFade {
    solid_right: bool,
}

impl canvas::Program<StartMessage> for SeparatorFade {
    type State = ();

    fn draw(
        &self,
        _state: &Self::State,
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry> {
        let w = bounds.width;
        let h = bounds.height;
        if w < 8.0 || h <= 0.0 {
            return Vec::new();
        }
        let mut frame = Frame::new(renderer, bounds.size());
        let (edge, solid) = if self.solid_right {
            (
                Color {
                    a: 0.0,
                    ..theme::LINE
                },
                theme::LINE,
            )
        } else {
            (
                theme::LINE,
                Color {
                    a: 0.0,
                    ..theme::LINE
                },
            )
        };
        let fade = canvas::gradient::Linear::new(Point::new(0.0, 0.0), Point::new(w, 0.0))
            .add_stop(0.0, edge)
            .add_stop(1.0, solid);
        let thickness = 1.5;
        frame.fill_rectangle(
            Point::new(0.0, (h - thickness) * 0.5),
            Size::new(w, thickness),
            fade,
        );
        vec![frame.into_geometry()]
    }
}

pub(crate) fn games_list(state: &State) -> Element<'_, StartMessage> {
    if state.rows.is_empty() {
        let empty: Element<'_, StartMessage> = if state.editing {
            text("No installed Steam games — add a manual shortcut below.")
                .size(15.0)
                .color(theme::MUTED)
                .into()
        } else {
            empty_games_browse_prompt(state, 15.0)
        };
        return container(empty)
            .width(Fill)
            .height(Fill)
            .center_x(Fill)
            .center_y(Fill)
            .into();
    }
    // One timestamp for every row: the same section values the scroll math
    // (`games_row_sections`) uses, so headers and offsets can never disagree.
    let sections = games_row_sections(state);
    let labels = crate::ui::start::position::labels(state.games_section_mode());
    // Sticky current-section title: pinned in-flow above the list (it takes
    // layout space, like web-dev `sticky`) and remains in view until the next
    // section's header reaches the top and supersedes it.
    let sticky = sticky_section_for_offset(&sections, state.scrolls.offset(StartListId::Games));
    let mut entries: Vec<(u64, Element<'_, StartMessage>)> =
        Vec::with_capacity(state.rows.len() * 2);
    let mut last_section: Option<usize> = None;
    for (index, row) in state.rows.iter().enumerate() {
        let section = sections.get(index).copied().unwrap_or(0);
        // Every section opens with a header — including the first — so the
        // list always shows its current section title. The row at hand proves
        // its own section non-empty.
        if last_section != Some(section) {
            let label = labels.get(section).copied().unwrap_or("");
            // The stuck section's in-list header collapses to zero height — the
            // pinned bar carries its label, so keeping the 30px slot would double
            // it and leave a 38px pinned+blank+gap stack above an anchored row
            // (vs the normal 8px gap). Collapsing keeps exactly one collapsed
            // header at all times, so content height is math height minus one
            // header (stable; see `actual_row_top_for_sticky`).
            let header: Element<'_, StartMessage> = if Some(section) == sticky {
                container(space())
                    .width(Fill)
                    .height(Length::Fixed(0.0))
                    .into()
            } else {
                section_separator(label)
            };
            entries.push((
                list_key(&format!("compact-section-{index}-{section}")),
                header,
            ));
        }
        last_section = Some(section);
        let selected = index == state.game_selected;
        let status = if state.editing {
            None
        } else {
            state.launch_status(&row.target)
        };
        entries.push((
            list_key(&row.play_key),
            game_row(
                index,
                row,
                selected,
                status,
                state.editing,
                RowHintState::for_selected(state, selected),
            ),
        ));
    }
    let items = keyed_column(entries).spacing(ROW_GAP).width(Fill);
    let list: Element<'_, StartMessage> = scrollable(items)
        .id(games_scroll_id())
        .height(Fill)
        .width(Fill)
        .spacing(8)
        .on_scroll(|viewport| {
            let y = viewport.absolute_offset().y;
            StartMessage::GamesScrolled(y, viewport.bounds().height)
        })
        .into();
    let pinned_label = labels.get(sticky.unwrap_or(0)).copied().unwrap_or("");
    // Width-match the in-list titles: when the list overflows, the embedded
    // scrollbar narrows scrollable content by PINNED_SCROLL_GUTTER while the
    // pinned bar (a sibling of the scrollable) keeps full width, stretching
    // its fade rules. No gutter when everything fits (no scrollbar reserved).
    let gutter = if state.games_max_scroll(state.scrolls.viewport(StartListId::Games)) > 0.0 {
        PINNED_SCROLL_GUTTER
    } else {
        0.0
    };
    let pinned: Element<'_, StartMessage> = container(section_separator(pinned_label))
        .padding(Padding {
            right: gutter,
            ..Padding::ZERO
        })
        .width(Fill)
        .into();
    column![pinned, list].width(Fill).height(Fill).into()
}

pub(crate) fn controllers_list<'a>(
    state: &'a State,
    spectrum: &BatterySpectrum,
) -> Element<'a, StartMessage> {
    if state.controllers.is_empty() {
        return container(
            text("No DualSense connected")
                .size(15.0)
                .color(theme::MUTED),
        )
        .width(Fill)
        .height(Fill)
        .center_x(Fill)
        .center_y(Fill)
        .into();
    }
    let items = keyed_column(state.controllers.iter().enumerate().map(|(index, row)| {
        let selected = index == state.controller_selected;
        let powering_off = row.connected
            && state
                .powering_off
                .as_ref()
                .is_some_and(|s| s == &row.serial);
        (
            list_key(&row.serial),
            controller_row(
                index,
                row,
                selected,
                powering_off,
                spectrum,
                state.ring_flash_white(&row.serial),
                RowHintState::for_selected(state, selected),
            ),
        )
    }))
    .spacing(ROW_GAP)
    .width(Fill);
    scrollable(items)
        .id(controllers_scroll_id())
        .height(Fill)
        .width(Fill)
        .spacing(8)
        .on_scroll(|viewport| {
            let y = viewport.absolute_offset().y;
            StartMessage::ControllersScrolled(y, viewport.bounds().height)
        })
        .into()
}
