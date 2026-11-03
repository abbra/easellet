//! Screens: several plugin results arranged on one frame.
//!
//! A screen is a fixed preset of rectangles and a list of jobs, one per
//! rectangle. The bridge keeps each job's latest structured output and draws
//! the whole panel once, so a frame with weather, prices and the day's list
//! refreshes once instead of once per plugin.
//!
//! Composition happens before drawing: a plugin still returns one card or
//! picture and knows nothing about the other plugins on the screen.

use std::sync::Arc;

use crate::plugins::Output;
use crate::render::{self, Card, Fit, Fonts, Rect};

/// The arrangement of tiles on a panel. Each preset is a fixed geometry; the
/// panel has no room for arbitrary windows, and every rectangle needs a text
/// layout that fits it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preset {
    /// One panel owns the whole screen. This is the behaviour of a frame with
    /// no screen.
    Single,
    /// Two panels, one above the other.
    Stack,
    /// Two panels, side by side.
    Columns,
    /// One wide panel above two half-width ones.
    Hero,
    /// Four panels in a 2x2 grid.
    Grid,
}

impl Preset {
    /// Read a preset name from the configuration; empty means `single`.
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "" | "single" => Some(Self::Single),
            "stack" => Some(Self::Stack),
            "columns" => Some(Self::Columns),
            "hero" => Some(Self::Hero),
            "grid" => Some(Self::Grid),
            _ => None,
        }
    }

    /// The name the configuration uses.
    pub fn name(self) -> &'static str {
        match self {
            Self::Single => "single",
            Self::Stack => "stack",
            Self::Columns => "columns",
            Self::Hero => "hero",
            Self::Grid => "grid",
        }
    }

    /// The tiles, in slot order.
    pub fn cells(self) -> &'static [Rect] {
        const SINGLE: [Rect; 1] = [Rect::new(0, 0, render::WIDTH, render::HEIGHT)];
        const STACK: [Rect; 2] = [
            Rect::new(0, 0, render::WIDTH, 239),
            Rect::new(0, 241, render::WIDTH, 239),
        ];
        const COLUMNS: [Rect; 2] = [
            Rect::new(0, 0, 399, render::HEIGHT),
            Rect::new(401, 0, 399, render::HEIGHT),
        ];
        const HERO: [Rect; 3] = [
            Rect::new(0, 0, render::WIDTH, 258),
            Rect::new(0, 260, 399, 220),
            Rect::new(401, 260, 399, 220),
        ];
        const GRID: [Rect; 4] = [
            Rect::new(0, 0, 399, 239),
            Rect::new(401, 0, 399, 239),
            Rect::new(0, 241, 399, 239),
            Rect::new(401, 241, 399, 239),
        ];
        match self {
            Self::Single => &SINGLE,
            Self::Stack => &STACK,
            Self::Columns => &COLUMNS,
            Self::Hero => &HERO,
            Self::Grid => &GRID,
        }
    }

    /// The 2 px black lines between the tiles.
    fn rules(self) -> &'static [Rect] {
        const NONE: [Rect; 0] = [];
        const STACK: [Rect; 1] = [Rect::new(0, 239, render::WIDTH, 2)];
        const COLUMNS: [Rect; 1] = [Rect::new(399, 0, 2, render::HEIGHT)];
        const HERO: [Rect; 2] = [
            Rect::new(0, 258, render::WIDTH, 2),
            Rect::new(399, 260, 2, 220),
        ];
        const GRID: [Rect; 4] = [
            Rect::new(399, 0, 2, 239),
            Rect::new(0, 239, 399, 2),
            Rect::new(401, 239, 399, 2),
            Rect::new(399, 241, 2, 239),
        ];
        match self {
            Self::Single => &NONE,
            Self::Stack => &STACK,
            Self::Columns => &COLUMNS,
            Self::Hero => &HERO,
            Self::Grid => &GRID,
        }
    }
}

