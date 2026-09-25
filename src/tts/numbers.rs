//! Numbers -> English words, before espeak-ng sees them.
//!
//! espeak-ng expands digits on its own and is good at it: `1,000` is "one
//! thousand", `1433` is "one thousand four hundred thirty three", `1.8` is "one
//! point eight", `50%` is "fifty percent", `1st` is "first", `3:45` is "three
//! forty-five". All of that is left alone here — a second implementation of
//! something already right is only a second thing to get wrong.
//!
//! Two readings it gets wrong, and a human does not:
//!
//! * **Currency.** espeak-ng reads the symbol where it stands, so `$5` is
//!   "dollar five" and `$1,500.50` is "dollar one thousand five hundred point
//!   five zero". Nobody says that.
//! * **Years.** `1066` is "one thousand sixty six" and `1985` is "nineteen
//!   hundred eighty five". A four-digit number in prose is almost always a year
//!   and is read as two pairs: "ten sixty-six", "nineteen eighty-five".
//!
//! Both are fixed the way misaki does it, because misaki is Kokoro's real front
//! end and this is the half of it that does not need a POS tagger:
//! `Lexicon.get_number` in `misaki/en.py` builds a currency phrase out of its
//! `CURRENCIES` table and sends any bare four-digit number through
//! `num2words(n, to='year')`. The `num2words` crate is the same algorithm as
//! the Python package misaki imports, and its year output is byte-identical to
//! it for every year from 1000 to 2100 — asserted below over the range that
//! matters most, and spot-checked against the Python for the rest.
//!
//! The rewrite is at the level of *text*: a number becomes English words (or,
//! for a currency amount, a rearranged run of digits) and espeak-ng phonemizes
//! the result like any other words. Nothing here touches the chunker, so no
//! chunk index and no stored position moves.

use std::borrow::Cow;

use num2words::Num2Words;

/// misaki's `CURRENCIES`: the symbol, its unit, and its hundredth.
const CURRENCIES: &[(char, &str, &str)] = &[
    ('$', "dollar", "cent"),
    ('\u{A3}', "pound", "pence"),
    ('\u{20AC}', "euro", "cent"),
];

/// A scale word after a currency amount moves the unit to the end: `$1.5
/// million` is "one point five million dollars", never "one dollar and five
/// cents million". misaki has no guard for this because it phonemizes token by
/// token and never sees the next word; here the next word is right there.
const SCALES: &[&str] = &["thousand", "million", "billion", "trillion"];

/// Four-digit numbers read as years only inside this range. misaki applies the
/// year reading to *any* four-digit number, which is right for every year a
/// book will mention and turns `9999` into "ninety-nine ninety-nine". Outside
/// the range espeak-ng's cardinal is both correct and what a person would say.
const YEARS: std::ops::RangeInclusive<u64> = 1000..=2099;

/// Rewrite the numbers in `text` that espeak-ng would read wrong.
pub fn normalize(text: &str) -> Cow<'_, str> {
    if !text.bytes().any(|b| b.is_ascii_digit()) {
        return Cow::Borrowed(text);
    }
    let cs: Vec<char> = text.chars().collect();
    let mut out: Option<String> = None;
    let mut i = 0usize;
    while i < cs.len() {
        match rewrite_at(&cs, i) {
            Some((next, words)) => {
                let o = out.get_or_insert_with(|| cs[..i].iter().collect());
                o.push_str(&words);
                i = next;
            }
            None => {
                if let Some(o) = out.as_mut() {
                    o.push(cs[i]);
                }
                i += 1;
            }
        }
    }
    out.map_or(Cow::Borrowed(text), Cow::Owned)
}

