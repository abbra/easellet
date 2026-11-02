//! Which plugin, if any, a spoken question asks for.
//!
//! «Рамка, покажи погоду» should run the `weather` plugin for that frame rather
//! than make the model talk about the weather. Plugins say what they answer to
//! in their manifest (`[assistant] intent` and `phrases`); this matches a
//! question against those phrases, deterministically and in microseconds,
//! before the model is asked anything.
//!
//! Russian inflects, so words are compared by stem: lowercase, `ё` as `е`, the
//! last two letters dropped from words of six or more and the last one from
//! words of four or five. A phrase matches when each of its stems begins some
//! word of the question; the phrase with the most words wins, so "покажи
//! погоду на неделю" beats "погоду" when both exist.

/// One plugin's phrases.
pub struct Intent<'a> {
    pub plugin: &'a str,
    pub phrases: Vec<&'a str>,
}

/// Lowercase words of letters and digits; `ё` folded into `е`.
fn words(text: &str) -> Vec<String> {
    text.split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(|word| word.to_lowercase().replace('ё', "е"))
        .collect()
}

fn stem(word: &str) -> String {
    let letters: Vec<char> = word.chars().collect();
    let keep = match letters.len() {
        0..=3 => letters.len(),
        4..=5 => letters.len() - 1,
        n => n - 2,
    };
    letters[..keep].iter().collect()
}

/// The plugin a question asks for, with the phrase that matched.
pub fn find<'a>(question: &str, intents: &[Intent<'a>]) -> Option<(&'a str, &'a str)> {
    let spoken = words(question);
    let mut best: Option<(&'a str, &'a str, usize)> = None;
    for intent in intents {
        for phrase in &intent.phrases {
            let stems: Vec<String> = words(phrase).iter().map(|word| stem(word)).collect();
            if stems.is_empty() {
                continue;
            }
            let all = stems
                .iter()
                .all(|stem| spoken.iter().any(|word| word.starts_with(stem.as_str())));
            if all && best.is_none_or(|(_, _, size)| stems.len() > size) {
                best = Some((intent.plugin, phrase, stems.len()));
            }
        }
    }
    best.map(|(plugin, phrase, _)| (plugin, phrase))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn intents() -> Vec<Intent<'static>> {
        vec![
            Intent {
                plugin: "weather",
                phrases: vec!["показать погоду", "покажи погоду", "какая погода"],
            },
            Intent {
                plugin: "note",
                // Imperatives differ from infinitives past the stem, so a
                // manifest lists the forms people say.
                phrases: vec!["оставить записку", "оставь записку"],
            },
        ]
    }

    #[test]
    fn inflected_questions_find_their_plugin() {
        for question in [
            "покажи погоду",
            "Покажи, пожалуйста, погоду на завтра",
            "какая будет погода?",
            "КАКАЯ ПОГОДА",
        ] {
            assert_eq!(
                find(question, &intents()).map(|hit| hit.0),
                Some("weather"),
                "{question}"
            );
        }
        assert_eq!(
            find("оставь записку маме", &intents()).map(|hit| hit.0),
            Some("note")
        );
    }

    #[test]
    fn other_questions_go_to_the_model() {
        for question in [
            "сколько будет два плюс два",
            "расскажи сказку",
            "погода",
            "",
        ] {
            assert_eq!(find(question, &intents()), None, "{question}");
        }
    }

    #[test]
    fn stems_drop_endings() {
        assert_eq!(stem("погоду"), "пого");
        assert_eq!(stem("покажи"), "пока");
        assert_eq!(stem("какая"), "кака");
        assert_eq!(stem("сок"), "сок");
        assert_eq!(words("Ёлка, ёж!"), ["елка", "еж"]);
    }
}
