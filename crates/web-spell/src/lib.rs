//! This module contains the spell checker. It is roughly based on the paper
//! http://static.googleusercontent.com/media/research.google.com/en/us/pubs/archive/36180.pdf
//! from google.
//!
//! # Usage
//!
//! ```rust
//! # use std::path::Path;
//! # use web_spell::{CorrectionConfig, SpellChecker, Lang};
//!
//! # let path = Path::new("../data/web_spell/checker");
//!
//! # if !path.exists() {
//! #     return;
//! # }
//!
//! let checker = SpellChecker::open("<path-to-model>", CorrectionConfig::default());
//! # let checker = SpellChecker::open(path, CorrectionConfig::default());
//! let correction = checker.unwrap().correct("hwllo", &Lang::Eng);
//! ```

mod config;
mod error_model;
pub mod spell_checker;
mod stupid_backoff;
mod term_freqs;
mod trainer;

pub use config::CorrectionConfig;
pub use error_model::ErrorModel;
pub use spell_checker::Lang;
pub use spell_checker::SpellChecker;
pub use stupid_backoff::StupidBackoff;
pub use term_freqs::TermDict;
pub use trainer::FirstTrainer;
pub use trainer::FirstTrainerResult;
pub use trainer::SecondTrainer;

use fst::Streamer;
use std::ops::Range;

use itertools::intersperse;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("FST error: {0}")]
    Fst(#[from] fst::Error),

    #[error("Serde error: {0}")]
    Serde(#[from] serde_json::Error),

    #[error("Encode error: {0}")]
    Encode(#[from] bincode::error::EncodeError),

    #[error("Decode error: {0}")]
    Decode(#[from] bincode::error::DecodeError),

    #[error("Checker not found")]
    CheckerNotFound,
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(
    PartialEq,
    Eq,
    Debug,
    serde::Serialize,
    serde::Deserialize,
    bincode::Encode,
    bincode::Decode,
    Clone,
)]
pub struct Correction {
    original: String,
    pub terms: Vec<CorrectionTerm>,
}

#[derive(
    PartialEq,
    Eq,
    Debug,
    serde::Serialize,
    serde::Deserialize,
    bincode::Encode,
    bincode::Decode,
    Clone,
)]
pub enum CorrectionTerm {
    Corrected { orig: String, correction: String },
    NotCorrected(String),
}

impl From<Correction> for String {
    fn from(correction: Correction) -> Self {
        intersperse(
            correction.terms.into_iter().map(|term| match term {
                CorrectionTerm::Corrected {
                    orig: _,
                    correction,
                } => correction,
                CorrectionTerm::NotCorrected(orig) => orig,
            }),
            " ".to_string(),
        )
        .collect()
    }
}

impl Correction {
    /// Create an empty correction.
    pub fn empty(original: String) -> Self {
        Self {
            original,
            terms: Vec::new(),
        }
    }

    /// Push a term to the correction.
    pub fn push(&mut self, term: CorrectionTerm) {
        self.terms.push(term);
    }

    /// Check if all terms are not corrected.
    pub fn is_all_orig(&self) -> bool {
        self.terms
            .iter()
            .all(|term| matches!(term, CorrectionTerm::NotCorrected(_)))
    }
}

/// Split text into sentence ranges by detecting common sentence boundaries like periods, exclamation marks,
/// question marks and newlines. Returns a Vec of byte ranges for each detected sentence.
///
/// The splitting is optimized for performance and simplicity rather than perfect accuracy. It handles
/// common cases like abbreviations, URLs, ellipses and whitespace trimming.
///
/// Note that this is a heuristic approach and may not handle all edge cases correctly.
pub fn sentence_ranges(text: &str) -> Vec<Range<usize>> {
    let skip = ["mr.", "ms.", "dr."];

    let mut res = Vec::new();
    let mut last_start = 0;

    let text = text.to_ascii_lowercase();

    // We should really do something more clever than this.
    // Tried using `SRX`[https://docs.rs/srx/latest/srx/] but it was a bit too slow.
    for (end, _) in text
        .char_indices()
        .filter(|(_, c)| matches!(c, '.' | '\n' | '?' | '!'))
    {
        let end = ceil_char_boundary(&text, end + 1);

        if skip.iter().any(|p| text[last_start..end].ends_with(p)) {
            continue;
        }

        // skip 'site.com', '...', '!!!' etc.
        if !text[end..].starts_with(|c: char| c.is_ascii_whitespace()) {
            continue;
        }

        let mut start = last_start;

        while start < end && text[start..].starts_with(|c: char| c.is_whitespace()) {
            start = ceil_char_boundary(&text, start + 1);
        }

        // just a precaution
        if start > end {
            continue;
        }

        res.push(start..end);

        last_start = end;
    }

    let mut start = last_start;

    while start < text.len() && text[start..].starts_with(|c: char| c.is_whitespace()) {
        start = ceil_char_boundary(&text, start + 1);
    }

    res.push(start..text.len());

    res
}

/// Tokenize text into words.
pub fn tokenize(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split_whitespace()
        .filter(|s| {
            !s.chars()
                .any(|c| !c.is_ascii_alphanumeric() && c != '-' && c != '_')
        })
        .map(|s| s.to_string())
        .collect()
}

/// A pointer for merging two term streams.
struct MergePointer<'a> {
    /// The current head of the stream.
    pub(crate) term: String,

    /// The current head value.
    pub(crate) value: u64,

    /// The stream to merge.
    pub(crate) stream: fst::map::Stream<'a>,

    /// Whether the stream is finished.
    pub(crate) is_finished: bool,
}