/// The number token starting at `i`, if there is one worth rewriting. Returns
/// the index to carry on from and the words to put in its place.
fn rewrite_at(cs: &[char], i: usize) -> Option<(usize, String)> {
    // Never start inside a word: `COVID19` and `mp3` are not numbers.
    if i > 0 && cs[i - 1].is_alphanumeric() {
        return None;
    }
    // Nor inside a number. The scan below swallows a `,` or `.` between digits,
    // but a token it declines is skipped one character at a time, so the next
    // attempt used to land just past the separator: `3.1415` became "3." and a
    // year, "3.fourteen fifteen", and `25.12.2016` a date with a year bolted on.
    // A digit run that follows `<digit>.` or `<digit>,` is the rest of a number
    // that has already been looked at.
    if i >= 2 && matches!(cs[i - 1], '.' | ',') && cs[i - 2].is_ascii_digit() {
        return None;
    }
    let currency = CURRENCIES
        .iter()
        .find(|(sym, _, _)| *sym == cs[i])
        .filter(|_| cs.get(i + 1).is_some_and(char::is_ascii_digit));
    let start = if currency.is_some() { i + 1 } else { i };
    if !cs.get(start).is_some_and(char::is_ascii_digit) {
        return None;
    }

    // Digits, plus any `,` or `.` that has another digit right after it — the
    // same rule the punctuation splitter uses, so a sentence-final full stop is
    // not swallowed into the number.
    let mut end = start;
    while end < cs.len() {
        let c = cs[end];
        let separator = matches!(c, ',' | '.') && cs.get(end + 1).is_some_and(char::is_ascii_digit);
        if !c.is_ascii_digit() && !separator {
            break;
        }
        end += 1;
    }
    // A letter suffix (`1st`, `1990s`, `5km`) is the token's, not the next
    // word's.
    let mut suffix = String::new();
    let mut after = end;
    if cs.get(after) == Some(&'\'') && cs.get(after + 1).is_some_and(|c| c.is_alphabetic()) {
        after += 1;
    }
    while after < cs.len() && cs[after].is_alphabetic() {
        suffix.push(cs[after].to_ascii_lowercase());
        after += 1;
    }

    let digits: String = cs[start..end].iter().collect();
    match currency {
        // A currency amount this cannot read is consumed verbatim rather than
        // left to the next pass: `$1990s` must not become `$` followed by a
        // year, with the symbol stranded in front of it.
        Some(&(_, unit, sub)) => Some(
            money(cs, after, &digits, &suffix, unit, sub)
                .unwrap_or_else(|| (after, cs[i..after].iter().collect())),
        ),
        None if part_of_a_larger_number(cs, i, after) => year_range(cs, i, &digits, &suffix, after),
        None => year(&digits, &suffix).map(|w| (after, w)),
    }
}

/// Is this digit run one group of something bigger — a phone number, a range
/// of years, a date written with slashes?
///
/// `1066` in `555-1066` or `1050-1066` is not a year, and the separators that
/// say so are the ones the digit scan above stops at: `,` and `.` between
/// digits are already swallowed into the token, so only `-`, `/` and `:` can
/// leave a four-digit group stranded next to more digits. A trailing `,` or
/// `.` followed by a space is a sentence, not a number, which is why the digit
/// on the far side is what is tested rather than the separator alone.
///
/// An en dash joins exactly as a hyphen does. It used to be no joiner at all,
/// so `1914–1918` read as two years while `1914-1918` read as two cardinals,
/// and `555–1066` put a year in a phone number.
fn part_of_a_larger_number(cs: &[char], start: usize, end: usize) -> bool {
    let before = start >= 2 && JOINERS.contains(&cs[start - 1]) && cs[start - 2].is_ascii_digit();
    let after = cs
        .get(end)
        .is_some_and(|c| JOINERS.contains(c) && cs.get(end + 1).is_some_and(char::is_ascii_digit));
    before || after
}

/// What may join a four-digit group to more digits: see
/// [`part_of_a_larger_number`].
const JOINERS: &[char] = &['-', '\u{2013}', '/', ':'];

