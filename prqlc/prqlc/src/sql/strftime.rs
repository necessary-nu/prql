//! The format strings `date.to_text` takes, split into the items each dialect
//! translates.
//!
//! The syntax is the strftime dialect of the `chrono` crate
//! (<https://docs.rs/chrono/latest/chrono/format/strftime/index.html>), and a
//! string splits into the items chrono's `StrftimeItems` reads from it, up to
//! and including the first error: a format means what chrono's documentation
//! says it does, and refuses where chrono refuses it.

/// The padding a numeric item is written with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Pad {
    None,
    Zero,
    Space,
}

/// A number a specifier stands for.
///
/// A variant's name is part of an error: BigQuery names an item it refuses by
/// `item_to_strftime` in `dialect.rs`, which spells a variant with no strftime
/// letter there by its `Debug` form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Numeric {
    Year,
    YearDiv100,
    YearMod100,
    IsoYear,
    IsoYearMod100,
    Quarter,
    Month,
    Day,
    WeekFromSun,
    WeekFromMon,
    IsoWeek,
    NumDaysFromSun,
    WeekdayFromMon,
    Ordinal,
    Hour,
    Hour12,
    Minute,
    Second,
    Nanosecond,
    Timestamp,
}

/// A specifier written in a form of its own rather than as a padded number.
///
/// A variant's name is part of an error, as [`Numeric`]'s is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Fixed {
    ShortMonthName,
    LongMonthName,
    ShortWeekdayName,
    LongWeekdayName,
    LowerAmPm,
    UpperAmPm,
    Nanosecond,
    Nanosecond3,
    Nanosecond6,
    Nanosecond9,
    Nanosecond3NoDot,
    Nanosecond6NoDot,
    Nanosecond9NoDot,
    TimezoneName,
    TimezoneOffset,
    TimezoneOffsetColon,
    TimezoneOffsetDoubleColon,
    TimezoneOffsetTripleColon,
    TimezoneOffsetPermissive,
    RFC3339,
}

/// One item of a format string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Item<'a> {
    /// Text written as it is: a run of characters that are neither whitespace
    /// nor `%`, or the `%` that `%%` stands for.
    Literal(&'a str),
    /// A run of whitespace, or the newline `%n` and the tab `%t` stand for.
    Space(&'a str),
    Numeric(Numeric, Pad),
    Fixed(Fixed),
    /// A `%` that does not begin a specifier: an unknown one, a padding
    /// modifier on anything but a single number, or the string's end.
    Error,
}

/// What one specifier stands for: one item, or several for the specifiers that
/// are shorthand for a sequence (`%F` for `%Y-%m-%d`).
enum Spec {
    One(Item<'static>),
    Many(&'static [Item<'static>]),
}

const fn numeric(numeric: Numeric, pad: Pad) -> Spec {
    Spec::One(Item::Numeric(numeric, pad))
}

const fn fixed(fixed: Fixed) -> Spec {
    Spec::One(Item::Fixed(fixed))
}

const MONTH_DAY_YEAR: &[Item<'static>] = &[
    Item::Numeric(Numeric::Month, Pad::Zero),
    Item::Literal("/"),
    Item::Numeric(Numeric::Day, Pad::Zero),
    Item::Literal("/"),
    Item::Numeric(Numeric::YearMod100, Pad::Zero),
];

const YEAR_MONTH_DAY: &[Item<'static>] = &[
    Item::Numeric(Numeric::Year, Pad::Zero),
    Item::Literal("-"),
    Item::Numeric(Numeric::Month, Pad::Zero),
    Item::Literal("-"),
    Item::Numeric(Numeric::Day, Pad::Zero),
];

const HOUR_MINUTE: &[Item<'static>] = &[
    Item::Numeric(Numeric::Hour, Pad::Zero),
    Item::Literal(":"),
    Item::Numeric(Numeric::Minute, Pad::Zero),
];

const HOUR_MINUTE_SECOND: &[Item<'static>] = &[
    Item::Numeric(Numeric::Hour, Pad::Zero),
    Item::Literal(":"),
    Item::Numeric(Numeric::Minute, Pad::Zero),
    Item::Literal(":"),
    Item::Numeric(Numeric::Second, Pad::Zero),
];

const DAY_MONTH_NAME_YEAR: &[Item<'static>] = &[
    Item::Numeric(Numeric::Day, Pad::Space),
    Item::Literal("-"),
    Item::Fixed(Fixed::ShortMonthName),
    Item::Literal("-"),
    Item::Numeric(Numeric::Year, Pad::Zero),
];

const DATE_AND_TIME: &[Item<'static>] = &[
    Item::Fixed(Fixed::ShortWeekdayName),
    Item::Space(" "),
    Item::Fixed(Fixed::ShortMonthName),
    Item::Space(" "),
    Item::Numeric(Numeric::Day, Pad::Space),
    Item::Space(" "),
    Item::Numeric(Numeric::Hour, Pad::Zero),
    Item::Literal(":"),
    Item::Numeric(Numeric::Minute, Pad::Zero),
    Item::Literal(":"),
    Item::Numeric(Numeric::Second, Pad::Zero),
    Item::Space(" "),
    Item::Numeric(Numeric::Year, Pad::Zero),
];

const TWELVE_HOUR_TIME: &[Item<'static>] = &[
    Item::Numeric(Numeric::Hour12, Pad::Zero),
    Item::Literal(":"),
    Item::Numeric(Numeric::Minute, Pad::Zero),
    Item::Literal(":"),
    Item::Numeric(Numeric::Second, Pad::Zero),
    Item::Space(" "),
    Item::Fixed(Fixed::UpperAmPm),
];

/// Splits a format string into its items.
///
/// Text outside specifiers is split wherever it changes between whitespace (as
/// [`char::is_whitespace`] decides) and anything else, so each run is one
/// [`Item::Literal`] or one [`Item::Space`]. A `%` that does not begin a
/// specifier is an [`Item::Error`], and nothing after it is read: every
/// dialect refuses a string at its first error.
pub(super) fn items(format: &str) -> Vec<Item<'_>> {
    let mut items = Vec::new();
    let mut rest = format;
    while let Some(first) = rest.chars().next() {
        if first == '%' {
            match specifier(&rest[1..]) {
                Some((Spec::One(item), after)) => {
                    items.push(item);
                    rest = after;
                }
                Some((Spec::Many(many), after)) => {
                    items.extend_from_slice(many);
                    rest = after;
                }
                None => {
                    items.push(Item::Error);
                    break;
                }
            }
            continue;
        }

        let space = first.is_whitespace();
        let end = rest
            .find(|c: char| {
                if space {
                    !c.is_whitespace()
                } else {
                    c.is_whitespace() || c == '%'
                }
            })
            .unwrap_or(rest.len());
        let (text, after) = rest.split_at(end);
        items.push(if space {
            Item::Space(text)
        } else {
            Item::Literal(text)
        });
        rest = after;
    }
    items
}

