//! Narrow arithmetic, answered exactly instead of by the model.
//!
//! «Сколько будет 458 умножить на 714?» is asked often enough to matter, and every
//! model tried so far answered it confidently and wrongly: Phi-4-mini said
//! `326 412` five times out of five, and Gemini said `327 732` (right: `327 012`).
//! A wrong number stated in a confident voice is worse than no answer, so one
//! number, one operator and one number are computed here instead.
//!
//! The recogniser is deliberately narrow. Anything wider — three operands,
//! parentheses, units, a word besides the operator — is left to the model, because
//! a calculator that silently drops part of the question is worse than none.

/// One binary operation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Operator {
    Add,
    Sub,
    Mul,
    Div,
}

impl Operator {
    /// How the operator is spoken back, so the answer reads like the question.
    fn word(self) -> &'static str {
        match self {
            Self::Add => "плюс",
            Self::Sub => "минус",
            Self::Mul => "умножить на",
            Self::Div => "разделить на",
        }
    }
}

/// Every way the four operators are said or written here, longest first.
const OPERATORS: &[(&str, Operator)] = &[
    ("умножить на", Operator::Mul),
    ("разделить на", Operator::Div),
    ("поделить на", Operator::Div),
    ("делить на", Operator::Div),
    ("плюс", Operator::Add),
    ("минус", Operator::Sub),
    ("*", Operator::Mul),
    ("×", Operator::Mul),
    ("·", Operator::Mul),
    ("x", Operator::Mul),
    ("+", Operator::Add),
    ("−", Operator::Sub),
    ("–", Operator::Sub),
    ("—", Operator::Sub),
    ("-", Operator::Sub),
    ("/", Operator::Div),
];

/// Polite lead-ins that carry no meaning for the calculation.
const PREFIXES: &[&str] = &[
    "сколько будет",
    "сколько равно",
    "сколько",
    "посчитай",
    "вычисли",
    "рассчитай",
    "what is",
    "how much is",
];

/// The exact answer to a one-operation arithmetic question, or `None` when the
/// question is not narrow enough to answer without a model.
#[must_use]
pub fn answer(prompt: &str) -> Option<String> {
    let text = prompt
        .trim()
        .trim_end_matches(['?', '!', '.', ' '])
        .to_lowercase();
    let text = strip_prefix(&text);
    let (left, operator, right) = split(text)?;
    let left_value = number(left)?;
    let right_value = number(right)?;
    if operator == Operator::Div && right_value == 0.0 {
        return Some("На ноль делить нельзя.".to_owned());
    }
    let result = match operator {
        Operator::Add => left_value + right_value,
        Operator::Sub => left_value - right_value,
        Operator::Mul => left_value * right_value,
        Operator::Div => left_value / right_value,
    };
    Some(format!(
        "{} {} {} равно {}.",
        left.trim(),
        operator.word(),
        right.trim(),
        format_result(result)?
    ))
}

/// Drop a lead-in such as «сколько будет», keeping the expression.
fn strip_prefix(text: &str) -> &str {
    for prefix in PREFIXES {
        if let Some(rest) = text.strip_prefix(prefix) {
            // A separator after the lead-in may be dropped, but a sign may not:
            // in «сколько будет -5 плюс 3» the minus belongs to the operand.
            let rest = rest.trim_start_matches(char::is_whitespace);
            let rest = rest
                .strip_prefix([',', ':', ';', '—', '–'])
                .unwrap_or(rest)
                .trim_start_matches(char::is_whitespace);
            if !rest.is_empty() {
                return rest;
            }
        }
    }
    text
}

/// Split at the first operator that leaves a number on both sides.
///
/// Every occurrence is tried, because a symbol can also be part of an operand:
/// in «-5 плюс 3» the leading minus does not split anything.
fn split(text: &str) -> Option<(&str, Operator, &str)> {
    for (pattern, operator) in OPERATORS {
        let word = pattern
            .chars()
            .all(|character| character.is_alphabetic() || character == ' ');
        for (start, _) in text.match_indices(pattern) {
            let end = start + pattern.len();
            if word && !on_word_boundaries(text, start, end) {
                continue;
            }
            let (left, right) = (&text[..start], &text[end..]);
            if number(left).is_some() && number(right).is_some() {
                return Some((left, *operator, right));
            }
        }
    }
    None
}

