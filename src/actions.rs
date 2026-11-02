//! What the bridge may ask the frame to do after an answer, and the readings it
//! may ask the frame to speak.
//!
//! The bridge recognises commands for the frame («следующая картинка»,
//! «выключи слайд-шоу», «какая температура?») and answers with words to speak
//! plus, for commands, an `X-Action` header. The frame executes only the fixed
//! list below; anything else is ignored, so the bridge cannot make the frame do
//! more than a person at it could with the buttons and the console.
//!
//! A reply about the room carries placeholders the frame fills from its own
//! sensors, since only the frame knows them: `{temp}`, `{humidity}` and
//! `{battery}` become «21 градус», «45 процентов», «80 процентов». Kept free of
//! ESP-IDF so it is tested on the host.

/// One command from the bridge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// The next picture of the slideshow.
    Next,
    /// The picture before the one on the panel.
    Previous,
    /// Picture `n`, counted from 1 in the slideshow's (sorted) order.
    Show(u32),
    /// Start (`true`) or stop the slideshow.
    Slideshow(bool),
    /// Advance every this many seconds; the gallery clamps it.
    Interval(u32),
}

/// Longest `X-Action` value read; anything longer is not one of ours.
pub const MAX_ACTION_BYTES: usize = 32;

/// Parse an `X-Action` value; `None` for anything outside the list.
#[must_use]
pub fn parse(text: &str) -> Option<Action> {
    let text = text.trim();
    if text.len() > MAX_ACTION_BYTES {
        return None;
    }
    let mut parts = text.split_ascii_whitespace();
    let verb = parts.next()?;
    let argument = parts.next();
    if parts.next().is_some() {
        return None;
    }
    let number = || argument.and_then(|value| value.parse::<u32>().ok());
    match (verb, argument) {
        ("next", None) => Some(Action::Next),
        ("previous", None) => Some(Action::Previous),
        ("show", Some(_)) => number().filter(|n| *n >= 1).map(Action::Show),
        ("slideshow", Some("on")) => Some(Action::Slideshow(true)),
        ("slideshow", Some("off")) => Some(Action::Slideshow(false)),
        ("interval", Some(_)) => number().filter(|s| *s >= 1).map(Action::Interval),
        _ => None,
    }
}

/// The frame's own readings, as the sensors give them.
#[derive(Debug, Clone, Copy, Default)]
pub struct Readings {
    /// Room temperature in hundredths of a degree.
    pub temp_hundredths: Option<i32>,
    /// Relative humidity in hundredths of a percent.
    pub rh_hundredths: Option<u32>,
    /// Battery charge, 0..=100, when a cell is present.
    pub battery_percent: Option<u8>,
}

/// Whether `reply` asks for any reading, so the sensors are read only then.
#[must_use]
pub fn wants_readings(reply: &str) -> bool {
    ["{temp}", "{humidity}", "{battery}"]
        .iter()
        .any(|placeholder| reply.contains(placeholder))
}

/// `reply` with each placeholder replaced by the reading and its noun.
#[must_use]
pub fn fill(reply: &str, readings: &Readings) -> String {
    const UNKNOWN: &str = "неизвестно сколько: датчик не ответил";
    let temp = readings.temp_hundredths.map_or_else(
        || UNKNOWN.to_owned(),
        |hundredths| {
            let degrees = rounded(hundredths);
            agree(degrees, "градус", "градуса", "градусов")
        },
    );
    let humidity = readings.rh_hundredths.map_or_else(
        || UNKNOWN.to_owned(),
        |hundredths| {
            let percent = rounded(i32::try_from(hundredths).unwrap_or(i32::MAX));
            agree(percent, "процент", "процента", "процентов")
        },
    );
    let battery = readings.battery_percent.map_or_else(
        || "неизвестен: батареи нет".to_owned(),
        |percent| agree(i32::from(percent), "процент", "процента", "процентов"),
    );
    reply
        .replace("{temp}", &temp)
        .replace("{humidity}", &humidity)
        .replace("{battery}", &battery)
}

/// Hundredths to the nearest whole number, halves away from zero.
fn rounded(hundredths: i32) -> i32 {
    if hundredths >= 0 {
        (hundredths + 50) / 100
    } else {
        (hundredths - 50) / 100
    }
}

/// «21 градус», «22 градуса», «25 градусов», «11 градусов»: the noun agrees
/// with the last digits, the sign is the number's.
fn agree(value: i32, one: &str, few: &str, many: &str) -> String {
    let last = value.unsigned_abs();
    let form = match (last % 10, last % 100) {
        (_, 11..=14) => many,
        (1, _) => one,
        (2..=4, _) => few,
        _ => many,
    };
    format!("{value} {form}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_list_and_nothing_else() {
        assert_eq!(parse("next"), Some(Action::Next));
        assert_eq!(parse(" previous "), Some(Action::Previous));
        assert_eq!(parse("show 5"), Some(Action::Show(5)));
        assert_eq!(parse("slideshow on"), Some(Action::Slideshow(true)));
        assert_eq!(parse("slideshow off"), Some(Action::Slideshow(false)));
        assert_eq!(parse("interval 600"), Some(Action::Interval(600)));
        for other in [
            "",
            "show",
            "show 0",
            "show -1",
            "show 5 6",
            "next 1",
            "slideshow maybe",
            "reboot",
            "format sd",
            "interval x",
        ] {
            assert_eq!(parse(other), None, "{other:?}");
        }
        assert_eq!(parse(&"show 1".repeat(10)), None);
    }

    #[test]
    fn readings_are_spoken_with_agreeing_nouns() {
        let readings = Readings {
            temp_hundredths: Some(2_149),
            rh_hundredths: Some(4_451),
            battery_percent: Some(81),
        };
        assert_eq!(
            fill("В комнате {temp}, влажность {humidity}.", &readings),
            "В комнате 21 градус, влажность 45 процентов."
        );
        assert_eq!(fill("Заряд {battery}.", &readings), "Заряд 81 процент.");
        let warm = Readings {
            temp_hundredths: Some(2_350),
            ..readings
        };
        assert_eq!(fill("{temp}", &warm), "24 градуса");
        let cold = Readings {
            temp_hundredths: Some(-1_160),
            ..readings
        };
        assert_eq!(fill("{temp}", &cold), "-12 градусов");
        assert_eq!(agree(11, "градус", "градуса", "градусов"), "11 градусов");
        assert_eq!(agree(101, "градус", "градуса", "градусов"), "101 градус");
    }

    #[test]
    fn missing_readings_are_said_to_be_missing() {
        let text = fill("В комнате {temp}. Заряд {battery}.", &Readings::default());
        assert!(text.contains("датчик не ответил"), "{text}");
        assert!(text.contains("батареи нет"), "{text}");
        assert!(wants_readings("{temp}"));
        assert!(!wants_readings("Привет."));
    }
}