impl MergePointer<'_> {
    pub fn advance(&mut self) -> bool {
        self.is_finished = self
            .stream
            .next()
            .map(|(term, value)| {
                self.term = std::str::from_utf8(term).unwrap().to_string();
                self.value = value;
            })
            .is_none();

        !self.is_finished
    }
}

impl PartialOrd for MergePointer<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for MergePointer<'_> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        match (self.is_finished, other.is_finished) {
            (true, true) | (false, false) => self.term.cmp(&other.term),
            (true, false) => std::cmp::Ordering::Greater,
            (false, true) => std::cmp::Ordering::Less,
        }
    }
}

impl PartialEq for MergePointer<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.term == other.term && self.is_finished == other.is_finished
    }
}

impl Eq for MergePointer<'_> {}

/// Get the next character boundary after or at the given index.
fn ceil_char_boundary(str: &str, index: usize) -> usize {
    let mut res = index;

    while !str.is_char_boundary(res) && res < str.len() {
        res += 1;
    }

    res
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn test_sentence_ranges() {
        let text = "This is a sentence. This is another sentence. This is a third sentence.";
        let ranges = sentence_ranges(text);
        assert_eq!(ranges.len(), 3);

        assert_eq!(&text[ranges[0].clone()], "This is a sentence.");
        assert_eq!(&text[ranges[1].clone()], "This is another sentence.");
        assert_eq!(&text[ranges[2].clone()], "This is a third sentence.");

        let text = "This is a sentence. This is another sentence. This is a third sentence";
        let ranges = sentence_ranges(text);
        assert_eq!(ranges.len(), 3);

        assert_eq!(&text[ranges[0].clone()], "This is a sentence.");
        assert_eq!(&text[ranges[1].clone()], "This is another sentence.");
        assert_eq!(&text[ranges[2].clone()], "This is a third sentence");

        let text = "mr. roberts";

        let ranges = sentence_ranges(text);

        assert_eq!(ranges.len(), 1);
        assert_eq!(&text[ranges[0].clone()], "mr. roberts");

        let text = "site.com is the best";

        let ranges = sentence_ranges(text);

        assert_eq!(ranges.len(), 1);
        assert_eq!(&text[ranges[0].clone()], "site.com is the best");
    }

    proptest! {
        #[test]
        fn prop_ceil_char_boundary(s: String, index: usize) {
            let index = if s.is_empty() {
                0
            } else {
                index % s.len()
            };

            let ceil = ceil_char_boundary(&s, index);
            prop_assert!(s.is_char_boundary(ceil));
        }
    }
}