fn split_char(text: &str) -> Option<(char, &str)> {
    let c = text.chars().next()?;
    Some((c, &text[c.len_utf8()..]))
}

/// Reads the specifier `text` begins with, the text after a `%`: what it
/// stands for and the text after it, or `None` where it is not a specifier.
///
/// A specifier may open with one padding modifier (`-` unpadded, `0` zeros,
/// `_` spaces), allowed on a specifier that is a single number alone, or with
/// `#`, which only `%#z` takes.
fn specifier(text: &str) -> Option<(Spec, &str)> {
    let (mut letter, mut rest) = split_char(text)?;
    let pad = match letter {
        '-' => Some(Pad::None),
        '0' => Some(Pad::Zero),
        '_' => Some(Pad::Space),
        _ => None,
    };
    let alternate = letter == '#';
    if pad.is_some() || alternate {
        (letter, rest) = split_char(rest)?;
    }
    if alternate && letter != 'z' {
        return None;
    }

    let spec = match letter {
        'A' => fixed(Fixed::LongWeekdayName),
        'B' => fixed(Fixed::LongMonthName),
        'C' => numeric(Numeric::YearDiv100, Pad::Zero),
        'D' | 'x' => Spec::Many(MONTH_DAY_YEAR),
        'F' => Spec::Many(YEAR_MONTH_DAY),
        'G' => numeric(Numeric::IsoYear, Pad::Zero),
        'H' => numeric(Numeric::Hour, Pad::Zero),
        'I' => numeric(Numeric::Hour12, Pad::Zero),
        'M' => numeric(Numeric::Minute, Pad::Zero),
        'P' => fixed(Fixed::LowerAmPm),
        'R' => Spec::Many(HOUR_MINUTE),
        'S' => numeric(Numeric::Second, Pad::Zero),
        'T' | 'X' => Spec::Many(HOUR_MINUTE_SECOND),
        'U' => numeric(Numeric::WeekFromSun, Pad::Zero),
        'V' => numeric(Numeric::IsoWeek, Pad::Zero),
        'W' => numeric(Numeric::WeekFromMon, Pad::Zero),
        'Y' => numeric(Numeric::Year, Pad::Zero),
        'Z' => fixed(Fixed::TimezoneName),
        'a' => fixed(Fixed::ShortWeekdayName),
        'b' | 'h' => fixed(Fixed::ShortMonthName),
        'c' => Spec::Many(DATE_AND_TIME),
        'd' => numeric(Numeric::Day, Pad::Zero),
        'e' => numeric(Numeric::Day, Pad::Space),
        'f' => numeric(Numeric::Nanosecond, Pad::Zero),
        'g' => numeric(Numeric::IsoYearMod100, Pad::Zero),
        'j' => numeric(Numeric::Ordinal, Pad::Zero),
        'k' => numeric(Numeric::Hour, Pad::Space),
        'l' => numeric(Numeric::Hour12, Pad::Space),
        'm' => numeric(Numeric::Month, Pad::Zero),
        'n' => Spec::One(Item::Space("\n")),
        'p' => fixed(Fixed::UpperAmPm),
        'q' => numeric(Numeric::Quarter, Pad::None),
        'r' => Spec::Many(TWELVE_HOUR_TIME),
        's' => numeric(Numeric::Timestamp, Pad::None),
        't' => Spec::One(Item::Space("\t")),
        'u' => numeric(Numeric::WeekdayFromMon, Pad::None),
        'v' => Spec::Many(DAY_MONTH_NAME_YEAR),
        'w' => numeric(Numeric::NumDaysFromSun, Pad::None),
        'y' => numeric(Numeric::YearMod100, Pad::Zero),
        'z' if alternate => fixed(Fixed::TimezoneOffsetPermissive),
        'z' => fixed(Fixed::TimezoneOffset),
        '+' => fixed(Fixed::RFC3339),
        ':' => {
            let (offset, len) = if rest.starts_with("::z") {
                (Fixed::TimezoneOffsetTripleColon, 3)
            } else if rest.starts_with(":z") {
                (Fixed::TimezoneOffsetDoubleColon, 2)
            } else if rest.starts_with('z') {
                (Fixed::TimezoneOffsetColon, 1)
            } else {
                return None;
            };
            rest = &rest[len..];
            fixed(offset)
        }
        '.' => {
            let (precision, after) = split_char(rest)?;
            rest = after;
            fixed(match precision {
                'f' => Fixed::Nanosecond,
                '3' | '6' | '9' => {
                    let (f, after) = split_char(rest)?;
                    rest = after;
                    if f != 'f' {
                        return None;
                    }
                    match precision {
                        '3' => Fixed::Nanosecond3,
                        '6' => Fixed::Nanosecond6,
                        _ => Fixed::Nanosecond9,
                    }
                }
                _ => return None,
            })
        }
        '3' | '6' | '9' => {
            let (f, after) = split_char(rest)?;
            rest = after;
            if f != 'f' {
                return None;
            }
            fixed(match letter {
                '3' => Fixed::Nanosecond3NoDot,
                '6' => Fixed::Nanosecond6NoDot,
                _ => Fixed::Nanosecond9NoDot,
            })
        }
        '%' => Spec::One(Item::Literal("%")),
        _ => return None,
    };

    let spec = match (pad, spec) {
        (None, spec) => spec,
        (Some(pad), Spec::One(Item::Numeric(numeric, _))) => Spec::One(Item::Numeric(numeric, pad)),
        (Some(_), _) => return None,
    };
    Some((spec, rest))
}