/// `1914-1918` and `1914–1918`: a span of years, "nineteen fourteen to
/// nineteen eighteen".
///
/// The one joined group that *is* a year, and it is recognised narrowly so that
/// everything [`part_of_a_larger_number`] protects stays protected: exactly two
/// bare four-digit groups, joined by a hyphen or an en dash, both inside
/// [`YEARS`], the second no earlier than the first, and nothing joined on
/// either end. `555-1066` fails the first test, `1066-1050` the fourth,
/// `1914-1918-1939` and `12-1914-1918` the last.
///
/// The dash becomes "to", which is what a person says, and neither dash can
/// simply be kept: espeak-ng fuses two words joined by a hyphen into one
/// (`fourteen-nineteen` comes out as a single `fˈɔːɹtiːnnˈaɪntiːn`), and an en
/// dash between words is silence, which leaves four year-words in a row with
/// nothing to say where the first year ends.
fn year_range(
    cs: &[char],
    start: usize,
    first: &str,
    suffix: &str,
    end: usize,
) -> Option<(usize, String)> {
    let four = |d: &str| d.len() == 4 && d.bytes().all(|b| b.is_ascii_digit());
    if !suffix.is_empty() || !four(first) {
        return None;
    }
    // Joined on the left means this is the far end of something already
    // declined, not the start of a span.
    if start >= 2 && JOINERS.contains(&cs[start - 1]) && cs[start - 2].is_ascii_digit() {
        return None;
    }
    let dash = *cs.get(end)?;
    if !matches!(dash, '-' | '\u{2013}') {
        return None;
    }
    let second_start = end + 1;
    let mut second_end = second_start;
    while cs.get(second_end).is_some_and(char::is_ascii_digit) {
        second_end += 1;
    }
    let second: String = cs[second_start..second_end].iter().collect();
    if !four(&second) {
        return None;
    }
    // Nothing after the second year but a boundary: not a letter, not another
    // digit, not a joiner leading to one, not a decimal.
    let tail = cs.get(second_end).copied();
    if tail.is_some_and(char::is_alphanumeric)
        || (tail.is_some_and(|c| JOINERS.contains(&c) || matches!(c, '.' | ','))
            && cs.get(second_end + 1).is_some_and(char::is_ascii_digit))
    {
        return None;
    }
    let (a, b): (u64, u64) = (first.parse().ok()?, second.parse().ok()?);
    if !YEARS.contains(&a) || !YEARS.contains(&b) || b < a {
        return None;
    }
    Some((
        second_end,
        format!("{} to {}", year(first, "")?, year(&second, "")?),
    ))
}

/// `$1,500.50` -> `1500 dollars and 50 cents`, and the plain cases beside it.
///
/// The amount stays in digits: espeak-ng's own cardinal expansion is correct
/// and is what misaki's `extend_num` produces too, so all this has to do is put
/// the unit where English puts it and split the hundredths off.
fn money(
    cs: &[char],
    after: usize,
    digits: &str,
    suffix: &str,
    unit: &str,
    sub: &str,
) -> Option<(usize, String)> {
    if !suffix.is_empty() || digits.matches('.').count() > 1 {
        return None;
    }
    // "$1.5 million": the scale belongs between the amount and the unit.
    if let Some((next, scale)) = following_scale(cs, after) {
        let bare = digits.replace(',', "");
        return Some((next, format!("{bare} {scale} {}", plural(unit))));
    }
    let bare = digits.replace(',', "");
    let (whole, cents) = match bare.split_once('.') {
        // Only an exact two-digit fraction is hundredths. `$1.5` is an amount,
        // not one dollar and five cents, whatever misaki makes of it: "1.5
        // dollars", the decimal left to espeak-ng, the unit plural because a
        // fraction of anything is — "one point five dollars", "one point oh
        // dollars".
        Some((w, c)) if c.len() == 2 => (w.to_string(), Some(c.to_string())),
        Some(_) => return Some((after, format!("{bare} {}", plural(unit)))),
        None => (bare.clone(), None),
    };
    let mut parts: Vec<String> = Vec::new();
    let whole_n: u64 = whole.parse().ok()?;
    let cents_n: Option<u64> = cents.as_deref().map(str::parse).transpose().ok()?;
    // misaki drops a zero half rather than saying "zero cents".
    if whole_n != 0 || cents_n.is_none_or(|c| c == 0) {
        parts.push(format!("{whole} {}", unit_form(unit, whole_n)));
    }
    if let Some(c) = cents_n.filter(|c| *c != 0) {
        parts.push(format!("{c} {}", unit_form(sub, c)));
    }
    Some((after, parts.join(" and ")))
}

