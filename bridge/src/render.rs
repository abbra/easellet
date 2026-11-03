//! Pictures for the panels: cards drawn in exact palette colours, photographs
//! dithered, both packed into the panel's own bytes, plus a PNG of what the
//! panel will show.
//!
//! The Spectra 6 path is a port of `tools/convert.py`'s `frame_from_rgb`, kept
//! byte-identical to it by a test that runs the Python on the same pixels: the
//! same picture must look the same whether the browser (`tools/el6.mjs`), the
//! host converter or the bridge prepared it.
//!
//! Cards are e-paper first: text and shapes are drawn without anti-aliasing in
//! the six colours the panel has, so nothing is dithered and every edge is crisp.
//! See `research/bridge-ui-design.md` section 4.

use ab_glyph::{Font, FontVec, PxScale, ScaleFont};
use serde::Deserialize;

/// The Spectra 6 panel.
pub const WIDTH: usize = 800;
pub const HEIGHT: usize = 480;
pub const FRAME_BYTES: usize = WIDTH * HEIGHT / 2;

/// Panel index and RGB of each colour, in `convert.py`'s order: nearest-colour
/// ties go to the first, exactly as Python's `min` does.
pub const PALETTE: [(u8, [u8; 3]); 6] = [
    (0, [0, 0, 0]),
    (1, [255, 255, 255]),
    (2, [255, 255, 0]),
    (3, [255, 0, 0]),
    (5, [0, 0, 255]),
    (6, [0, 255, 0]),
];

pub const BLACK: [u8; 3] = [0, 0, 0];
pub const WHITE: [u8; 3] = [255, 255, 255];
pub const YELLOW: [u8; 3] = [255, 255, 0];
pub const RED: [u8; 3] = [255, 0, 0];
pub const BLUE: [u8; 3] = [0, 0, 255];
pub const GREEN: [u8; 3] = [0, 255, 0];

/// Nearest palette entry, by squared distance; the first wins a tie.
fn nearest(rgb: [i32; 3]) -> (u8, [u8; 3]) {
    let mut best = PALETTE[0];
    let mut best_distance = i64::MAX;
    for entry in PALETTE {
        let distance: i64 = (0..3)
            .map(|channel| {
                let delta = i64::from(rgb[channel]) - i64::from(entry.1[channel]);
                delta * delta
            })
            .sum();
        if distance < best_distance {
            best = entry;
            best_distance = distance;
        }
    }
    best
}

/// 800x480 RGB to a packed panel frame, optionally with Floyd-Steinberg.
///
/// Line for line `convert.py`: float pixels, rounding half to even (Python's
/// `round`), the error taken from the unclamped value, the four neighbours in
/// the same order with the same factors.
pub fn frame_from_rgb(rgb: &[u8], dither: bool) -> Vec<u8> {
    assert_eq!(rgb.len(), WIDTH * HEIGHT * 3, "need {WIDTH}x{HEIGHT} RGB");
    pack(&palette_indices(rgb, WIDTH, HEIGHT, dither))
}

/// RGB (viewer order, `width` x `height`) to the palette index of each pixel.
/// The dithering error never leaves the rectangle, so a picture tile cannot
/// speckle a card beside it.
fn palette_indices(rgb: &[u8], width: usize, height: usize, dither: bool) -> Vec<u8> {
    assert_eq!(rgb.len(), width * height * 3, "need {width}x{height} RGB");
    let mut pixels: Vec<[f64; 3]> = rgb
        .as_chunks::<3>()
        .0
        .iter()
        .map(|pixel| {
            [
                f64::from(pixel[0]),
                f64::from(pixel[1]),
                f64::from(pixel[2]),
            ]
        })
        .collect();
    let mut indices = vec![0u8; width * height];
    let spread = |pixels: &mut [[f64; 3]], offset: usize, error: [f64; 3], factor: f64| {
        for channel in 0..3 {
            pixels[offset][channel] += error[channel] * factor;
        }
    };
    for y in 0..height {
        for x in 0..width {
            let offset = y * width + x;
            let old = pixels[offset];
            let current = old.map(|channel| channel.round_ties_even().clamp(0.0, 255.0) as i32);
            let (index, target) = nearest(current);
            indices[offset] = index;
            if !dither {
                continue;
            }
            let error = [0, 1, 2].map(|channel| old[channel] - f64::from(target[channel]));
            if x + 1 < width {
                spread(&mut pixels, offset + 1, error, 7.0 / 16.0);
            }
            if y + 1 < height {
                let row = offset + width;
                if x > 0 {
                    spread(&mut pixels, row - 1, error, 3.0 / 16.0);
                }
                spread(&mut pixels, row, error, 5.0 / 16.0);
                if x + 1 < width {
                    spread(&mut pixels, row + 1, error, 1.0 / 16.0);
                }
            }
        }
    }
    indices
}

/// Panel indices, viewer order, to the panel's bytes: rotated 180°, two pixels
/// per byte, the high nibble for even native columns.
fn pack(indices: &[u8]) -> Vec<u8> {
    let mut frame = vec![0u8; FRAME_BYTES];
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let native_x = WIDTH - 1 - x;
            let native_y = HEIGHT - 1 - y;
            let address = native_y * (WIDTH / 2) + native_x / 2;
            let index = indices[y * WIDTH + x];
            frame[address] |= if native_x.is_multiple_of(2) {
                index << 4
            } else {
                index
            };
        }
    }
    frame
}

/// A packed frame back to viewer-order RGB, as the panel will show it.
pub fn unpack(frame: &[u8]) -> Vec<u8> {
    let mut rgb = vec![0u8; WIDTH * HEIGHT * 3];
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let native_x = WIDTH - 1 - x;
            let native_y = HEIGHT - 1 - y;
            let byte = frame[native_y * (WIDTH / 2) + native_x / 2];
            let index = if native_x.is_multiple_of(2) {
                byte >> 4
            } else {
                byte & 0x0F
            };
            let colour = PALETTE
                .iter()
                .find(|entry| entry.0 == index)
                .map_or(BLACK, |entry| entry.1);
            rgb[(y * WIDTH + x) * 3..][..3].copy_from_slice(&colour);
        }
    }
    rgb
}

/// RGB to a PNG file.
pub fn png(rgb: &[u8], width: usize, height: usize) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, width as u32, height as u32);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().map_err(|error| error.to_string())?;
        writer
            .write_image_data(rgb)
            .map_err(|error| error.to_string())?;
    }
    Ok(out)
}

// ---- fonts and drawing ------------------------------------------------------

/// The two faces cards use.
pub struct Fonts {
    regular: FontVec,
    bold: FontVec,
}

/// Where fonts are looked for when the settings name none.
const FONT_DIRS: &[&str] = &[
    "/usr/share/fonts/dejavu-sans-fonts",
    "/usr/share/fonts/truetype/dejavu",
    "/usr/share/fonts/TTF",
    "/usr/share/fonts/dejavu",
];

