//! Shared in-immersive dialog overlay: darken wash + centered card.
//!
//! Every dialog over the games stage is the same shape — a full-bleed wash
//! that dims the stage behind it, and a fixed-width opaque card centered on
//! top — but each call site used to build its own copy (manual-add's
//! takeover card, the replace-confirm wash + card, the debug tuning panel).
//! That drift is how the replace dialog ended up a 768px screen swap while
//! everything else moved on. New dialogs build from these pieces instead:
//!
//! * [`wash`] — dim layer; a press message dismisses, the card's own shield
//!   stops card clicks reaching it.
//! * [`dialog_card`] — padded opaque card at a caller-chosen width.
//! * [`click_shield`] — absorbs clicks that would otherwise fall through to
//!   the live stage behind a dialog.
//! * [`reveal`] — maps an eased 0..=1 transition progress to wash alpha,
//!   so enter/exit tweens can't drift either.
//!
//! Motion contract: the wash alpha fades and the card scales 0→1 about
//! its own center (see [`crate::ui::scale`] — a true visual scale with an
//! unchanged layout footprint, since iced 0.14 ships no generic opacity).
//! Neither ever translates. The wash fade always plays, including under
//! reduced motion (which snaps progress, so both settle instantly).

use crate::ui::motion::ease_out_cubic;
use crate::ui::start::view::StartMessage;
use crate::ui::theme;
use iced::{Alignment, Element, Fill, Length};
use std::time::Instant;

/// Dialog enter/exit tween length (drawer/dock travel at 220ms; a small
/// card settles a beat quicker).
pub const MODAL_ANIM_MS: u64 = 180;
/// Wash strength with the dialog fully revealed.
pub const MODAL_WASH_MAX: f32 = 0.6;

/// Generic eased 0..=1 tween driver (dialog enter/exit; same chase shape as
/// the drawer/dock anims). Content stays mounted through exit tweens and is
/// cleared when the tween settles.
#[derive(Debug, Clone)]
pub struct Tween {
    pub from: f32,
    pub to: f32,
    pub duration_ms: u64,
    pub started: Instant,
}

impl Tween {
    /// Eased progress along the tween.
    pub fn progress(&self, now: Instant) -> f32 {
        let t = (now.saturating_duration_since(self.started).as_secs_f32()
            / (self.duration_ms.max(1) as f32 / 1000.0))
            .clamp(0.0, 1.0);
        let e = ease_out_cubic(t);
        self.from + (self.to - self.from) * e
    }

    /// True once the tween's wall-clock duration has elapsed.
    pub fn is_settled(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.started)
            >= std::time::Duration::from_millis(self.duration_ms)
    }
}

/// Progress for an optional tween (`present` = settled value from content
/// presence). Pure so drivers stay one-liners.
pub fn tween_progress(anim: &Option<Tween>, present: bool, now: Instant) -> f32 {
    if let Some(tween) = anim {
        return tween.progress(now);
    }
    if present { 1.0 } else { 0.0 }
}

/// Arm a tween from `from` to `to` (`None` = snap: already there).
pub fn tween_to(from: f32, to: f32, now: Instant) -> Option<Tween> {
    if (from - to).abs() < 0.01 {
        None
    } else {
        Some(Tween {
            from,
            to,
            duration_ms: MODAL_ANIM_MS,
            started: now,
        })
    }
}

/// Settle a tween whose wall-clock duration has elapsed. Returns true when
/// an exit tween just drained (the caller clears the content); enter
/// settles and mid-tweens return false.
pub fn tween_settle(anim: &mut Option<Tween>, now: Instant) -> bool {
    let Some(tween) = anim.as_ref() else {
        return false;
    };
    if !tween.is_settled(now) {
        return false;
    }
    let closing = tween.to < 0.5;
    *anim = None;
    closing
}