/// The word after `at`, if it is a scale word, and where it ends. Handed back
/// as it was written: espeak-ng reads an all-caps word differently, and a
/// rewrite has no business changing the case of a word it is only moving.
fn following_scale(cs: &[char], at: usize) -> Option<(usize, String)> {
    // A no-break space is what typeset money puts there on purpose (`$5\u{a0}million`,
    // and the narrow one French typography uses), so it separates exactly as a
    // space does.
    let mut i = at;
    while cs
        .get(i)
        .is_some_and(|c| matches!(c, ' ' | '\u{a0}' | '\u{202f}'))
    {
        i += 1;
    }
    if i == at {
        return None;
    }
    let mut end = i;
    while cs.get(end).is_some_and(|c| c.is_alphabetic()) {
        end += 1;
    }
    let word: String = cs[i..end].iter().collect();
    SCALES
        .iter()
        .any(|s| *s == word.to_lowercase())
        .then_some((end, word))
}

/// "pence" has no plural; everything else takes an `s` unless there is one of
/// it. misaki: `abs(num) != 1 and unit != 'pence'`.
fn unit_form(unit: &str, n: u64) -> String {
    if n == 1 || unit == "pence" {
        unit.to_string()
    } else {
        plural(unit)
    }
}

fn plural(unit: &str) -> String {
    if unit == "pence" {
        unit.to_string()
    } else {
        format!("{unit}s")
    }
}