/// What a job produced, kept as content until the frame is drawn. Pictures are
/// shared, not copied, because a photo can be large.
#[derive(Debug, Clone)]
pub enum Panel {
    Card(Card),
    Image { bytes: Arc<Vec<u8>>, fit: Fit },
}

impl Panel {
    /// Keep an output for later; `None` means "nothing new".
    pub fn from_output(output: &Output) -> Option<Self> {
        match output {
            Output::Card(card) => Some(Self::Card(card.clone())),
            Output::Image { bytes, fit } => Some(Self::Image {
                bytes: Arc::new(bytes.clone()),
                fit: *fit,
            }),
            Output::None => None,
        }
    }

    /// One panel as the whole 800x480 frame.
    pub fn frame(&self, fonts: Option<&Fonts>) -> Result<Vec<u8>, String> {
        match self {
            Self::Card(card) => {
                let fonts = fonts.ok_or_else(|| "no font for cards".to_owned())?;
                Ok(render::card_frame(card, fonts))
            }
            Self::Image { bytes, fit } => render::photo_frame(bytes, *fit),
        }
    }
}

/// One slot of a screen: the panel to draw and whether it is older than its job
/// wants. An empty slot is `None`, not a tile.
pub struct Tile<'a> {
    pub panel: &'a Panel,
    pub stale: bool,
}

