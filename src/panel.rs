//! Pure six-color framebuffer packing for the 7.3-inch Spectra 6 panel.
//!
//! 4 bits per pixel, two pixels per byte, high nibble first. Values match the
//! vendor demo's palette indices.

use crate::font;

pub const WIDTH: usize = 800;
pub const HEIGHT: usize = 480;
pub const STRIDE: usize = WIDTH / 2;

pub const BLACK: u8 = 0;
pub const WHITE: u8 = 1;
pub const YELLOW: u8 = 2;
pub const RED: u8 = 3;
pub const BLUE: u8 = 5;
pub const GREEN: u8 = 6;

pub const PALETTE: [(u8, [u8; 3], &str); 6] = [
    (BLACK, [0, 0, 0], "black"),
    (WHITE, [255, 255, 255], "white"),
    (YELLOW, [255, 255, 0], "yellow"),
    (RED, [255, 0, 0], "red"),
    (BLUE, [0, 0, 255], "blue"),
    (GREEN, [0, 255, 0], "green"),
];

pub const BAR_ORDER: [(u8, &str); 6] = [
    (BLACK, "black"),
    (YELLOW, "yellow"),
    (RED, "red"),
    (BLUE, "blue"),
    (GREEN, "green"),
    (WHITE, "white"),
];

pub const fn buffer_len() -> usize {
    STRIDE * HEIGHT
}

/// Nearest palette entry by squared RGB distance. Ties follow `PALETTE` order.
pub fn nearest(red: u8, green: u8, blue: u8) -> u8 {
    let mut best = PALETTE[0];
    let mut best_distance = u32::MAX;
    for &(index, rgb, _) in &PALETTE {
        let dr = i32::from(red) - i32::from(rgb[0]);
        let dg = i32::from(green) - i32::from(rgb[1]);
        let db = i32::from(blue) - i32::from(rgb[2]);
        let distance = (dr * dr + dg * dg + db * db) as u32;
        if distance < best_distance {
            best = (index, rgb, "");
            best_distance = distance;
        }
    }
    best.0
}

pub fn fill(buffer: &mut [u8], color: u8) {
    assert_eq!(buffer.len(), buffer_len());
    buffer.fill((color << 4) | color);
}

/// Draw with `(0, 0)` at the viewer's top-left. The panel's native scan order
/// is rotated 180 degrees, so both axes are flipped here.
pub fn set_pixel(buffer: &mut [u8], x: usize, y: usize, color: u8) {
    assert_eq!(buffer.len(), buffer_len());
    if x >= WIDTH || y >= HEIGHT {
        return;
    }
    let x = WIDTH - 1 - x;
    let y = HEIGHT - 1 - y;
    let address = x / 2 + y * STRIDE;
    let mask = 0xF0_u8 >> ((x % 2) * 4);
    buffer[address] = (buffer[address] & !mask) | ((color << 4) >> ((x % 2) * 4));
}

#[cfg(test)]
pub fn pixel(buffer: &[u8], x: usize, y: usize) -> u8 {
    assert_eq!(buffer.len(), buffer_len());
    assert!(x < WIDTH && y < HEIGHT);
    let x = WIDTH - 1 - x;
    let y = HEIGHT - 1 - y;
    let byte = buffer[x / 2 + y * STRIDE];
    if x.is_multiple_of(2) {
        byte >> 4
    } else {
        byte & 0x0F
    }
}

/// Six equal vertical bars, left to right in `BAR_ORDER`.
pub fn bars(buffer: &mut [u8]) {
    assert_eq!(buffer.len(), buffer_len());
    for (index, (color, _)) in BAR_ORDER.iter().enumerate() {
        let start = index * WIDTH / BAR_ORDER.len();
        let end = (index + 1) * WIDTH / BAR_ORDER.len();
        for y in 0..HEIGHT {
            for x in start..end {
                set_pixel(buffer, x, y, *color);
            }
        }
    }
}