impl Fonts {
    /// DejaVu Sans and DejaVu Sans Bold from `dir`, or from the usual places.
    pub fn load(dir: Option<&str>) -> Result<Self, String> {
        let dirs: Vec<&str> = dir.map_or_else(|| FONT_DIRS.to_vec(), |dir| vec![dir]);
        for dir in dirs {
            let path = std::path::Path::new(dir);
            let (Ok(regular), Ok(bold)) = (
                std::fs::read(path.join("DejaVuSans.ttf")),
                std::fs::read(path.join("DejaVuSans-Bold.ttf")),
            ) else {
                continue;
            };
            return Ok(Self {
                regular: FontVec::try_from_vec(regular).map_err(|error| error.to_string())?,
                bold: FontVec::try_from_vec(bold).map_err(|error| error.to_string())?,
            });
        }
        Err("DejaVu Sans not found; install the dejavu-sans fonts or set `font_dir`".to_owned())
    }
}

#[derive(Clone, Copy)]
pub enum Face {
    Regular,
    Bold,
}

/// A viewer-order RGB canvas the size of the panel.
pub struct Canvas {
    pub rgb: Vec<u8>,
    /// When set, nothing is drawn outside this rectangle.
    clip: Option<Rect>,
}

impl Canvas {
    pub fn new(background: [u8; 3]) -> Self {
        Self {
            rgb: background.repeat(WIDTH * HEIGHT),
            clip: None,
        }
    }

    /// Keep every later drawing inside `rect`.
    pub fn clip(&mut self, rect: Rect) {
        self.clip = Some(rect);
    }

    pub fn unclip(&mut self) {
        self.clip = None;
    }

    fn inside(&self, x: i32, y: i32) -> bool {
        if !(0..WIDTH as i32).contains(&x) || !(0..HEIGHT as i32).contains(&y) {
            return false;
        }
        match self.clip {
            None => true,
            Some(rect) => {
                x >= rect.x as i32
                    && y >= rect.y as i32
                    && x < rect.right() as i32
                    && y < rect.bottom() as i32
            }
        }
    }

    pub fn fill(&mut self, x: usize, y: usize, width: usize, height: usize, colour: [u8; 3]) {
        let mut x0 = x.min(WIDTH);
        let mut y0 = y.min(HEIGHT);
        let mut x1 = x.saturating_add(width).min(WIDTH);
        let mut y1 = y.saturating_add(height).min(HEIGHT);
        if let Some(rect) = self.clip {
            x0 = x0.max(rect.x);
            y0 = y0.max(rect.y);
            x1 = x1.min(rect.right());
            y1 = y1.min(rect.bottom());
        }
        for row in y0..y1 {
            for column in x0..x1 {
                self.rgb[(row * WIDTH + column) * 3..][..3].copy_from_slice(&colour);
            }
        }
    }

    /// Copy palette indices (viewer order, `width` x `height`) into the canvas.
    fn blit_indices(&mut self, x: usize, y: usize, width: usize, height: usize, indices: &[u8]) {
        for row in 0..height {
            for column in 0..width {
                let index = indices[row * width + column];
                let colour = PALETTE
                    .iter()
                    .find(|entry| entry.0 == index)
                    .map_or(BLACK, |entry| entry.1);
                self.put((x + column) as i32, (y + row) as i32, colour);
            }
        }
    }

    /// Draw one line of text with its baseline at `baseline`; returns its width.
    /// Coverage above one half is ink, below is paper: no grey for the panel to
    /// dither into a speckled edge.
    pub fn text(
        &mut self,
        fonts: &Fonts,
        (face, size): (Face, f32),
        (x, baseline): (f32, f32),
        line: &str,
        colour: [u8; 3],
    ) -> f32 {
        let font = match face {
            Face::Regular => &fonts.regular,
            Face::Bold => &fonts.bold,
        };
        let scaled = font.as_scaled(PxScale::from(size));
        let mut caret = x;
        let mut previous = None;
        for character in line.chars() {
            let id = scaled.glyph_id(character);
            if let Some(previous) = previous {
                caret += scaled.kern(previous, id);
            }
            let glyph = id.with_scale_and_position(size, ab_glyph::point(caret, baseline));
            caret += scaled.h_advance(id);
            previous = Some(id);
            let Some(outline) = font.outline_glyph(glyph) else {
                continue;
            };
            let bounds = outline.px_bounds();
            outline.draw(|gx, gy, coverage| {
                if coverage < 0.5 {
                    return;
                }
                let px = bounds.min.x as i32 + gx as i32;
                let py = bounds.min.y as i32 + gy as i32;
                if self.inside(px, py) {
                    let at = (py as usize * WIDTH + px as usize) * 3;
                    self.rgb[at..at + 3].copy_from_slice(&colour);
                }
            });
        }
        caret - x
    }
}

/// Width of `line` in pixels.
fn measure(fonts: &Fonts, face: Face, size: f32, line: &str) -> f32 {
    let font = match face {
        Face::Regular => &fonts.regular,
        Face::Bold => &fonts.bold,
    };
    let scaled = font.as_scaled(PxScale::from(size));
    let mut width = 0.0;
    let mut previous = None;
    for character in line.chars() {
        let id = scaled.glyph_id(character);
        if let Some(previous) = previous {
            width += scaled.kern(previous, id);
        }
        width += scaled.h_advance(id);
        previous = Some(id);
    }
    width
}

/// Break `text` into lines no wider than `max`; paragraphs stay apart, and a
/// word longer than a line is cut where it overflows.
fn wrap(fonts: &Fonts, face: Face, size: f32, text: &str, max: f32) -> Vec<String> {
    let mut lines = Vec::new();
    for paragraph in text.split('\n') {
        let mut line = String::new();
        for word in paragraph.split_whitespace() {
            let candidate = if line.is_empty() {
                word.to_owned()
            } else {
                format!("{line} {word}")
            };
            if measure(fonts, face, size, &candidate) <= max {
                line = candidate;
                continue;
            }
            if !line.is_empty() {
                lines.push(std::mem::take(&mut line));
            }
            // A single word wider than the line: cut it.
            let mut piece = String::new();
            for character in word.chars() {
                piece.push(character);
                if measure(fonts, face, size, &piece) > max {
                    piece.pop();
                    lines.push(std::mem::take(&mut piece));
                    piece.push(character);
                }
            }
            line = piece;
        }
        lines.push(line);
    }
    lines
}

/// Keep at most `rows` lines, ending the last kept one with an ellipsis when
/// something was dropped.
/// `text` on one line: whitespace collapsed, cut by characters to `max` with
/// an ellipsis. Unlike wrapping, a long word after a short one is kept as far as
/// it fits instead of disappearing onto a line that is never drawn.
fn one_line(fonts: &Fonts, face: Face, size: f32, text: &str, max: f32) -> String {
    let mut line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if measure(fonts, face, size, &line) <= max {
        return line;
    }
    while !line.is_empty() && measure(fonts, face, size, &format!("{line}…")) > max {
        line.pop();
    }
    format!("{}…", line.trim_end())
}

