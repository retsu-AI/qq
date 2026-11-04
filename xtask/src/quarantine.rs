use std::{fmt, fs, io, path::Path};

use thiserror::Error;
use time::{Date, Duration, OffsetDateTime, format_description::well_known::Iso8601};

pub const PATH: &str = ".config/quarantine.toml";
pub const MAX_DAYS: i64 = 14;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Entry {
    pub line: usize,
    pub test: Option<String>,
    pub owner: Option<String>,
    pub issue: Option<String>,
    pub added: Option<String>,
    pub expires: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{PATH}:{line}: {message}")]
pub struct ParseError {
    pub line: usize,
    pub message: String,
}

#[derive(Debug, Error)]
pub enum QuarantineError {
    #[error("failed to read {PATH}")]
    Read(#[source] io::Error),
    #[error(transparent)]
    Parse(#[from] ParseError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    pub line: usize,
    pub detail: String,
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{PATH}:{}: {}", self.line, self.detail)
    }
}

pub fn parse(text: &str) -> Result<Vec<Entry>, ParseError> {
    let mut entries: Vec<Entry> = Vec::new();
    for (index, raw) in text.lines().enumerate() {
        let line = index + 1;
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if trimmed == "[[quarantine]]" {
            entries.push(Entry {
                line,
                ..Entry::default()
            });
            continue;
        }
        let error = |message: &str| ParseError {
            line,
            message: message.to_owned(),
        };
        let Some((key, value)) = trimmed.split_once('=') else {
            return Err(error("expected `[[quarantine]]` or `key = \"value\"`"));
        };
        let Some(entry) = entries.last_mut() else {
            return Err(error("a key must follow a `[[quarantine]]` header"));
        };
        let value = value.trim();
        let Some(value) = value
            .strip_prefix('"')
            .and_then(|value| value.strip_suffix('"'))
            .filter(|value| !value.contains(['"', '\\']))
        else {
            return Err(error("values are double-quoted strings without escapes"));
        };
        let slot = match key.trim() {
            "test" => &mut entry.test,
            "owner" => &mut entry.owner,
            "issue" => &mut entry.issue,
            "added" => &mut entry.added,
            "expires" => &mut entry.expires,
            other => {
                return Err(error(&format!(
                    "unknown key `{other}`; expected test, owner, issue, added or expires"
                )));
            }
        };
        if slot.is_some() {
            return Err(error(&format!("duplicate key `{}`", key.trim())));
        }
        *slot = Some(value.trim().to_owned());
    }
    Ok(entries)
}

pub fn load(root: &Path) -> Result<Vec<Entry>, QuarantineError> {
    match fs::read_to_string(root.join(PATH)) {
        Ok(text) => Ok(parse(&text)?),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(QuarantineError::Read(error)),
    }
}

pub fn today() -> Date {
    OffsetDateTime::now_utc().date()
}

fn date(value: &str) -> Option<Date> {
    Date::parse(value, &Iso8601::DATE).ok()
}

fn present(value: Option<&String>) -> Option<&str> {
    value.map(String::as_str).filter(|value| !value.is_empty())
}

pub fn problems(
    entries: &[Entry],
    today: Date,
    test_exists: impl Fn(&str, &str) -> bool,
) -> Vec<Problem> {
    let mut problems = Vec::new();
    for entry in entries {
        let mut problem = |detail: String| {
            problems.push(Problem {
                line: entry.line,
                detail,
            })
        };
        for (name, value) in [
            ("test", &entry.test),
            ("owner", &entry.owner),
            ("issue", &entry.issue),
            ("added", &entry.added),
            ("expires", &entry.expires),
        ] {
            if present(value.as_ref()).is_none() {
                problem(format!("missing `{name}`"));
            }
        }
        if let Some(test) = present(entry.test.as_ref()) {
            match test.split_once(' ') {
                Some((binary, name)) if !binary.is_empty() && !name.trim().is_empty() => {
                    if !test_exists(binary, name.trim()) {
                        problem(format!("`{test}` matches no test"));
                    }
                }
                _ => problem(format!(
                    "`{test}` is not `<binary-id> <test path>` as `cargo nextest list` prints it"
                )),
            }
        }
        let added = present(entry.added.as_ref()).map(|value| (value, date(value)));
        let expires = present(entry.expires.as_ref()).map(|value| (value, date(value)));
        for (name, parsed) in [("added", added), ("expires", expires)] {
            if let Some((value, None)) = parsed {
                problem(format!("`{name}` {value:?} is not a YYYY-MM-DD date"));
            }
        }
        let added = added.and_then(|(_, date)| date);
        let expires = expires.and_then(|(_, date)| date);
        if let Some(added) = added {
            if added > today {
                problem(format!("`added` {added} is after today, {today}"));
            }
            let limit = added + Duration::days(MAX_DAYS);
            if today > limit {
                problem(format!(
                    "added {added}, more than {MAX_DAYS} days ago; fix the test or delete it"
                ));
            }
            if let Some(expires) = expires
                && (expires > limit || expires < added)
            {
                problem(format!(
                    "`expires` {expires} must fall within {MAX_DAYS} days after `added` {added}"
                ));
            }
        }
        if let Some(expires) = expires
            && today > expires
        {
            problem(format!("expired on {expires}; fix the test or delete it"));
        }
    }
    problems
}

pub fn filterset(entries: &[Entry]) -> Option<String> {
    let terms = entries
        .iter()
        .filter_map(|entry| present(entry.test.as_ref()))
        .filter_map(|test| test.split_once(' '))
        .map(|(binary, name)| format!("(binary_id(={binary}) & test(={}))", name.trim()))
        .collect::<Vec<_>>();
    (!terms.is_empty()).then(|| terms.join(" | "))
}

#[cfg(test)]
mod tests {
    use super::{Entry, ParseError, Problem, filterset, parse, problems};
    use time::{Date, Month};

    fn day(text: &str) -> Date {
        let mut parts = text.split('-').map(|part| part.parse::<i32>().unwrap());
        let (year, month, day) = (
            parts.next().unwrap(),
            parts.next().unwrap(),
            parts.next().unwrap(),
        );
        Date::from_calendar_date(
            year,
            Month::try_from(u8::try_from(month).unwrap()).unwrap(),
            u8::try_from(day).unwrap(),
        )
        .unwrap()
    }

    const ENTRY: &str = r#"
[[quarantine]]
test = "qq-core sessions::tests::runs::a_flaky_case"
owner = "zmeyer"
issue = "ENG-1200"
added = "2026-11-01"
expires = "2026-11-15"
"#;

    fn details(text: &str, today: &str, exists: bool) -> Vec<String> {
        problems(&parse(text).unwrap(), day(today), |_, _| exists)
            .into_iter()
            .map(|problem| problem.detail)
            .collect()
    }

    #[test]
    fn an_empty_list_parses_to_no_entries_and_no_filter() {
        assert_eq!(parse("").unwrap(), Vec::<Entry>::new());
        assert_eq!(parse("\n# none yet\n").unwrap(), Vec::<Entry>::new());
        assert_eq!(filterset(&[]), None);
    }

    #[test]
    fn entries_parse_every_field_and_build_one_nextest_filter() {
        let entries = parse(&format!("{ENTRY}{}", ENTRY.replace("a_flaky_case", "b"))).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(
            entries[0],
            Entry {
                line: 2,
                test: Some("qq-core sessions::tests::runs::a_flaky_case".to_owned()),
                owner: Some("zmeyer".to_owned()),
                issue: Some("ENG-1200".to_owned()),
                added: Some("2026-11-01".to_owned()),
                expires: Some("2026-11-15".to_owned()),
            }
        );
        assert_eq!(
            filterset(&entries).unwrap(),
            "(binary_id(=qq-core) & test(=sessions::tests::runs::a_flaky_case)) | (binary_id(=qq-core) & test(=sessions::tests::runs::b))"
        );
    }

    #[test]
    fn malformed_lists_name_the_offending_line() {
        for (text, line, message) in [
            (
                "test = \"x\"\n",
                1,
                "a key must follow a `[[quarantine]]` header",
            ),
            (
                "[[quarantine]]\ntest\n",
                2,
                "expected `[[quarantine]]` or `key = \"value\"`",
            ),
            (
                "[[quarantine]]\nowner = zmeyer\n",
                2,
                "values are double-quoted strings without escapes",
            ),
            (
                "[[quarantine]]\nowner = \"a\\\"b\"\n",
                2,
                "values are double-quoted strings without escapes",
            ),
            (
                "[[quarantine]]\nreason = \"x\"\n",
                2,
                "unknown key `reason`; expected test, owner, issue, added or expires",
            ),
            (
                "[[quarantine]]\nowner = \"a\"\nowner = \"b\"\n",
                3,
                "duplicate key `owner`",
            ),
        ] {
            assert_eq!(
                parse(text),
                Err(ParseError {
                    line,
                    message: message.to_owned()
                }),
                "{text:?}"
            );
        }
    }

    #[test]
    fn an_entry_is_valid_through_its_expiry_day_and_expired_after_it() {
        assert_eq!(details(ENTRY, "2026-11-01", true), Vec::<String>::new());
        assert_eq!(details(ENTRY, "2026-11-15", true), Vec::<String>::new());
        assert_eq!(
            details(ENTRY, "2026-11-16", true),
            [
                "added 2026-11-01, more than 14 days ago; fix the test or delete it",
                "expired on 2026-11-15; fix the test or delete it"
            ]
        );
        let early = ENTRY.replace("2026-11-15", "2026-11-05");
        assert_eq!(details(&early, "2026-11-05", true), Vec::<String>::new());
        assert_eq!(
            details(&early, "2026-11-06", true),
            ["expired on 2026-11-05; fix the test or delete it"]
        );
    }

    #[test]
    fn an_entry_added_in_the_future_is_refused() {
        let future = ENTRY
            .replace("2026-11-01", "2027-01-01")
            .replace("2026-11-15", "2027-01-10");
        assert_eq!(
            details(&future, "2026-11-01", true),
            ["`added` 2027-01-01 is after today, 2026-11-01"]
        );
        assert_eq!(details(&future, "2027-01-01", true), Vec::<String>::new());
    }

    #[test]
    fn an_expiry_beyond_fourteen_days_or_before_the_added_day_is_refused() {
        let bound = |expires: &str| {
            format!("`expires` {expires} must fall within 14 days after `added` 2026-11-01")
        };
        assert_eq!(
            details(
                &ENTRY.replace("2026-11-15", "2026-11-16"),
                "2026-11-01",
                true
            ),
            [bound("2026-11-16")]
        );
        assert_eq!(
            details(
                &ENTRY.replace("2026-11-15", "2026-10-31"),
                "2026-11-01",
                true
            ),
            [
                bound("2026-10-31"),
                "expired on 2026-10-31; fix the test or delete it".to_owned()
            ]
        );
    }

    #[test]
    fn missing_owner_issue_or_dates_and_unknown_tests_are_problems() {
        let entry = "[[quarantine]]\ntest = \"qq-core gone\"\nowner = \"\"\n";
        assert_eq!(
            problems(&parse(entry).unwrap(), day("2026-11-01"), |binary, name| {
                assert_eq!((binary, name), ("qq-core", "gone"));
                false
            }),
            [
                "missing `owner`",
                "missing `issue`",
                "missing `added`",
                "missing `expires`",
                "`qq-core gone` matches no test",
            ]
            .map(|detail| Problem {
                line: 1,
                detail: detail.to_owned()
            })
        );
        assert_eq!(
            details(
                &ENTRY.replace("qq-core sessions", "sessions"),
                "2026-11-01",
                true
            ),
            [
                "`sessions::tests::runs::a_flaky_case` is not `<binary-id> <test path>` as `cargo nextest list` prints it"
            ]
        );
        assert_eq!(
            details(
                &ENTRY.replace("\"2026-11-01\"", "\"Nov 1\""),
                "2026-11-01",
                true
            ),
            ["`added` \"Nov 1\" is not a YYYY-MM-DD date"]
        );
    }
}