/// Index of `ch` in the generated table, or `None` when the font has no glyph.
///
/// The table is sorted by code point, so this is a binary search rather than a scan:
/// drawing happens while a user is waiting, and the panel is 800 pixels of text.
fn glyph(ch: char) -> Option<&'static font::Glyph> {
    font::GLYPHS
        .binary_search_by(|candidate| candidate.ch.cmp(&ch))
        .ok()
        .map(|index| &font::GLYPHS[index])
}

/// Advance a single character occupies, with `?` standing in for anything missing.
///
/// The fallback keeps a layout honest: an unsupported character still costs space, so
/// text after it does not slide left into a shape nobody asked for.
fn char_advance(ch: char) -> usize {
    match glyph(ch) {
        Some(entry) => usize::from(entry.advance),
        None => glyph('?').map_or(0, |entry| usize::from(entry.advance)),
    }
}

/// Width in pixels `text` needs on one line.
pub fn text_width(text: &str) -> usize {
    text.chars().map(char_advance).sum()
}

/// Draw one line with its top-left at `(x, y)`, returning the pen position after the
/// last glyph.
///
/// Coordinates are the viewer's, like `set_pixel`, so the rotation lives in one place.
/// Ink only: the background is whatever the framebuffer already holds, which lets a
/// caller clear once and write a header, a transcript and a reply without repainting.
pub fn draw_text(buffer: &mut [u8], x: usize, y: usize, color: u8, text: &str) -> usize {
    let line_height = usize::from(font::LINE_HEIGHT);
    let mut pen = x;
    for ch in text.chars() {
        let Some(entry) = glyph(ch) else {
            pen += char_advance(ch);
            continue;
        };
        let advance = usize::from(entry.advance);
        let row_bytes = advance.div_ceil(8);
        let offset = entry.offset as usize;
        for row in 0..line_height {
            let base = offset + row * row_bytes;
            for column in 0..advance {
                let Some(&byte) = font::BITMAP.get(base + column / 8) else {
                    continue;
                };
                if byte & (0x80 >> (column % 8)) != 0 {
                    set_pixel(buffer, pen + column, y + row, color);
                }
            }
        }
        pen += advance;
    }
    pen
}

/// Break `text` into lines no wider than `width`, honouring explicit newlines.
///
/// Words are kept whole where they fit. A word wider than the column is broken by
/// character, because the alternative is pushing text off the panel and losing it: a
/// German compound or a URL would otherwise vanish from the transcript.
pub fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let space = char_advance(' ');
    let mut lines = Vec::new();
    for paragraph in text.split('\n') {
        let mut line = String::new();
        let mut line_width = 0;
        for word in paragraph.split_whitespace() {
            let word_width = text_width(word);
            if !line.is_empty() && line_width + space + word_width <= width {
                line.push(' ');
                line.push_str(word);
                line_width += space + word_width;
                continue;
            }
            if !line.is_empty() {
                lines.push(core::mem::take(&mut line));
                line_width = 0;
            }
            if word_width <= width {
                line.push_str(word);
                line_width = word_width;
                continue;
            }
            for ch in word.chars() {
                let advance = char_advance(ch);
                if !line.is_empty() && line_width + advance > width {
                    lines.push(core::mem::take(&mut line));
                    line_width = 0;
                }
                line.push(ch);
                line_width += advance;
            }
        }
        lines.push(line);
    }
    lines
}