fn fit_rows(
    fonts: &Fonts,
    face: Face,
    size: f32,
    mut lines: Vec<String>,
    rows: usize,
    max: f32,
) -> Vec<String> {
    if lines.len() <= rows {
        return lines;
    }
    lines.truncate(rows);
    if let Some(last) = lines.last_mut() {
        while !last.is_empty() && measure(fonts, face, size, &format!("{last}…")) > max {
            last.pop();
        }
        last.push('…');
    }
    lines
}

// ---- cards --------------------------------------------------------------------

/// A palette colour by name, for cards.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Colour {
    Black,
    #[default]
    Blue,
    Red,
    Green,
    Yellow,
}

impl Colour {
    fn rgb(self) -> [u8; 3] {
        match self {
            Self::Black => BLACK,
            Self::Blue => BLUE,
            Self::Red => RED,
            Self::Green => GREEN,
            Self::Yellow => YELLOW,
        }
    }

    /// Text that reads on this colour.
    fn ink(self) -> [u8; 3] {
        match self {
            Self::Yellow | Self::Green => BLACK,
            _ => WHITE,
        }
    }
}

/// The state of one list item, shown as a coloured square.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Ok,
    Fail,
    Warn,
    Info,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Item {
    pub text: String,
    #[serde(default)]
    pub status: Option<Status>,
    /// Tallies drawn at the right end of the row, e.g. a PR's checks.
    #[serde(default)]
    pub counts: Option<Counts>,
}

/// How many of something ended which way; zero tallies are not drawn.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Counts {
    #[serde(default)]
    pub fail: u32,
    #[serde(default)]
    pub running: u32,
    #[serde(default)]
    pub ok: u32,
    /// Cancelled, skipped, neutral: neither good nor bad.
    #[serde(default)]
    pub other: u32,
}

impl Counts {
    /// Colour and number of each tally worth drawing, worst first.
    fn tallies(self) -> Vec<([u8; 3], u32)> {
        [
            (RED, self.fail),
            (YELLOW, self.running),
            (GREEN, self.ok),
            (BLACK, self.other),
        ]
        .into_iter()
        .filter(|(_, count)| *count > 0)
        .collect()
    }
}

/// Width of a row's tallies at `size`: a square, a gap, the number, a gap.
fn tallies_width(fonts: &Fonts, size: f32, counts: Counts) -> f32 {
    counts
        .tallies()
        .iter()
        .map(|(_, count)| {
            TALLY_SQUARE + 6.0 + measure(fonts, Face::Regular, size, &count.to_string()) + 14.0
        })
        .sum()
}

const TALLY_SQUARE: f32 = 16.0;

/// Structured content a plugin or the UI asks to show (design section 5.3).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "layout", rename_all = "lowercase", deny_unknown_fields)]
pub enum Card {
    /// A title and a paragraph.
    Notice {
        title: String,
        #[serde(default)]
        body: String,
        #[serde(default)]
        accent: Colour,
    },
    /// A title and items, each with an optional status.
    List {
        title: String,
        items: Vec<Item>,
        #[serde(default)]
        accent: Colour,
    },
    /// A few days of weather, one column each.
    Forecast {
        title: String,
        days: Vec<Day>,
        #[serde(default)]
        note: String,
        #[serde(default)]
        accent: Colour,
    },
}

impl Card {
    /// The card's title, for the assistant to name what it shows.
    pub fn title(&self) -> &str {
        match self {
            Self::Notice { title, .. }
            | Self::List { title, .. }
            | Self::Forecast { title, .. } => title,
        }
    }
}

/// The weather pictures the renderer can draw; a plugin maps its source's codes
/// onto these.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Icon {
    Clear,
    Partly,
    Cloudy,
    Fog,
    Drizzle,
    Rain,
    Snow,
    Thunder,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Day {
    /// "Пн", "Завтра", "3.11".
    pub label: String,
    pub icon: Icon,
    #[serde(default)]
    pub high: Option<f32>,
    #[serde(default)]
    pub low: Option<f32>,
    /// Precipitation in millimetres.
    #[serde(default)]
    pub precip_mm: Option<f32>,
}

impl Eq for Day {}

const MARGIN: f32 = 40.0;
const BAND: usize = 104;
const TITLE_SIZE: f32 = 46.0;

/// Draw `card` for the 800x480 panel.
pub fn render_card(card: &Card, fonts: &Fonts) -> Canvas {
    let mut canvas = Canvas::new(WHITE);
    let max = WIDTH as f32 - 2.0 * MARGIN;
    let (title, accent) = match card {
        Card::Notice { title, accent, .. }
        | Card::List { title, accent, .. }
        | Card::Forecast { title, accent, .. } => (title, *accent),
    };
    canvas.fill(0, 0, WIDTH, BAND, accent.rgb());
    let title = one_line(fonts, Face::Bold, TITLE_SIZE, title, max);
    canvas.text(
        fonts,
        (Face::Bold, TITLE_SIZE),
        (MARGIN, 68.0),
        &title,
        accent.ink(),
    );
    match card {
        Card::Notice { body, .. } => {
            let size = 34.0;
            let leading = size * 1.3;
            let rows = ((HEIGHT - BAND) as f32 - MARGIN) / leading;
            let lines = fit_rows(
                fonts,
                Face::Regular,
                size,
                wrap(fonts, Face::Regular, size, body, max),
                rows as usize,
                max,
            );
            for (row, line) in lines.iter().enumerate() {
                let baseline = BAND as f32 + MARGIN + size + row as f32 * leading;
                canvas.text(
                    fonts,
                    (Face::Regular, size),
                    (MARGIN, baseline),
                    line,
                    BLACK,
                );
            }
        }
        Card::List { items, .. } => {
            let size = 30.0;
            let row_height = 52.0;
            let rows = (((HEIGHT - BAND) as f32 - 20.0) / row_height) as usize;
            let shown = if items.len() > rows {
                rows - 1
            } else {
                items.len()
            };
            let square = 22;
            // Once any item has a status, all text starts in the same column.
            let indented = items.iter().any(|item| item.status.is_some());
            for (row, item) in items.iter().take(shown).enumerate() {
                let top = BAND as f32 + 14.0 + row as f32 * row_height;
                let mut x = MARGIN;
                if let Some(status) = item.status {
                    let colour = match status {
                        Status::Ok => GREEN,
                        Status::Fail => RED,
                        Status::Warn => YELLOW,
                        Status::Info => BLUE,
                    };
                    // A black outline keeps yellow and green readable on white.
                    canvas.fill(x as usize, (top + 12.0) as usize, square, square, BLACK);
                    canvas.fill(
                        x as usize + 2,
                        (top + 14.0) as usize,
                        square - 4,
                        square - 4,
                        colour,
                    );
                    x += square as f32 + 16.0;
                } else if indented {
                    x += square as f32 + 16.0;
                }
                let tally_size = 26.0;
                let tallies = item
                    .counts
                    .map_or(0.0, |counts| tallies_width(fonts, tally_size, counts));
                let room = max - (x - MARGIN) - tallies;
                let line = one_line(fonts, Face::Regular, size, &item.text, room);
                canvas.text(fonts, (Face::Regular, size), (x, top + 34.0), &line, BLACK);
                if let Some(counts) = item.counts {
                    // Right-aligned: [■] 6  [■] 2  [■] 35, worst first.
                    let mut tx = WIDTH as f32 - MARGIN - tallies + 14.0;
                    for (colour, count) in counts.tallies() {
                        let square_top = (top + 16.0) as usize;
                        canvas.fill(
                            tx as usize,
                            square_top,
                            TALLY_SQUARE as usize,
                            TALLY_SQUARE as usize,
                            BLACK,
                        );
                        canvas.fill(
                            tx as usize + 2,
                            square_top + 2,
                            TALLY_SQUARE as usize - 4,
                            TALLY_SQUARE as usize - 4,
                            colour,
                        );
                        tx += TALLY_SQUARE + 6.0;
                        let number = count.to_string();
                        let width = canvas.text(
                            fonts,
                            (Face::Regular, tally_size),
                            (tx, top + 32.0),
                            &number,
                            BLACK,
                        );
                        tx += width + 14.0;
                    }
                }
                canvas.fill(
                    MARGIN as usize,
                    (top + row_height - 4.0) as usize,
                    WIDTH - 2 * MARGIN as usize,
                    1,
                    BLACK,
                );
            }
            if shown < items.len() {
                let top = BAND as f32 + 14.0 + shown as f32 * row_height;
                let more = format!("и ещё {}", items.len() - shown);
                canvas.text(
                    fonts,
                    (Face::Regular, size),
                    (MARGIN, top + 34.0),
                    &more,
                    BLUE,
                );
            }
        }
        Card::Forecast { days, note, .. } => forecast(&mut canvas, fonts, days, note),
    }
    canvas
}

