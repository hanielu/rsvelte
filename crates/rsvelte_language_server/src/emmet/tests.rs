//! The port against emmet-helper itself: `tests/data/emmet-oracle.json` is
//! `doComplete(document, position, 'html', {})` on a corpus of lines.
//! Regenerate it with `node scripts/dev/generate-emmet-data.mjs`.

use serde::Deserialize;

use super::*;

const ORACLE: &str = include_str!("../../tests/data/emmet-oracle.json");

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Case {
    line: String,
    cursor: usize,
    items: Option<Vec<Item>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Item {
    label: String,
    new_text: String,
    documentation: String,
    start: usize,
    end: usize,
    random: bool,
}

/// Lorem ipsum differs every time; its tags and lines do not.
fn skeleton(text: &str) -> (Vec<&str>, usize) {
    let tags = text
        .match_indices('<')
        .filter_map(|(start, _)| {
            text[start..]
                .find('>')
                .map(|end| &text[start..=start + end])
        })
        .collect();
    (tags, text.lines().count())
}

#[test]
fn every_corpus_line_completes_as_emmet_helper_does() {
    let cases: Vec<Case> = serde_json::from_str(ORACLE).expect("parse the Emmet oracle");
    let mut failures = Vec::new();
    for case in &cases {
        let got = suggestions(&case.line, case.cursor);
        let want = case.items.as_deref().unwrap_or_default();
        let same = got.len() == want.len()
            && got.iter().zip(want).all(|(got, want)| {
                got.label == want.label
                    && got.start == want.start
                    && got.end == want.end
                    && if want.random {
                        !got.new_text.is_empty()
                            && skeleton(&got.new_text) == skeleton(&want.new_text)
                    } else {
                        got.new_text == want.new_text && got.documentation == want.documentation
                    }
            });
        if !same {
            failures.push(format!(
                "{:?} at {}:\n  want {:?}\n  got  {:?}",
                case.line,
                case.cursor,
                want.iter()
                    .map(|item| (&item.label, item.start, item.end, &item.new_text))
                    .collect::<Vec<_>>(),
                got.iter()
                    .map(|item| (&item.label, item.start, item.end, &item.new_text))
                    .collect::<Vec<_>>(),
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} lines differ:\n{}",
        failures.len(),
        cases.len(),
        failures.join("\n")
    );
}

#[test]
fn the_oracle_covers_lines_with_and_without_suggestions() {
    let cases: Vec<Case> = serde_json::from_str(ORACLE).expect("parse the Emmet oracle");
    assert!(cases.iter().filter(|case| case.items.is_some()).count() > 300);
    assert!(cases.iter().filter(|case| case.items.is_none()).count() > 20);
}

/// emmet would build this text forever; the port stops at a size an editor
/// can still show.
#[test]
fn a_huge_lorem_is_capped() {
    let items = suggestions("lorem99999999999", 16);
    assert_eq!(items.len(), 1);
    let words = items[0].new_text.split_whitespace().count();
    assert_eq!(words, transform::MAX_LOREM_WORDS);
}

/// emmet has no limit, and builds the text for every repeat.
#[test]
fn lorem_ipsum_shares_one_budget_per_expansion() {
    let items = suggestions("lorem10000*1000", 15);
    assert_eq!(items.len(), 1);
    let words = items[0].new_text.split_whitespace().count();
    assert_eq!(words, transform::MAX_LOREM_WORDS);
}

/// A minified line makes emmet's backward scan quadratic; past the limit the
/// abbreviation is not offered rather than guessed at.
#[test]
fn an_abbreviation_longer_than_the_limit_is_not_offered() {
    let long = "a>".repeat(1500) + "b";
    assert!(suggestions(&long, long.len()).is_empty());
    let short = "a>".repeat(400) + "b";
    assert_eq!(suggestions(&short, short.len())[0].label, short);
    let line = format!("{} p", "x".repeat(5000));
    assert_eq!(suggestions(&line, line.len())[0].label, "p");
}
