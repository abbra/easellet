//! Questions that are commands for the frame itself.
//!
//! «Рамка, следующая картинка», «покажи пятую картинку», «выключи слайд-шоу»,
//! «меняй картинки каждые десять минут», «какая температура?». The model cannot
//! act on the frame and would only talk about doing so, so these are matched
//! here, before plugins and the model, and answered with a reply to speak and
//! an action from a fixed list that the frame executes (`X-Action`):
//!
//! | action            | the frame                                    |
//! |-------------------|----------------------------------------------|
//! | `next`            | shows the next picture of its slideshow      |
//! | `previous`        | shows the one before the current one         |
//! | `show <n>`        | shows picture n (1-based, sorted by name)    |
//! | `slideshow on`    | starts the slideshow                         |
//! | `slideshow off`   | stops it; the panel keeps its picture        |
//! | `interval <secs>` | changes how often it advances (120..3600 s)  |
//!
//! Sensor questions need no action: the reply carries `{temp}`, `{humidity}`
//! or `{battery}`, which the frame replaces with its own reading, the Russian
//! noun agreeing with the number («21 градус», «45 процентов»), before speaking.
//!
//! Matching is on word stems, like plugin phrases (`intents.rs`), and on
//! numbers written as digits or as Russian words, which is how Whisper writes
//! them depending on the sentence.

/// What the frame should do, and what it should say.
#[derive(Debug, PartialEq, Eq)]
pub struct Command {
    /// `X-Action`; `None` for questions answered by words alone.
    pub action: Option<String>,
    pub reply: String,
}

/// Lowercase words of letters and digits; `ё` folded into `е`.
fn words(text: &str) -> Vec<String> {
    text.split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(|word| word.to_lowercase().replace('ё', "е"))
        .collect()
}

fn has(words: &[String], stems: &[&str]) -> bool {
    words
        .iter()
        .any(|word| stems.iter().any(|stem| word.starts_with(stem)))
}

const PICTURE: &[&str] = &["картин", "фото", "снимок", "снимк", "изображен", "кадр"];
const SLIDESHOW: &[&str] = &["слайд", "показ"];

/// A number in `words` from `start` on: digits, or Russian cardinal words up
/// to 99 («двадцать пять»), or ordinals one to ten («пятую»).
fn number(words: &[String], start: usize) -> Option<(u32, usize)> {
    const UNITS: &[(&str, u32)] = &[
        ("один", 1),
        ("одну", 1),
        ("одна", 1),
        ("два", 2),
        ("две", 2),
        ("три", 3),
        ("четыр", 4),
        ("пят", 5),
        ("шест", 6),
        ("сем", 7),
        ("восем", 8),
        ("девят", 9),
    ];
    const TEENS: &[(&str, u32)] = &[
        ("десят", 10),
        ("одиннадцат", 11),
        ("двенадцат", 12),
        ("тринадцат", 13),
        ("четырнадцат", 14),
        ("пятнадцат", 15),
        ("шестнадцат", 16),
        ("семнадцат", 17),
        ("восемнадцат", 18),
        ("девятнадцат", 19),
    ];
    const TENS: &[(&str, u32)] = &[
        ("двадцат", 20),
        ("тридцат", 30),
        ("сорок", 40),
        ("пятьдесят", 50),
        ("шестьдесят", 60),
        ("семьдесят", 70),
        ("восемьдесят", 80),
        ("девяносто", 90),
    ];
    const ORDINALS: &[(&str, u32)] = &[
        ("перв", 1),
        ("втор", 2),
        ("трет", 3),
        ("четверт", 4),
        ("пят", 5),
        ("шест", 6),
        ("седьм", 7),
        ("восьм", 8),
        ("девят", 9),
        ("десят", 10),
    ];
    let word = words.get(start)?;
    if let Ok(value) = word.parse::<u32>() {
        return Some((value, 1));
    }
    let exact = |table: &[(&str, u32)]| {
        table
            .iter()
            .find(|(stem, _)| word.starts_with(stem))
            .map(|(_, value)| *value)
    };
    // Longest stems first: «пятнадцать» before «пять», «пятьдесят» before both.
    if let Some(tens) = exact(TENS) {
        let units = words.get(start + 1).and_then(|next| {
            UNITS
                .iter()
                .find(|(stem, _)| next.starts_with(stem) && !next.starts_with("десят"))
                .map(|(_, value)| *value)
        });
        return Some(units.map_or((tens, 1), |units| (tens + units, 2)));
    }
    if let Some(teen) = exact(TEENS) {
        // «десятую» is an ordinal ten, which is the same number.
        return Some((teen, 1));
    }
    exact(UNITS)
        .or_else(|| exact(ORDINALS))
        .map(|value| (value, 1))
}