/// Signed whole degrees, the way a weather card writes them.
fn degrees(value: f32) -> String {
    let rounded = value.round() as i32;
    if rounded > 0 {
        format!("+{rounded}°")
    } else {
        format!("{rounded}°")
    }
}

fn forecast(canvas: &mut Canvas, fonts: &Fonts, days: &[Day], note: &str) {
    let shown = &days[..days.len().min(6)];
    if shown.is_empty() {
        return;
    }
    let column = (WIDTH as f32 - 2.0 * MARGIN) / shown.len() as f32;
    for (index, day) in shown.iter().enumerate() {
        let centre = MARGIN + column * (index as f32 + 0.5);
        canvas.centred(
            fonts,
            (Face::Bold, 30.0),
            (centre, 150.0),
            &day.label,
            BLACK,
        );
        canvas.icon(day.icon, centre, 220.0, 46.0);
        if let Some(high) = day.high {
            canvas.centred(
                fonts,
                (Face::Bold, 42.0),
                (centre, 318.0),
                &degrees(high),
                RED,
            );
        }
        if let Some(low) = day.low {
            canvas.centred(
                fonts,
                (Face::Regular, 32.0),
                (centre, 360.0),
                &degrees(low),
                BLUE,
            );
        }
        if let Some(precip) = day.precip_mm.filter(|mm| *mm >= 0.1) {
            canvas.centred(
                fonts,
                (Face::Regular, 24.0),
                (centre, 398.0),
                &format!("{precip:.1} мм"),
                BLUE,
            );
        }
    }
    if !note.is_empty() {
        canvas.text(
            fonts,
            (Face::Regular, 20.0),
            (MARGIN, HEIGHT as f32 - 22.0),
            note,
            BLACK,
        );
    }
}

/// A rectangle on the panel, in viewer-order pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: usize,
    pub y: usize,
    pub width: usize,
    pub height: usize,
}

impl Rect {
    pub const fn new(x: usize, y: usize, width: usize, height: usize) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub const fn right(self) -> usize {
        self.x + self.width
    }

    pub const fn bottom(self) -> usize {
        self.y + self.height
    }
}