/// Not in the middle of a longer word: «плюс» must not match «плюсом».
fn on_word_boundaries(text: &str, start: usize, end: usize) -> bool {
    let is_word = |character: char| character.is_alphanumeric() || character == '_';
    !text[..start].chars().next_back().is_some_and(is_word)
        && !text[end..].chars().next().is_some_and(is_word)
}

/// One number, optionally signed and with either `.` or `,` before the fraction.
///
/// Bounded to 15 digits so the arithmetic stays exact: `f64` carries 15 decimal
/// digits safely, and a question with longer operands is not what this is for.
fn number(text: &str) -> Option<f64> {
    let mut characters = text.trim().chars().peekable();
    let negative = match characters.peek() {
        Some('-') | Some('−') => {
            characters.next();
            true
        }
        Some('+') => {
            characters.next();
            false
        }
        _ => false,
    };
    let mut digits = String::new();
    let mut seen_point = false;
    for character in characters {
        match character {
            '0'..='9' => digits.push(character),
            '.' | ',' if !seen_point && !digits.is_empty() => {
                seen_point = true;
                digits.push('.');
            }
            _ => return None,
        }
    }
    if digits.is_empty() || digits.len() > 16 {
        return None;
    }
    let value: f64 = digits.parse().ok()?;
    Some(if negative { -value } else { value })
}

/// A result a person would say: whole numbers without a decimal point, fractions
/// rounded to six places.
fn format_result(value: f64) -> Option<String> {
    if !value.is_finite() || value.abs() >= 1e15 {
        return None;
    }
    let rounded = (value * 1e6).round() / 1e6;
    if rounded == rounded.trunc() {
        return Some(format!("{}", rounded as i64));
    }
    let mut text = format!("{rounded:.6}");
    while text.ends_with('0') {
        text.pop();
    }
    if text.ends_with('.') {
        text.pop();
    }
    Some(text)
}

#[cfg(test)]
mod tests {
    use super::answer;

    fn some(text: &str) -> Option<String> {
        answer(text)
    }

    #[test]
    fn the_multiply_that_models_kept_getting_wrong() {
        assert_eq!(
            some("Сколько будет 458 умножить на 714?").as_deref(),
            Some("458 умножить на 714 равно 327012.")
        );
        assert_eq!(
            some("458 * 714").as_deref(),
            Some("458 умножить на 714 равно 327012.")
        );
    }

    #[test]
    fn symbols_are_spoken_back_as_words() {
        assert_eq!(some("2+2").as_deref(), Some("2 плюс 2 равно 4."));
        assert_eq!(
            some("10 / 4").as_deref(),
            Some("10 разделить на 4 равно 2.5.")
        );
        assert_eq!(some("10 − 15").as_deref(), Some("10 минус 15 равно -5."));
        assert_eq!(
            some("458 x 714").as_deref(),
            Some("458 умножить на 714 равно 327012.")
        );
    }

    #[test]
    fn decimals_with_a_comma_are_accepted() {
        assert_eq!(
            some("1,5 плюс 2,25").as_deref(),
            Some("1,5 плюс 2,25 равно 3.75.")
        );
    }

    #[test]
    fn a_signed_operand_does_not_confuse_the_split() {
        assert_eq!(
            some("сколько будет -5 плюс 3").as_deref(),
            Some("-5 плюс 3 равно -2.")
        );
    }

    #[test]
    fn division_by_zero_never_reaches_the_model() {
        assert_eq!(
            some("7 разделить на 0").as_deref(),
            Some("На ноль делить нельзя.")
        );
    }

    #[test]
    fn wider_expressions_are_left_alone() {
        assert_eq!(some("2 плюс 2 умножить на 3"), None);
        assert_eq!(some("(2 плюс 2) умножить на 3"), None);
        assert_eq!(some("2 плюс 2 плюс 2"), None);
    }

    #[test]
    fn ordinary_questions_are_not_captured() {
        assert_eq!(some("Покажи погоду в Хельсинки"), None);
        assert_eq!(some("Сколько будет стоить билет?"), None);
        assert_eq!(some("следующая картинка"), None);
        assert_eq!(some("458 градусов"), None);
        assert_eq!(some("умножить на 714"), None);
    }
}