#[cfg(test)]
mod tests {
    use super::{items, Fixed, Item, Numeric, Pad};

    #[test]
    fn literals_split_at_whitespace_and_percent() {
        assert_eq!(
            items("100%% in\t%d  days"),
            [
                Item::Literal("100"),
                Item::Literal("%"),
                Item::Space(" "),
                Item::Literal("in"),
                Item::Space("\t"),
                Item::Numeric(Numeric::Day, Pad::Zero),
                Item::Space("  "),
                Item::Literal("days"),
            ]
        );
        assert_eq!(items("a/b-c"), [Item::Literal("a/b-c")]);
        assert_eq!(items(""), []);
    }

    #[test]
    fn non_ascii_text() {
        assert_eq!(
            items("日本%Yé"),
            [
                Item::Literal("日本"),
                Item::Numeric(Numeric::Year, Pad::Zero),
                Item::Literal("é"),
            ]
        );
        // Unicode whitespace is a space item, as ASCII whitespace is.
        assert_eq!(
            items("a\u{a0}b\u{3000}"),
            [
                Item::Literal("a"),
                Item::Space("\u{a0}"),
                Item::Literal("b"),
                Item::Space("\u{3000}"),
            ]
        );
        // A non-ASCII character is no specifier.
        assert_eq!(items("x%é"), [Item::Literal("x"), Item::Error]);
    }