/// Draw `card` inside `rect`: the same content as a full-panel card, in a
/// compact header and body that fit the smaller space. `stale` marks a tile
/// whose content is older than its job wants.
pub fn render_tile(canvas: &mut Canvas, rect: Rect, card: &Card, fonts: &Fonts, stale: bool) {
    if rect.width < 16 || rect.height < 16 {
        return;
    }
    canvas.clip(rect);
    canvas.fill(rect.x, rect.y, rect.width, rect.height, WHITE);
    let (title, accent) = match card {
        Card::Notice { title, accent, .. }
        | Card::List { title, accent, .. }
        | Card::Forecast { title, accent, .. } => (title, *accent),
    };
    let margin = (rect.width as f32 * 0.04).clamp(10.0, 20.0);
    let title_size: f32 = if rect.height >= 260 {
        26.0
    } else if rect.height >= 190 {
        24.0
    } else {
        22.0
    };
    let header = (title_size + 12.0).min(rect.height as f32);
    let mut body_top = rect.y as f32 + 4.0;
    if !title.is_empty() {
        let square = ((title_size * 0.55).round() as usize).max(8);
        let square_left = rect.x + margin as usize;
        let square_top = (rect.y as f32 + (header - square as f32) * 0.5) as usize;
        canvas.fill(square_left, square_top, square, square, accent.rgb());
        if stale {
            canvas.fill(
                square_left + 2,
                square_top + 2,
                square.saturating_sub(4),
                square.saturating_sub(4),
                WHITE,
            );
        }
        let text_x = square_left as f32 + square as f32 + 10.0;
        let room = (rect.right() as f32 - margin - text_x).max(20.0);
        let line = one_line(fonts, Face::Bold, title_size, title, room);
        canvas.text(
            fonts,
            (Face::Bold, title_size),
            (text_x, rect.y as f32 + (header + title_size) * 0.5 - 2.0),
            &line,
            BLACK,
        );
        body_top = rect.y as f32 + header;
    }
    let body_height = rect.bottom() as f32 - body_top;
    let max = (rect.width as f32 - 2.0 * margin).max(20.0);
    match card {
        Card::Notice { body, .. } => {
            let size = (body_height * 0.11).clamp(17.0, 28.0);
            let leading = size * 1.3;
            let rows = (((body_height - margin) / leading).floor() as usize).max(1);
            let lines = fit_rows(
                fonts,
                Face::Regular,
                size,
                wrap(fonts, Face::Regular, size, body, max),
                rows,
                max,
            );
            for (row, line) in lines.iter().enumerate() {
                canvas.text(
                    fonts,
                    (Face::Regular, size),
                    (
                        rect.x as f32 + margin,
                        body_top + margin + size + row as f32 * leading,
                    ),
                    line,
                    BLACK,
                );
            }
        }
        Card::List { items, .. } => {
            let row_height: f32 = if rect.height >= 320 {
                46.0
            } else if rect.height >= 190 {
                40.0
            } else {
                34.0
            };
            let size = (row_height * 0.56).clamp(16.0, 28.0);
            let rows = (((body_height - 4.0) / row_height).floor() as usize).max(1);
            let shown = if items.len() > rows {
                rows.saturating_sub(1).max(1)
            } else {
                items.len()
            };
            let square = ((row_height * 0.5).round() as usize).max(10);
            let indented = items.iter().any(|item| item.status.is_some());
            for (row, item) in items.iter().take(shown).enumerate() {
                let top = body_top + 4.0 + row as f32 * row_height;
                let mut x = rect.x as f32 + margin;
                if let Some(status) = item.status {
                    let colour = match status {
                        Status::Ok => GREEN,
                        Status::Fail => RED,
                        Status::Warn => YELLOW,
                        Status::Info => BLUE,
                    };
                    let square_left = x as usize;
                    let square_top = (top + (row_height - square as f32) * 0.5) as usize;
                    canvas.fill(square_left, square_top, square, square, BLACK);
                    canvas.fill(
                        square_left + 2,
                        square_top + 2,
                        square.saturating_sub(4),
                        square.saturating_sub(4),
                        colour,
                    );
                    x += square as f32 + 10.0;
                } else if indented {
                    x += square as f32 + 10.0;
                }
                let tally_size = size * 0.85;
                let tallies = item
                    .counts
                    .map_or(0.0, |counts| tallies_width(fonts, tally_size, counts));
                let room = (rect.right() as f32 - margin - x - tallies).max(20.0);
                let line = one_line(fonts, Face::Regular, size, &item.text, room);
                canvas.text(
                    fonts,
                    (Face::Regular, size),
                    (x, top + row_height * 0.68),
                    &line,
                    BLACK,
                );
                if let Some(counts) = item.counts {
                    let mut tx = rect.right() as f32 - margin - tallies + 14.0;
                    for (colour, count) in counts.tallies() {
                        let square_top = (top + (row_height - TALLY_SQUARE) * 0.5) as usize;
                        canvas.fill(
                            tx as usize,
                            square_top,
                            TALLY_SQUARE as usize,
                            TALLY_SQUARE as usize,
                            BLACK,
                        );
                        canvas.fill(
                            tx as usize + 2,
                            square_top + 2,
                            TALLY_SQUARE as usize - 4,
                            TALLY_SQUARE as usize - 4,
                            colour,
                        );
                        tx += TALLY_SQUARE + 6.0;
                        let width = canvas.text(
                            fonts,
                            (Face::Regular, tally_size),
                            (tx, top + row_height * 0.68),
                            &count.to_string(),
                            BLACK,
                        );
                        tx += width + 14.0;
                    }
                }
                canvas.fill(
                    rect.x + margin as usize,
                    (top + row_height - 1.0) as usize,
                    rect.width.saturating_sub(2 * margin as usize),
                    1,
                    BLACK,
                );
            }
            if shown < items.len() {
                let top = body_top + 4.0 + shown as f32 * row_height;
                canvas.text(
                    fonts,
                    (Face::Regular, size),
                    (rect.x as f32 + margin, top + row_height * 0.68),
                    &format!("+{}", items.len() - shown),
                    BLUE,
                );
            }
        }
        Card::Forecast { days, note, .. } => {
            let max_days = ((max / 118.0).floor() as usize).clamp(1, 7);
            let shown = days.len().min(max_days);
            if shown == 0 {
                canvas.unclip();
                return;
            }
            let column = max / shown as f32;
            let label_size = (column * 0.20).clamp(15.0, 28.0);
            let icon = (column * 0.32).clamp(22.0, 44.0);
            let high_size = icon * 0.85;
            let low_size = icon * 0.62;
            let note_room = if note.is_empty() { 0.0 } else { 20.0 };
            let usable = (body_height - note_room).max(40.0);
            for (index, day) in days.iter().take(shown).enumerate() {
                let centre = rect.x as f32 + margin + column * (index as f32 + 0.5);
                canvas.centred(
                    fonts,
                    (Face::Bold, label_size),
                    (centre, body_top + label_size + 4.0),
                    &day.label,
                    BLACK,
                );
                canvas.icon(day.icon, centre, body_top + usable * 0.36, icon);
                if let Some(high) = day.high {
                    canvas.centred(
                        fonts,
                        (Face::Bold, high_size),
                        (centre, body_top + usable * 0.70),
                        &degrees(high),
                        RED,
                    );
                }
                if let Some(low) = day.low {
                    canvas.centred(
                        fonts,
                        (Face::Regular, low_size),
                        (centre, body_top + usable * 0.86),
                        &degrees(low),
                        BLUE,
                    );
                }
                if let Some(precip) = day.precip_mm.filter(|mm| *mm >= 0.1)
                    && usable > 150.0
                {
                    let size = (low_size * 0.8).clamp(12.0, 20.0);
                    canvas.centred(
                        fonts,
                        (Face::Regular, size),
                        (centre, body_top + usable * 0.98),
                        &format!("{precip:.1} мм"),
                        BLUE,
                    );
                }
            }
            if !note.is_empty() {
                let size = 16.0;
                let line = one_line(fonts, Face::Regular, size, note, max);
                canvas.text(
                    fonts,
                    (Face::Regular, size),
                    (rect.x as f32 + margin, rect.bottom() as f32 - 6.0),
                    &line,
                    BLACK,
                );
            }
        }
    }
    canvas.unclip();
}

/// Decode a picture and draw it inside `rect`, dithered only within the tile.
pub fn draw_picture(canvas: &mut Canvas, rect: Rect, bytes: &[u8], fit: Fit) -> Result<(), String> {
    if rect.width < 8 || rect.height < 8 {
        return Ok(());
    }
    let rgb = picture_rgb(bytes, fit, rect.width, rect.height)?;
    let indices = palette_indices(&rgb, rect.width, rect.height, true);
    canvas.clip(rect);
    canvas.blit_indices(rect.x, rect.y, rect.width, rect.height, &indices);
    canvas.unclip();
    Ok(())
}

impl Canvas {
    fn put(&mut self, x: i32, y: i32, colour: [u8; 3]) {
        if self.inside(x, y) {
            let at = (y as usize * WIDTH + x as usize) * 3;
            self.rgb[at..at + 3].copy_from_slice(&colour);
        }
    }

    fn disc(&mut self, cx: f32, cy: f32, radius: f32, colour: [u8; 3]) {
        let r = radius.ceil() as i32;
        for dy in -r..=r {
            for dx in -r..=r {
                if (dx * dx + dy * dy) as f32 <= radius * radius {
                    self.put(cx as i32 + dx, cy as i32 + dy, colour);
                }
            }
        }
    }