/// Full-bleed darken wash. `dismiss` fires on press (backdrop click
/// cancels, matching the pad Cancel path).
pub fn wash(amount: f32, dismiss: StartMessage) -> Element<'static, StartMessage> {
    use iced::widget::{container, mouse_area, space};
    mouse_area(
        container(space())
            .width(Fill)
            .height(Fill)
            .style(theme::immersive_dim(amount)),
    )
    .on_press(dismiss)
    .into()
}

/// Opaque centered-card shell around dialog content. `width` is the
/// caller's: fixed for compact confirms, [`Length::Shrink`] for forms
/// whose inner column already fixes content width (the card hugs it plus
/// padding); `max_height` caps tall forms.
pub fn dialog_card<'a>(
    content: impl Into<Element<'a, StartMessage>>,
    width: Length,
    max_height: Option<f32>,
) -> Element<'a, StartMessage> {
    use iced::widget::container;
    let card = container(content)
        .width(width)
        .padding(24)
        .style(theme::modal_card);
    match max_height {
        Some(max) => card.max_height(max).into(),
        None => card.into(),
    }
}

/// Click shield: absorbs presses over the card so they never reach the
/// live stage behind the dialog. `absorb` is an inert message.
pub fn click_shield<'a>(
    content: impl Into<Element<'a, StartMessage>>,
    absorb: StartMessage,
) -> Element<'a, StartMessage> {
    use iced::widget::mouse_area;
    mouse_area(content.into()).on_press(absorb).into()
}

/// Center `content` in the window (both axes).
pub fn centered<'a>(content: impl Into<Element<'a, StartMessage>>) -> Element<'a, StartMessage> {
    use iced::widget::container;
    container(content)
        .width(Fill)
        .height(Fill)
        .center_x(Fill)
        .center_y(Fill)
        .into()
}

/// Reveal mapping for an eased 0..=1 dialog progress: wash alpha. Pure
/// so the enter/exit math is unit-testable; the easing itself lives in
/// the progress driver (same `ease_out_cubic` chase as the drawer/dock).
/// The card itself never moves — it mounts once the wash starts in and
/// unmounts when the wash drains.
pub fn reveal(progress: f32) -> f32 {
    progress.clamp(0.0, 1.0) * MODAL_WASH_MAX
}

/// Centered dialog text column (title + subtitle), matching the card's
/// centered content alignment.
pub fn dialog_text<'a>(
    title: &'a str,
    subtitle: String,
    title_size: f32,
    subtitle_size: f32,
) -> Element<'a, StartMessage> {
    use iced::widget::{column, text};
    column![
        text(title).size(title_size).color(theme::INK),
        text(subtitle).size(subtitle_size).color(theme::MUTED),
    ]
    .spacing(4)
    .align_x(Alignment::Center)
    .width(Fill)
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reveal_maps_progress_to_wash_alpha() {
        assert!((reveal(0.0) - 0.0).abs() < 1e-6);
        assert!((reveal(1.0) - MODAL_WASH_MAX).abs() < 1e-6);
        assert!((reveal(0.5) - MODAL_WASH_MAX * 0.5).abs() < 1e-6);
    }

    #[test]
    fn reveal_clamps_out_of_range_progress() {
        assert_eq!(reveal(-1.0), 0.0);
        assert_eq!(reveal(2.0), MODAL_WASH_MAX);
    }

    #[test]
    fn tween_progress_drives_enter_settle_and_snap() {
        let start = Instant::now();
        let anim = tween_to(0.0, 1.0, start).expect("moves");
        assert!((tween_progress(&Some(anim.clone()), false, start) - 0.0).abs() < 1e-6);
        let end = start + std::time::Duration::from_millis(MODAL_ANIM_MS + 10);
        assert!((tween_progress(&Some(anim), false, end) - 1.0).abs() < 1e-3);
        assert_eq!(tween_progress(&None, true, end), 1.0);
        assert_eq!(tween_progress(&None, false, end), 0.0);
        assert!(tween_to(1.0, 1.0, start).is_none());
    }
}