    #[test]
    fn percent_signs() {
        assert_eq!(items("%%"), [Item::Literal("%")]);
        assert_eq!(items("%%%%"), [Item::Literal("%"), Item::Literal("%")]);
        assert_eq!(items("%%Y"), [Item::Literal("%"), Item::Literal("Y")]);
        // A trailing `%` begins no specifier.
        assert_eq!(
            items("%Y %"),
            [
                Item::Numeric(Numeric::Year, Pad::Zero),
                Item::Space(" "),
                Item::Error,
            ]
        );
        assert_eq!(items("%"), [Item::Error]);
        assert_eq!(items("%%%"), [Item::Literal("%"), Item::Error]);
    }

    #[test]
    fn padding_modifiers() {
        assert_eq!(items("%-d"), [Item::Numeric(Numeric::Day, Pad::None)]);
        assert_eq!(items("%_m"), [Item::Numeric(Numeric::Month, Pad::Space)]);
        assert_eq!(items("%0e"), [Item::Numeric(Numeric::Day, Pad::Zero)]);
        assert_eq!(items("%-q"), [Item::Numeric(Numeric::Quarter, Pad::None)]);
        // Only a single number takes one: not a shorthand for several items,
        // not a fixed form, not `%%`, `%n` or `%t`.
        for refused in [
            "%-D", "%0F", "%_T", "%-b", "%-+", "%-%", "%_n", "%0t", "%-3f",
        ] {
            assert_eq!(items(refused), [Item::Error], "{refused}");
        }
        // One modifier at most, and none with nothing after it.
        for refused in ["%--d", "%-0d", "%_-d", "%-", "%0", "%_"] {
            assert_eq!(items(refused), [Item::Error], "{refused}");
        }
    }

    #[test]
    fn alternate_form() {
        assert_eq!(items("%#z"), [Item::Fixed(Fixed::TimezoneOffsetPermissive)]);
        for refused in ["%#Y", "%#", "%-#z", "%#-z"] {
            assert_eq!(items(refused), [Item::Error], "{refused}");
        }
    }

    #[test]
    fn unknown_specifiers() {
        for refused in [
            "%Q", "%E", "%O", "%i", "%K", "%1f", "%.4f", "%.3x", "%:x", "% ", "%\t",
        ] {
            assert_eq!(items(refused), [Item::Error], "{refused}");
        }
        // Nothing after an error is read.
        assert_eq!(
            items("%Y%Q%m"),
            [Item::Numeric(Numeric::Year, Pad::Zero), Item::Error,]
        );
    }

    #[test]
    fn fractions_and_offsets() {
        assert_eq!(items("%.f"), [Item::Fixed(Fixed::Nanosecond)]);
        assert_eq!(items("%.6f"), [Item::Fixed(Fixed::Nanosecond6)]);
        assert_eq!(items("%9f"), [Item::Fixed(Fixed::Nanosecond9NoDot)]);
        assert_eq!(items("%:z"), [Item::Fixed(Fixed::TimezoneOffsetColon)]);
        assert_eq!(
            items("%::z"),
            [Item::Fixed(Fixed::TimezoneOffsetDoubleColon)]
        );
        assert_eq!(
            items("%:::z"),
            [Item::Fixed(Fixed::TimezoneOffsetTripleColon)]
        );
        for refused in ["%.", "%.3", "%3", "%:", "%::", "%::::z"] {
            assert_eq!(items(refused), [Item::Error], "{refused}");
        }
    }

    #[test]
    fn shorthands() {
        assert_eq!(
            items("%F"),
            [
                Item::Numeric(Numeric::Year, Pad::Zero),
                Item::Literal("-"),
                Item::Numeric(Numeric::Month, Pad::Zero),
                Item::Literal("-"),
                Item::Numeric(Numeric::Day, Pad::Zero),
            ]
        );
        assert_eq!(items("%D"), items("%x"));
        assert_eq!(items("%T"), items("%X"));
        assert_eq!(items("%T"), items("%H:%M:%S"));
        assert_eq!(items("%R"), items("%H:%M"));
        assert_eq!(items("%r"), items("%I:%M:%S %p"));
        assert_eq!(items("%v"), items("%e-%b-%Y"));
        assert_eq!(items("%c"), items("%a %b %e %H:%M:%S %Y"));
        assert_eq!(items("%b"), items("%h"));
        assert_eq!(items("%k"), items("%_H"));
        assert_eq!(items("%l"), items("%_I"));
        assert_eq!(items("%n%t"), [Item::Space("\n"), Item::Space("\t")]);
    }
}