    /// A filled triangle (edge function test), for the lightning bolt.
    fn triangle(&mut self, points: [(f32, f32); 3], colour: [u8; 3]) {
        let edge = |a: (f32, f32), b: (f32, f32), p: (f32, f32)| {
            (b.0 - a.0) * (p.1 - a.1) - (b.1 - a.1) * (p.0 - a.0)
        };
        let min_x = points.iter().map(|p| p.0).fold(f32::MAX, f32::min).floor() as i32;
        let max_x = points.iter().map(|p| p.0).fold(f32::MIN, f32::max).ceil() as i32;
        let min_y = points.iter().map(|p| p.1).fold(f32::MAX, f32::min).floor() as i32;
        let max_y = points.iter().map(|p| p.1).fold(f32::MIN, f32::max).ceil() as i32;
        for y in min_y..=max_y {
            for x in min_x..=max_x {
                let p = (x as f32 + 0.5, y as f32 + 0.5);
                let [a, b, c] = points;
                let (w0, w1, w2) = (edge(b, c, p), edge(c, a, p), edge(a, b, p));
                if (w0 >= 0.0 && w1 >= 0.0 && w2 >= 0.0) || (w0 <= 0.0 && w1 <= 0.0 && w2 <= 0.0) {
                    self.put(x, y, colour);
                }
            }
        }
    }

    /// A cloud: three discs on a base, outlined in black and filled white.
    fn cloud(&mut self, cx: f32, cy: f32, size: f32) {
        let parts = [
            (cx - size * 0.45, cy + size * 0.1, size * 0.38),
            (cx, cy - size * 0.2, size * 0.5),
            (cx + size * 0.45, cy + size * 0.1, size * 0.38),
        ];
        let base = (cx - size * 0.82, cy + size * 0.1, size * 1.64, size * 0.38);
        for (inset, colour) in [(0.0, BLACK), (3.0, WHITE)] {
            for (x, y, r) in parts {
                self.disc(x, y, r - inset, colour);
            }
            self.fill(
                (base.0 + inset) as usize,
                base.1 as usize,
                (base.2 - 2.0 * inset) as usize,
                (base.3 - inset) as usize,
                colour,
            );
        }
    }

    fn sun(&mut self, cx: f32, cy: f32, radius: f32) {
        self.disc(cx, cy, radius, BLACK);
        self.disc(cx, cy, radius - 3.0, YELLOW);
    }

    /// One weather picture in a box of `size` around (`cx`, `cy`).
    fn icon(&mut self, icon: Icon, cx: f32, cy: f32, size: f32) {
        match icon {
            Icon::Clear => self.sun(cx, cy, size * 0.6),
            Icon::Partly => {
                self.sun(cx + size * 0.35, cy - size * 0.3, size * 0.45);
                self.cloud(cx - size * 0.1, cy + size * 0.15, size * 0.75);
            }
            Icon::Cloudy => self.cloud(cx, cy, size),
            Icon::Fog => {
                self.cloud(cx, cy - size * 0.15, size * 0.85);
                for row in 0..3 {
                    let y = cy + size * 0.45 + row as f32 * 9.0;
                    self.fill(
                        (cx - size * 0.8) as usize,
                        y as usize,
                        (size * 1.6) as usize,
                        4,
                        BLACK,
                    );
                }
            }
            Icon::Drizzle | Icon::Rain => {
                self.cloud(cx, cy - size * 0.2, size * 0.9);
                let drops = if icon == Icon::Rain { 4 } else { 2 };
                for drop in 0..drops {
                    let x = cx - size * 0.45 + drop as f32 * size * 0.9 / (drops - 1).max(1) as f32;
                    for step in 0..12 {
                        self.disc(
                            x - step as f32 * 0.4,
                            cy + size * 0.42 + step as f32 * 1.6,
                            2.5,
                            BLUE,
                        );
                    }
                }
            }
            Icon::Snow => {
                self.cloud(cx, cy - size * 0.2, size * 0.9);
                for flake in 0..3 {
                    let x = cx - size * 0.5 + flake as f32 * size * 0.5;
                    let y = cy + size * 0.6;
                    self.fill((x - 8.0) as usize, (y - 2.0) as usize, 16, 4, BLUE);
                    self.fill((x - 2.0) as usize, (y - 8.0) as usize, 4, 16, BLUE);
                }
            }
            Icon::Thunder => {
                self.cloud(cx, cy - size * 0.25, size * 0.9);
                let (x, y) = (cx, cy + size * 0.25);
                self.triangle(
                    [
                        (x + 4.0, y - 4.0),
                        (x - 14.0, y + 22.0),
                        (x + 2.0, y + 18.0),
                    ],
                    RED,
                );
                self.triangle(
                    [
                        (x + 2.0, y + 12.0),
                        (x + 10.0, y + 12.0),
                        (x - 6.0, y + 44.0),
                    ],
                    RED,
                );
            }
        }
    }

    /// Text centred on `cx`.
    fn centred(
        &mut self,
        fonts: &Fonts,
        style: (Face, f32),
        (cx, baseline): (f32, f32),
        line: &str,
        colour: [u8; 3],
    ) {
        let width = measure(fonts, style.0, style.1, line);
        self.text(fonts, style, (cx - width / 2.0, baseline), line, colour);
    }
}

/// How a photograph fills the panel.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Fit {
    /// Fill the panel, cropping the centre (what `convert.py` does).
    #[default]
    Cover,
    /// Show the whole picture, letterboxed in white.
    Contain,
}

/// Largest picture accepted for decoding, in pixels: a 50-megapixel photo is
/// still fine, a decompression bomb is not.
const MAX_SOURCE_PIXELS: u64 = 50_000_000;

/// A PNG or JPEG, fitted to `width` x `height` in viewer order, not yet
/// quantised.
fn picture_rgb(
    bytes: &[u8],
    fit: Fit,
    target_width: usize,
    target_height: usize,
) -> Result<Vec<u8>, String> {
    let reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|error| error.to_string())?;
    let (source_width, source_height) = reader
        .into_dimensions()
        .map_err(|error| format!("picture: {error}"))?;
    if u64::from(source_width) * u64::from(source_height) > MAX_SOURCE_PIXELS {
        return Err(format!(
            "picture is {source_width}x{source_height}, too large"
        ));
    }
    let picture = image::load_from_memory(bytes)
        .map_err(|error| format!("picture: {error}"))?
        .to_rgb8();
    let target_w = target_width as u32;
    let target_h = target_height as u32;
    let fitted = match fit {
        Fit::Cover => image::imageops::resize(
            &crop_to_ratio(&picture, target_w, target_h),
            target_w,
            target_h,
            image::imageops::FilterType::Lanczos3,
        ),
        Fit::Contain => {
            let scale = f64::min(
                f64::from(target_w) / f64::from(picture.width()),
                f64::from(target_h) / f64::from(picture.height()),
            );
            let w = ((f64::from(picture.width()) * scale).round() as u32).clamp(1, target_w);
            let h = ((f64::from(picture.height()) * scale).round() as u32).clamp(1, target_h);
            let small =
                image::imageops::resize(&picture, w, h, image::imageops::FilterType::Lanczos3);
            let mut canvas = image::RgbImage::from_pixel(target_w, target_h, image::Rgb(WHITE));
            image::imageops::overlay(
                &mut canvas,
                &small,
                i64::from((target_w - w) / 2),
                i64::from((target_h - h) / 2),
            );
            canvas
        }
    };
    Ok(fitted.into_raw())
}