/// Draw the whole screen. A slot without a tile keeps the paper, so a frame
/// that has not run a job yet still shows the others.
pub fn render(
    preset: Preset,
    tiles: &[Option<Tile<'_>>],
    fonts: &Fonts,
) -> Result<Vec<u8>, String> {
    if preset == Preset::Single {
        let tile = tiles
            .first()
            .and_then(Option::as_ref)
            .ok_or_else(|| "nothing to show".to_owned())?;
        return tile.panel.frame(Some(fonts));
    }
    let mut canvas = render::Canvas::new(render::WHITE);
    for (cell, tile) in preset.cells().iter().zip(tiles) {
        let Some(tile) = tile else {
            continue;
        };
        match &tile.panel {
            Panel::Card(card) => render::render_tile(&mut canvas, *cell, card, fonts, tile.stale),
            Panel::Image { bytes, fit } => {
                render::draw_picture(&mut canvas, *cell, bytes, *fit)?;
            }
        }
    }
    for rule in preset.rules() {
        canvas.fill(rule.x, rule.y, rule.width, rule.height, render::BLACK);
    }
    Ok(render::frame_from_rgb(&canvas.rgb, false))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::{Colour, Item, Status};

    fn card(title: &str, text: &str) -> Panel {
        Panel::Card(Card::List {
            title: title.to_owned(),
            items: vec![Item {
                text: text.to_owned(),
                status: Some(Status::Ok),
                counts: None,
            }],
            accent: Colour::Blue,
        })
    }

    fn fonts() -> Fonts {
        Fonts::load(None).expect("DejaVu Sans on this host")
    }

    #[test]
    fn every_preset_covers_the_panel_without_overlapping_tiles() {
        for preset in [
            Preset::Single,
            Preset::Stack,
            Preset::Columns,
            Preset::Hero,
            Preset::Grid,
        ] {
            let cells = preset.cells();
            assert!(!cells.is_empty(), "{preset:?} has no slots");
            for cell in cells {
                assert!(cell.right() <= render::WIDTH, "{preset:?}: {cell:?}");
                assert!(cell.bottom() <= render::HEIGHT, "{preset:?}: {cell:?}");
            }
            for (index, cell) in cells.iter().enumerate() {
                for other in &cells[index + 1..] {
                    let apart = cell.right() <= other.x
                        || other.right() <= cell.x
                        || cell.bottom() <= other.y
                        || other.bottom() <= cell.y;
                    assert!(apart, "{preset:?}: {cell:?} overlaps {other:?}");
                }
            }
        }
    }

    #[test]
    fn a_screen_is_the_same_bytes_for_the_same_tiles() {
        let fonts = fonts();
        let a = card("Погода", "Эспоо +2°");
        let b = card("Цена", "0,67 c/кВт·ч");
        let tiles = [
            Some(Tile {
                panel: &a,
                stale: false,
            }),
            Some(Tile {
                panel: &b,
                stale: false,
            }),
        ];
        let first = render(Preset::Columns, &tiles, &fonts).unwrap();
        let second = render(Preset::Columns, &tiles, &fonts).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn a_changed_tile_changes_the_screen() {
        let fonts = fonts();
        let a = card("Погода", "Эспоо +2°");
        let b = card("Погода", "Эспоо -9°");
        let with = |panel| {
            [
                Some(Tile {
                    panel,
                    stale: false,
                }),
                None,
            ]
        };
        let first = render(Preset::Stack, &with(&a), &fonts).unwrap();
        let second = render(Preset::Stack, &with(&b), &fonts).unwrap();
        assert_ne!(first, second);
    }

    #[test]
    fn an_empty_slot_stays_paper() {
        let fonts = fonts();
        let a = card("Погода", "Эспоо +2°");
        let tiles = [
            Some(Tile {
                panel: &a,
                stale: false,
            }),
            None,
            None,
        ];
        let frame = render(Preset::Hero, &tiles, &fonts).unwrap();
        let rgb = render::unpack(&frame);
        let at = |x: usize, y: usize| rgb[(y * render::WIDTH + x) * 3..][..3].to_vec();
        // The empty strip below the main tile is white, and its rule is black.
        assert_eq!(at(200, 300), render::WHITE.to_vec());
        assert_eq!(at(200, 258), render::BLACK.to_vec());
        assert_eq!(at(399, 300), render::BLACK.to_vec());
    }

    #[test]
    fn a_picture_fills_its_slot_without_touching_the_other() {
        let fonts = fonts();
        let mut picture = image::RgbImage::new(1600, 400);
        for (x, _, pixel) in picture.enumerate_pixels_mut() {
            *pixel = image::Rgb(if x < 800 { render::RED } else { render::BLUE });
        }
        let mut encoded = Vec::new();
        image::DynamicImage::ImageRgb8(picture)
            .write_to(
                &mut std::io::Cursor::new(&mut encoded),
                image::ImageFormat::Png,
            )
            .unwrap();
        let output = Output::Image {
            bytes: encoded,
            fit: Fit::Cover,
        };
        let picture = Panel::from_output(&output).expect("a picture is a panel");
        let side = card("Цена", "0,67 c/кВт·ч");
        let tiles = [
            Some(Tile {
                panel: &picture,
                stale: false,
            }),
            Some(Tile {
                panel: &side,
                stale: false,
            }),
        ];
        let frame = render(Preset::Columns, &tiles, &fonts).unwrap();
        let rgb = render::unpack(&frame);
        let pixels = |x: usize, y: usize| rgb[(y * render::WIDTH + x) * 3..][..3].to_vec();
        // The picture's own two colours survive in its cell.
        assert_eq!(pixels(40, 240), render::RED.to_vec());
        assert_eq!(pixels(360, 240), render::BLUE.to_vec());
        // The card cell begins after the rule and is paper, not picture.
        assert_eq!(pixels(400, 10), render::BLACK.to_vec());
        assert_eq!(pixels(402, 470), render::WHITE.to_vec());
    }

    #[test]
    fn single_is_the_ordinary_full_panel_card() {
        let fonts = fonts();
        let panel = card("Заметка", "Одна карточка на весь экран");
        let Panel::Card(inner) = &panel else {
            unreachable!()
        };
        let tiles = [Some(Tile {
            panel: &panel,
            stale: false,
        })];
        assert_eq!(
            render(Preset::Single, &tiles, &fonts).unwrap(),
            render::card_frame(inner, &fonts)
        );
    }
}
