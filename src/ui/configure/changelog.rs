// Changelog modal content: build-time parsed release notes.
// Pull in the build-time generated structs and static data.
include!(concat!(env!("OUT_DIR"), "/changelog_generated.rs"));

use iced::font::Weight;
use iced::widget::{button, column, container, row, scrollable, text};
use iced::{Alignment, Element, Fill};

use crate::ui::theme;

/// Scrollable release notes with a close button top-right.
pub fn changelog_view<'a>() -> Element<'a, super::ConfigureMessage> {
    let header: Element<'a, super::ConfigureMessage> = row![
        text("Changelog")
            .size(16.0)
            .font(iced::Font {
                weight: Weight::Semibold,
                ..iced::Font::DEFAULT
            })
            .color(theme::INK)
            .width(Fill),
        button(text("Close").size(13.0))
            .padding([7, 14])
            .on_press(super::ConfigureMessage::CloseChangelog)
            .style(theme::secondary),
    ]
    .align_y(Alignment::Center)
    .spacing(8)
    .into();

    let mut releases = column![].spacing(18).width(Fill);
    for release in CHANGELOG {
        let mut release_col = column![
            text(release.version)
                .size(15.0)
                .font(iced::Font {
                    weight: Weight::Bold,
                    ..iced::Font::DEFAULT
                })
                .color(theme::INK),
        ]
        .spacing(10)
        .width(Fill);
        for section in release.sections {
            let mut section_col = column![
                text(section.title)
                    .size(theme::type_scale::CAPTION + 1.0)
                    .color(theme::DIM),
            ]
            .spacing(4)
            .width(Fill);
            for item in section.items {
                section_col =
                    section_col.push(text(format!("• {item}")).size(13.0).color(theme::INK));
            }
            release_col = release_col.push(section_col);
        }
        releases = releases.push(release_col);
    }

    container(
        column![
            header,
            scrollable(releases).width(Fill).style(theme::scrollbar),
        ]
        .spacing(12)
        .width(Fill),
    )
    .padding(16)
    .width(Fill)
    .into()
}