/// Draw wrapped text inside the column starting at `x` and `width` pixels wide, using
/// at most `max_lines` lines, and return `(lines drawn, y below the last line)`.
///
/// Lines that would fall off the bottom are dropped rather than drawn halfway: on a
/// six-colour panel a half row of text looks like a bug, not like punctuation.
pub fn draw_paragraph(
    buffer: &mut [u8],
    x: usize,
    y: usize,
    width: usize,
    color: u8,
    max_lines: usize,
    text: &str,
) -> (usize, usize) {
    let line_height = usize::from(font::LINE_HEIGHT);
    let mut top = y;
    let mut drawn = 0;
    for line in wrap_text(text, width) {
        if drawn == max_lines || top >= HEIGHT {
            break;
        }
        draw_text(buffer, x, top, color, &line);
        drawn += 1;
        top += line_height;
    }
    (drawn, top)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffer_is_four_bits_per_pixel() {
        assert_eq!(buffer_len(), 192_000);
    }

    #[test]
    fn pixel_nibbles_and_bounds() {
        let mut buffer = vec![0_u8; buffer_len()];
        fill(&mut buffer, WHITE);
        assert!(buffer.iter().all(|&byte| byte == 0x11));
        set_pixel(&mut buffer, 0, 0, BLACK);
        assert_eq!(buffer[(HEIGHT - 1) * STRIDE + STRIDE - 1], 0x10);
        assert_eq!(pixel(&buffer, 0, 0), BLACK);
        set_pixel(&mut buffer, 1, 0, RED);
        assert_eq!(buffer[(HEIGHT - 1) * STRIDE + STRIDE - 1], 0x30);
        assert_eq!(pixel(&buffer, 1, 0), RED);
        set_pixel(&mut buffer, 799, 479, GREEN);
        assert_eq!(buffer[0], 0x61);
        assert_eq!(pixel(&buffer, 799, 479), GREEN);
        set_pixel(&mut buffer, 800, 0, BLUE);
        set_pixel(&mut buffer, 0, 480, BLUE);
        assert_eq!(buffer[(HEIGHT - 1) * STRIDE + STRIDE - 1], 0x30);
    }

    #[test]
    fn nearest_maps_exact_palette_and_midpoints() {
        for (index, rgb, _) in PALETTE {
            assert_eq!(nearest(rgb[0], rgb[1], rgb[2]), index);
        }
        assert_eq!(nearest(120, 120, 120), BLACK);
        assert_eq!(nearest(250, 5, 5), RED);
    }

    #[test]
    fn font_covers_ascii_cyrillic_and_the_punctuation_that_reaches_the_panel() {
        for code in 0x20..0x7F_u32 {
            assert!(
                glyph(char::from_u32(code).unwrap()).is_some(),
                "ascii {code:#x}"
            );
        }
        for code in 0x410..0x450_u32 {
            assert!(
                glyph(char::from_u32(code).unwrap()).is_some(),
                "cyrillic {code:#x}"
            );
        }
        for ch in ['Ё', 'ё', '«', '»', '—', '…', '№'] {
            assert!(glyph(ch).is_some(), "{ch}");
        }
        let sentence = "Сколько зайцев пляшет вокруг ёлки?";
        assert!(sentence.chars().all(|ch| glyph(ch).is_some()));
    }

    #[test]
    fn blitter_writes_exactly_the_font_bits() {
        let ch = 'Ж';
        let entry = glyph(ch).expect("Ж in the table");
        let advance = usize::from(entry.advance);
        let row_bytes = advance.div_ceil(8);
        let mut buffer = vec![0_u8; buffer_len()];
        fill(&mut buffer, WHITE);
        let (x, y) = (40, 60);
        let pen = draw_text(&mut buffer, x, y, BLACK, &ch.to_string());
        assert_eq!(pen, x + advance);
        for row in 0..usize::from(font::LINE_HEIGHT) {
            for column in 0..advance {
                let byte = font::BITMAP[entry.offset as usize + row * row_bytes + column / 8];
                let expected = if byte & (0x80 >> (column % 8)) != 0 {
                    BLACK
                } else {
                    WHITE
                };
                assert_eq!(
                    pixel(&buffer, x + column, y + row),
                    expected,
                    "{ch} at column={column} row={row}"
                );
            }
        }
    }

    #[test]
    fn blitter_advances_by_the_pen_and_misses_unknown_characters() {
        let mut buffer = vec![0_u8; buffer_len()];
        fill(&mut buffer, WHITE);
        let pen = draw_text(&mut buffer, 0, 0, BLACK, "AA");
        assert_eq!(pen, 2 * text_width("A"));
        assert_eq!(text_width("Ая"), char_advance('А') + char_advance('я'));
        // A code point outside the table costs the width of `?` and draws nothing.
        let unknown = '\u{1F600}';
        assert!(glyph(unknown).is_none());
        assert_eq!(char_advance(unknown), char_advance('?'));
        let pen = draw_text(&mut buffer, 0, 40, BLACK, &unknown.to_string());
        assert_eq!(pen, char_advance('?'));
        for row in 0..usize::from(font::LINE_HEIGHT) {
            for column in 0..text_width("?") {
                assert_eq!(pixel(&buffer, column, 40 + row), WHITE);
            }
        }
    }

    #[test]
    fn wrap_keeps_words_inside_the_column() {
        let text = "Сколько зайцев пляшет вокруг ёлки и не хочет останавливаться";
        // Wide enough for the longest word, so this case is about keeping words
        // whole rather than about breaking them; the font size may move.
        let width = text_width("останавливаться") + 30;
        let lines = wrap_text(text, width);
        assert!(lines.len() > 1);
        for line in &lines {
            assert!(text_width(line) <= width, "{line:?} is too wide");
        }
        let rejoined: Vec<String> = lines
            .iter()
            .flat_map(|line| line.split_whitespace().map(str::to_owned))
            .collect();
        assert_eq!(rejoined, text.split_whitespace().collect::<Vec<_>>());
        // Explicit newlines survive, including a deliberate blank line.
        assert_eq!(wrap_text("one\n\ntwo", width), vec!["one", "", "two"]);
    }

    #[test]
    fn wrap_breaks_a_word_that_cannot_fit_alone() {
        let width = 60;
        let word = "нечленораздельность";
        assert!(text_width(word) > width);
        let lines = wrap_text(word, width);
        assert!(lines.len() > 1);
        for line in &lines {
            assert!(text_width(line) <= width, "{line:?} is too wide");
        }
        assert_eq!(lines.concat(), word);
    }

    #[test]
    fn paragraph_stops_at_the_line_limit_and_at_the_bottom_edge() {
        let mut buffer = vec![0_u8; buffer_len()];
        fill(&mut buffer, WHITE);
        let text = "Сколько зайцев пляшет вокруг ёлки и не хочет останавливаться";
        let (drawn, bottom) = draw_paragraph(&mut buffer, 20, 20, 300, BLACK, 2, text);
        assert_eq!(drawn, 2);
        assert_eq!(bottom, 20 + 2 * usize::from(font::LINE_HEIGHT));
        let (drawn, bottom) = draw_paragraph(&mut buffer, 20, HEIGHT - 10, 300, BLACK, 5, text);
        assert_eq!(drawn, 1);
        assert!(bottom > HEIGHT);
        let (drawn, _) = draw_paragraph(&mut buffer, 20, HEIGHT + 5, 300, BLACK, 5, text);
        assert_eq!(drawn, 0);
    }

    #[test]
    fn bars_cover_every_column_with_expected_colors() {
        let mut buffer = vec![0_u8; buffer_len()];
        bars(&mut buffer);
        for (index, (color, _)) in BAR_ORDER.iter().enumerate() {
            let start = index * WIDTH / BAR_ORDER.len();
            let end = (index + 1) * WIDTH / BAR_ORDER.len();
            for x in [start, (start + end) / 2, end - 1] {
                assert_eq!(pixel(&buffer, x, HEIGHT / 2), *color, "bar {index} x={x}");
            }
        }
        assert_eq!(pixel(&buffer, 0, 0), BLACK);
        assert_eq!(pixel(&buffer, WIDTH - 1, HEIGHT - 1), WHITE);
    }
}