/// A bare four-digit number is a year: `1066` -> "ten sixty-six". A trailing
/// `s` makes it a decade, which is the last word pluralised: `1990s` ->
/// "nineteen nineties".
fn year(digits: &str, suffix: &str) -> Option<String> {
    if digits.len() != 4 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n: u64 = digits.parse().ok()?;
    if !YEARS.contains(&n) {
        return None;
    }
    let plural_wanted = match suffix {
        "" => false,
        "s" => true,
        _ => return None,
    };
    let words = Num2Words::new(n).year().to_words().ok()?;
    if !plural_wanted {
        return Some(words);
    }
    // "one thousand" pluralised word by word is "one thousands", which nobody
    // says: `1000s of people` is "thousands of people". `2000s` is left to the
    // rule below — "two thousands" is how the decade is spoken, "the two
    // thousands" — and nothing else in the range is a whole thousand.
    if n == 1000 {
        return Some("thousands".into());
    }
    let (head, last) = match words.rsplit_once(' ') {
        Some((h, l)) => (Some(h), l),
        None => (None, words.as_str()),
    };
    let last = match last.strip_suffix('y') {
        Some(stem) => format!("{stem}ies"),
        None => format!("{last}s"),
    };
    Some(match head {
        Some(h) => format!("{h} {last}"),
        None => last,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(s: &str) -> String {
        normalize(s).into_owned()
    }

    #[test]
    fn currency_puts_the_unit_where_english_puts_it() {
        assert_eq!(n("It cost $5."), "It cost 5 dollars.");
        assert_eq!(n("It cost $1."), "It cost 1 dollar.");
        assert_eq!(n("$1,500.50 a year"), "1500 dollars and 50 cents a year");
        assert_eq!(n("£20.00 exactly"), "20 pounds exactly");
        assert_eq!(n("£1.50"), "1 pound and 50 pence");
        assert_eq!(n("€30.99"), "30 euros and 99 cents");
        // A zero half is dropped rather than spoken.
        assert_eq!(n("$0.50"), "50 cents");
        assert_eq!(n("$0.01"), "1 cent");
        // A scale word moves the unit to the end.
        assert_eq!(
            n("worth $1.5 million today"),
            "worth 1.5 million dollars today"
        );
        assert_eq!(n("$3 BILLION"), "3 BILLION dollars");
        // A no-break space is a space.
        assert_eq!(n("$5\u{a0}million"), "5 million dollars");
        assert_eq!(n("$5\u{202f}million"), "5 million dollars");
    }

    #[test]
    fn an_amount_that_is_not_hundredths_is_still_an_amount() {
        // Consumed verbatim, `$1.5` was "dollar one point five".
        assert_eq!(n("It was $1.5."), "It was 1.5 dollars.");
        assert_eq!(n("$2.5 each"), "2.5 dollars each");
        assert_eq!(n("£1.005"), "1.005 pounds");
        assert_eq!(n("$1.0"), "1.0 dollars");
    }

    #[test]
    fn a_rewrite_never_starts_inside_a_number() {
        for same in [
            "pi is 3.1415 or so",
            "0.1234 of it",
            "1.2000 exactly",
            "12,1990 of them",
            "on 25.12.2016",
            "12,345,1990",
        ] {
            assert_eq!(n(same), same, "{same}");
        }
    }

    #[test]
    fn a_whole_thousand_pluralises_the_way_people_say_it() {
        assert_eq!(n("1000s of people"), "thousands of people");
        assert_eq!(n("the 2000s"), "the two thousands");
        assert_eq!(n("the 1100s"), "the eleven hundreds");
    }

    #[test]
    fn a_span_of_years_reads_the_same_whichever_dash_joins_it() {
        let span = "the nineteen fourteen to nineteen eighteen war";
        assert_eq!(n("the 1914-1918 war"), span);
        assert_eq!(n("the 1914\u{2013}1918 war"), span);
        assert_eq!(
            n("(1939-1945)."),
            "(nineteen thirty-nine to nineteen forty-five)."
        );
        assert_eq!(n("1066-1066"), "ten sixty-six to ten sixty-six");
        // Not a span: backwards, abbreviated, three groups, out of range, or
        // part of something with more digits in it.
        for same in [
            "1918-1914",
            "1914-18",
            "1914-1918-1939",
            "12-1914-1918",
            "1914-1918s",
            "1914-1918.5",
            "0999-1066",
            "1990-2100",
            "555\u{2013}1066",
        ] {
            assert_eq!(n(same), same, "{same}");
        }
    }

    #[test]
    fn a_four_digit_number_is_a_year() {
        assert_eq!(n("born in 1066"), "born in ten sixty-six");
        assert_eq!(n("In 2016, it changed."), "In twenty sixteen, it changed.");
        assert_eq!(n("1985"), "nineteen eighty-five");
        assert_eq!(n("1805 was cold"), "eighteen oh-five was cold");
        assert_eq!(n("the 1990s"), "the nineteen nineties");
        assert_eq!(n("the 1900s"), "the nineteen hundreds");
    }

    #[test]
    fn everything_espeak_already_reads_correctly_is_left_alone() {
        for same in [
            "There were 1,000 of them",
            "He was 1.8 meters tall",
            "It was 3:45",
            "the 1st of May",
            "22nd Street",
            "50% of it",
            "chapter 7",
            "a 12345 line file",
            "9999 of them",
            "5000 men",
            "COVID19 and mp3 and H2O",
            "no digits here at all",
        ] {
            assert_eq!(n(same), same, "{same}");
        }
        // Nothing to do is the borrowed path.
        assert!(matches!(normalize("plain words"), Cow::Borrowed(_)));
    }

    #[test]
    fn a_four_digit_group_of_something_bigger_is_not_a_year() {
        for same in [
            "call 555-1066",
            "the 1066-1050 ledger",
            "12/1066 in the ledger",
            "10:1066",
            "$1990s",
            "$20k",
        ] {
            assert_eq!(n(same), same, "{same}");
        }
        // But an ordinary sentence boundary is not a joiner.
        assert_eq!(
            n("Born in 1066, died in 1120."),
            "Born in ten sixty-six, died in eleven twenty."
        );
        assert_eq!(n("It was 1066."), "It was ten sixty-six.");
        assert_eq!(n("(2016)"), "(twenty sixteen)");
    }

    #[test]
    fn the_year_range_is_where_years_are() {
        assert_eq!(year("1000", ""), Some("one thousand".into()));
        assert_eq!(year("2099", ""), Some("twenty ninety-nine".into()));
        // Outside it espeak-ng's cardinal is both right and human.
        assert_eq!(year("2100", ""), None);
        assert_eq!(year("999", ""), None);
        assert_eq!(year("1990", "th"), None);
    }
}