/// A PNG or JPEG, fitted to the panel and dithered.
pub fn photo_frame(bytes: &[u8], fit: Fit) -> Result<Vec<u8>, String> {
    let rgb = picture_rgb(bytes, fit, WIDTH, HEIGHT)?;
    Ok(frame_from_rgb(&rgb, true))
}

/// The centred part of `picture` with the panel's aspect ratio.
fn crop_to_ratio(picture: &image::RgbImage, width: u32, height: u32) -> image::RgbImage {
    let (w, h) = picture.dimensions();
    let (crop_w, crop_h) = if u64::from(w) * u64::from(height) > u64::from(h) * u64::from(width) {
        (
            (u64::from(h) * u64::from(width) / u64::from(height)) as u32,
            h,
        )
    } else {
        (
            w,
            (u64::from(w) * u64::from(height) / u64::from(width)) as u32,
        )
    };
    image::imageops::crop_imm(
        picture,
        (w - crop_w) / 2,
        (h - crop_h) / 2,
        crop_w.max(1),
        crop_h.max(1),
    )
    .to_image()
}

/// A card as the panel's bytes: every pixel is already a palette colour, so it
/// is packed without dithering.
pub fn card_frame(card: &Card, fonts: &Fonts) -> Vec<u8> {
    frame_from_rgb(&render_card(card, fonts).rgb, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::process::{Command, Stdio};

    /// Deterministic pixels: gradients, exact palette blocks and noise, the
    /// cases `check-el6-js.py` uses for the browser converter.
    fn test_pixels() -> Vec<u8> {
        let mut seed = 0x2446_u32;
        let mut rgb = Vec::with_capacity(WIDTH * HEIGHT * 3);
        for y in 0..HEIGHT {
            for x in 0..WIDTH {
                let pixel = if y < 160 {
                    [(x * 255 / (WIDTH - 1)) as u8, (y * 255 / 159) as u8, 128]
                } else if y < 240 {
                    PALETTE[(x / 134).min(5)].1
                } else {
                    seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                    let [a, b, c, _] = seed.to_le_bytes();
                    [a, b, c]
                };
                rgb.extend_from_slice(&pixel);
            }
        }
        rgb
    }

    fn python_frame(rgb: &[u8], dither: bool) -> Vec<u8> {
        let tools = concat!(env!("CARGO_MANIFEST_DIR"), "/../tools");
        let script = format!(
            "import sys; sys.path.insert(0, {tools:?}); import convert; \
             sys.stdout.buffer.write(convert.frame_from_rgb(sys.stdin.buffer.read(), dither={}))",
            if dither { "True" } else { "False" }
        );
        let mut child = Command::new("python3")
            .args(["-I", "-c", &script])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("python3 runs tools/convert.py");
        child.stdin.take().unwrap().write_all(rgb).unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success());
        output.stdout
    }

    #[test]
    fn the_spectra_path_matches_convert_py_byte_for_byte() {
        let rgb = test_pixels();
        for dither in [true, false] {
            assert!(
                frame_from_rgb(&rgb, dither) == python_frame(&rgb, dither),
                "dither={dither}: bridge and convert.py disagree"
            );
        }
    }

    #[test]
    fn unpack_inverts_pack_for_palette_pixels() {
        let rgb: Vec<u8> = (0..WIDTH * HEIGHT)
            .flat_map(|pixel| PALETTE[pixel % 6].1)
            .collect();
        assert_eq!(unpack(&frame_from_rgb(&rgb, false)), rgb);
    }

    #[test]
    fn cards_use_only_palette_colours_and_fit() {
        let fonts = Fonts::load(None).expect("DejaVu Sans on this host");
        let long = "Очень длинный текст ".repeat(80);
        let cards = [
            Card::Notice {
                title: "Батарея разряжена".to_owned(),
                body: long.clone(),
                accent: Colour::Red,
            },
            Card::List {
                title: "CI".to_owned(),
                items: (0..20)
                    .map(|n| Item {
                        text: format!("сборка {n} {long}"),
                        status: Some([Status::Ok, Status::Fail, Status::Warn, Status::Info][n % 4]),
                        counts: (n % 2 == 0).then_some(Counts {
                            fail: n as u32,
                            running: 2,
                            ok: 35,
                            other: 1,
                        }),
                    })
                    .collect(),
                accent: Colour::Yellow,
            },
            Card::Forecast {
                title: "Хельсинки".to_owned(),
                days: [
                    Icon::Clear,
                    Icon::Partly,
                    Icon::Cloudy,
                    Icon::Fog,
                    Icon::Rain,
                    Icon::Thunder,
                ]
                .into_iter()
                .enumerate()
                .map(|(n, icon)| Day {
                    label: format!("Д{n}"),
                    icon,
                    high: Some(5.0 - n as f32),
                    low: Some(-3.0),
                    precip_mm: Some(1.5),
                })
                .collect(),
                note: "обновлено 10:00".to_owned(),
                accent: Colour::Blue,
            },
        ];
        for card in &cards {
            let canvas = render_card(card, &fonts);
            for pixel in canvas.rgb.chunks_exact(3) {
                assert!(PALETTE.iter().any(|entry| entry.1 == pixel), "{pixel:?}");
            }
            // Something was drawn below the title band.
            let body = &canvas.rgb[(BAND + 10) * WIDTH * 3..];
            assert!(body.chunks_exact(3).any(|pixel| pixel == BLACK));
        }
    }

    #[test]
    fn tallies_take_their_room_from_the_text() {
        let fonts = Fonts::load(None).expect("DejaVu Sans on this host");
        let counts = Counts {
            fail: 6,
            running: 0,
            ok: 35,
            other: 3,
        };
        assert_eq!(counts.tallies().len(), 3, "a zero tally is not drawn");
        let card: Card = serde_json::from_str(
            r##"{"layout":"list","title":"PR","items":[{"text":"#8535 ipatests","status":"fail","counts":{"fail":6,"ok":35,"other":3}}]}"##,
        )
        .unwrap();
        let canvas = render_card(&card, &fonts);
        // The rightmost tally is near the right margin, in its colour.
        let row = BAND + 14 + 22;
        let reds = (WIDTH / 2..WIDTH)
            .filter(|x| canvas.rgb[(row * WIDTH + x) * 3..][..3] == RED)
            .count();
        assert!(reds > 0, "the failed tally is drawn on the right half");
        assert!(tallies_width(&fonts, 26.0, counts) > 100.0);
    }

    #[test]
    fn a_long_item_keeps_what_fits() {
        let fonts = Fonts::load(None).expect("DejaVu Sans on this host");
        let cut = one_line(
            &fonts,
            Face::Regular,
            30.0,
            &format!("Crabwatch · {}", "x".repeat(200)),
            600.0,
        );
        assert!(
            cut.starts_with("Crabwatch · xxx") && cut.ends_with('…'),
            "{cut}"
        );
        assert!(measure(&fonts, Face::Regular, 30.0, &cut) <= 600.0);
        assert_eq!(
            one_line(&fonts, Face::Regular, 30.0, "  short\n text ", 600.0),
            "short text"
        );
    }

    #[test]
    fn a_card_parses_from_plugin_json() {
        let card: Card = serde_json::from_str(
            r#"{"layout":"list","title":"CI","items":[{"text":"main","status":"ok"}]}"#,
        )
        .unwrap();
        assert!(
            matches!(card, Card::List { ref items, .. } if items[0].status == Some(Status::Ok))
        );
        assert!(serde_json::from_str::<Card>(r#"{"layout":"poster","title":"x"}"#).is_err());
    }

    #[test]
    fn a_photo_fits_either_way() {
        // A wide red/blue picture: cover crops it, contain letterboxes it in white.
        let mut picture = image::RgbImage::new(1600, 400);
        for (x, _, pixel) in picture.enumerate_pixels_mut() {
            *pixel = image::Rgb(if x < 800 { RED } else { BLUE });
        }
        let mut encoded = Vec::new();
        image::DynamicImage::ImageRgb8(picture)
            .write_to(
                &mut std::io::Cursor::new(&mut encoded),
                image::ImageFormat::Png,
            )
            .unwrap();
        let cover = unpack(&photo_frame(&encoded, Fit::Cover).unwrap());
        let contain = unpack(&photo_frame(&encoded, Fit::Contain).unwrap());
        let at = |rgb: &[u8], x: usize, y: usize| rgb[(y * WIDTH + x) * 3..][..3].to_vec();
        assert_eq!(at(&cover, 10, 10), RED);
        assert_eq!(at(&cover, 790, 470), BLUE);
        assert_eq!(at(&contain, 400, 10), WHITE, "letterbox above");
        assert_eq!(at(&contain, 10, 240), RED);
        assert!(photo_frame(b"not a picture", Fit::Cover).is_err());
    }

    fn tile_card() -> Card {
        Card::List {
            title: "Электричество".to_owned(),
            items: vec![
                Item {
                    text: "Сейчас 0,67 c/кВт·ч (НДС 25,5 %)".to_owned(),
                    status: Some(Status::Ok),
                    counts: None,
                },
                Item {
                    text: "Дешевле всего 06:00–08:00 · 0,88".to_owned(),
                    status: None,
                    counts: None,
                },
            ],
            accent: Colour::Blue,
        }
    }

    fn notice_tile_card() -> Card {
        Card::Notice {
            title: "Заметка".to_owned(),
            body: "Длинный текст, который переносится по строкам и остаётся в своей области. "
                .repeat(3),
            accent: Colour::Green,
        }
    }

    fn forecast_tile_card() -> Card {
        Card::Forecast {
            title: "Хельсинки".to_owned(),
            days: [Icon::Clear, Icon::Partly, Icon::Rain, Icon::Snow]
                .into_iter()
                .enumerate()
                .map(|(n, icon)| Day {
                    label: format!("Д{n}"),
                    icon,
                    high: Some(5.0 - n as f32),
                    low: Some(-3.0),
                    precip_mm: Some(1.5),
                })
                .collect(),
            note: "обновлено 10:00".to_owned(),
            accent: Colour::Blue,
        }
    }

    #[test]
    fn a_tile_stays_inside_its_rectangle() {
        let fonts = Fonts::load(None).expect("DejaVu Sans on this host");
        let mut canvas = Canvas::new(WHITE);
        let rect = Rect::new(401, 0, 399, 480);
        render_tile(&mut canvas, rect, &tile_card(), &fonts, false);
        for y in 0..HEIGHT {
            for x in 0..rect.x {
                let at = (y * WIDTH + x) * 3;
                assert_eq!(
                    &canvas.rgb[at..at + 3],
                    &WHITE,
                    "a tile drew at {x},{y}, outside its rectangle"
                );
            }
        }
        assert!(
            canvas.rgb.chunks_exact(3).any(|pixel| pixel == BLACK),
            "the tile drew nothing"
        );
    }

    #[test]
    fn tiles_draw_every_layout_in_the_palette() {
        let fonts = Fonts::load(None).expect("DejaVu Sans on this host");
        let cards = [notice_tile_card(), tile_card(), forecast_tile_card()];
        for card in &cards {
            for rect in [
                Rect::new(0, 0, 800, 258),
                Rect::new(0, 260, 399, 220),
                Rect::new(0, 0, 399, 239),
                Rect::new(401, 0, 399, 480),
            ] {
                let mut canvas = Canvas::new(BLACK);
                render_tile(&mut canvas, rect, card, &fonts, false);
                for y in rect.y..rect.bottom() {
                    for x in rect.x..rect.right() {
                        let at = (y * WIDTH + x) * 3;
                        let pixel = &canvas.rgb[at..at + 3];
                        assert!(
                            PALETTE.iter().any(|entry| entry.1 == pixel),
                            "{pixel:?} at {x},{y} is not a palette colour"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn a_picture_cannot_speckle_the_card_beside_it() {
        let fonts = Fonts::load(None).expect("DejaVu Sans on this host");
        let mut picture = image::RgbImage::new(64, 64);
        let mut seed = 0x51ee_d0ff_u32;
        for pixel in picture.pixels_mut() {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            let [r, g, b, _] = seed.to_le_bytes();
            *pixel = image::Rgb([r, g, b]);
        }
        let mut encoded = Vec::new();
        image::DynamicImage::ImageRgb8(picture)
            .write_to(
                &mut std::io::Cursor::new(&mut encoded),
                image::ImageFormat::Png,
            )
            .unwrap();
        let left = Rect::new(0, 0, 399, 480);
        let right = Rect::new(401, 0, 399, 480);
        let draw = |with_picture: bool| {
            let mut canvas = Canvas::new(WHITE);
            if with_picture {
                draw_picture(&mut canvas, left, &encoded, Fit::Cover).unwrap();
            }
            render_tile(&mut canvas, right, &tile_card(), &fonts, false);
            canvas
        };
        let with_picture = draw(true);
        let blank = draw(false);
        for y in 0..HEIGHT {
            let from = (y * WIDTH + right.x) * 3;
            let to = (y * WIDTH + right.right()) * 3;
            assert_eq!(
                &with_picture.rgb[from..to],
                &blank.rgb[from..to],
                "dithering from the picture reached row {y} of the card"
            );
        }
    }

    #[test]
    fn a_stale_tile_looks_different() {
        let fonts = Fonts::load(None).expect("DejaVu Sans on this host");
        let rect = Rect::new(0, 0, 399, 480);
        let mut fresh = Canvas::new(WHITE);
        let mut stale = Canvas::new(WHITE);
        render_tile(&mut fresh, rect, &tile_card(), &fonts, false);
        render_tile(&mut stale, rect, &tile_card(), &fonts, true);
        assert_ne!(fresh.rgb, stale.rgb, "the stale marker is invisible");
    }

    #[test]
    fn a_png_preview_encodes() {
        let png = png(&unpack(&vec![0x11; FRAME_BYTES]), WIDTH, HEIGHT).unwrap();
        assert_eq!(&png[1..4], b"PNG");
    }
}