/// The first number anywhere in the question, with the word after it.
fn first_number(words: &[String]) -> Option<(u32, Option<&String>)> {
    (0..words.len()).find_map(|index| {
        number(words, index).map(|(value, used)| (value, words.get(index + used)))
    })
}

/// «минута», «минуты», «минут» after a number, by Russian agreement.
fn plural(value: u32, one: &str, few: &str, many: &str) -> String {
    let form = match (value % 10, value % 100) {
        (_, 11..=14) => many,
        (1, _) => one,
        (2..=4, _) => few,
        _ => many,
    };
    format!("{value} {form}")
}

/// The command a question gives the frame, if it is one.
#[must_use]
pub fn find(question: &str) -> Option<Command> {
    let words = words(question);
    let picture = has(&words, PICTURE);
    let said = |action: Option<String>, reply: &str| {
        Some(Command {
            action,
            reply: reply.to_owned(),
        })
    };

    // A picture to be made is the image plugin's, not a slideshow command.
    if has(&words, &["нарису", "сгенерир", "создай", "придумай"]) {
        return None;
    }

    // Sensors: words alone, filled in by the frame. The room's, so a question
    // about outdoors is left to the weather plugin and the model.
    let outdoors = has(&words, &["погод", "улиц", "прогноз", "завтра"]);
    let temperature = !outdoors && has(&words, &["температур", "градус", "жарко", "холодно"]);
    let humidity = !outdoors && has(&words, &["влажн"]);
    if temperature && humidity {
        return said(None, "В комнате {temp}, влажность {humidity}.");
    }
    if temperature {
        return said(None, "В комнате {temp}.");
    }
    if humidity {
        return said(None, "Влажность {humidity}.");
    }
    if has(&words, &["батаре", "аккумулятор", "заряд"]) && !picture {
        return said(None, "Заряд батареи {battery}.");
    }

    // How often the slideshow advances.
    let every = has(&words, &["кажд", "интервал"]);
    if every && (picture || has(&words, SLIDESHOW) || has(&words, &["меня", "смен", "переключ"]))
    {
        let hour = has(&words, &["час"]);
        let half = has(&words, &["полчаса"]);
        let seconds = match first_number(&words) {
            Some((value, unit)) => {
                let unit = unit.map_or("", String::as_str);
                if unit.starts_with("сек") {
                    value
                } else if unit.starts_with("час") {
                    value * 3600
                } else {
                    value * 60
                }
            }
            None if half => 1800,
            None if hour => 3600,
            None => return None,
        };
        let clamped = seconds.clamp(120, 3600);
        let reply = if clamped % 3600 == 0 {
            "Буду менять картинку каждый час.".to_owned()
        } else if clamped % 60 == 0 {
            format!(
                "Буду менять картинку каждые {}.",
                plural(clamped / 60, "минуту", "минуты", "минут")
            )
        } else {
            format!(
                "Буду менять картинку каждые {}.",
                plural(clamped, "секунду", "секунды", "секунд")
            )
        };
        return Some(Command {
            action: Some(format!("interval {clamped}")),
            reply,
        });
    }

    // Slideshow on and off.
    if has(&words, SLIDESHOW) {
        if has(
            &words,
            &[
                "выключ",
                "останов",
                "отключ",
                "стоп",
                "хватит",
                "прекрат",
                "законч",
            ],
        ) {
            return said(Some("slideshow off".to_owned()), "Останавливаю слайд-шоу.");
        }
        if has(
            &words,
            &["включ", "запуст", "начн", "начать", "старт", "продолж"],
        ) {
            return said(Some("slideshow on".to_owned()), "Запускаю слайд-шоу.");
        }
    }

    if !picture {
        return None;
    }
    if has(&words, &["следующ", "друг", "дальше", "новую"]) {
        return said(Some("next".to_owned()), "Следующая картинка.");
    }
    if has(&words, &["предыдущ", "прошл", "назад", "обратно", "прежн"])
    {
        return said(Some("previous".to_owned()), "Предыдущая картинка.");
    }
    if let Some((value, _)) = first_number(&words)
        && (1..=999).contains(&value)
    {
        return Some(Command {
            action: Some(format!("show {value}")),
            reply: format!("Показываю картинку номер {value}."),
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn action(question: &str) -> Option<String> {
        find(question).and_then(|command| command.action)
    }

    #[test]
    fn pictures() {
        assert_eq!(action("Следующая картинка."), Some("next".to_owned()));
        assert_eq!(action("покажи другую фотографию"), Some("next".to_owned()));
        assert_eq!(
            action("верни предыдущую картинку"),
            Some("previous".to_owned())
        );
        assert_eq!(
            action("Покажи картинку номер 5."),
            Some("show 5".to_owned())
        );
        assert_eq!(action("покажи пятую картинку"), Some("show 5".to_owned()));
        assert_eq!(
            action("покажи картинку двадцать три"),
            Some("show 23".to_owned())
        );
        assert_eq!(
            action("покажи картинку пятнадцать"),
            Some("show 15".to_owned())
        );
    }

    #[test]
    fn slideshow() {
        assert_eq!(
            action("выключи слайд-шоу"),
            Some("slideshow off".to_owned())
        );
        assert_eq!(action("Включи слайдшоу!"), Some("slideshow on".to_owned()));
        assert_eq!(
            action("меняй картинки каждые десять минут"),
            Some("interval 600".to_owned())
        );
        assert_eq!(
            action("Меняй картинку каждые 5 минут."),
            Some("interval 300".to_owned())
        );
        assert_eq!(
            action("меняй картинки каждый час"),
            Some("interval 3600".to_owned())
        );
        assert_eq!(
            find("смена картинок каждые 30 секунд").map(|command| command.reply),
            Some("Буду менять картинку каждые 2 минуты.".to_owned()),
            "below the panel's minimum, clamped and said so"
        );
    }

    #[test]
    fn sensors_are_words_for_the_frame_to_fill() {
        let command = find("Какая температура в комнате?").unwrap();
        assert_eq!(command.action, None);
        assert!(command.reply.contains("{temp}"));
        assert!(
            find("какая влажность и температура")
                .unwrap()
                .reply
                .contains("{humidity}")
        );
    }

    #[test]
    fn other_questions_are_not_commands() {
        for question in [
            "покажи погоду в Эспоо",
            "сколько будет семь на восемь",
            "расскажи про картины Шишкина",
            "нарисуй картинку с котом",
            "нарисуй другую картинку",
            "сколько градусов на улице",
            "расскажи ещё про картины",
            "",
        ] {
            assert_eq!(action(question), None, "{question}");
        }
    }

    #[test]
    fn plurals_agree() {
        assert_eq!(plural(1, "минуту", "минуты", "минут"), "1 минуту");
        assert_eq!(plural(3, "минуту", "минуты", "минут"), "3 минуты");
        assert_eq!(plural(11, "минуту", "минуты", "минут"), "11 минут");
        assert_eq!(plural(21, "минуту", "минуты", "минут"), "21 минуту");
    }
}
