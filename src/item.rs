///! An item is line of text that read from `find` command or stdin together with
///! the internal states, such as selected or not
use crate::ansi::{ANSIParser, AnsiString};
use crate::field::*;
use regex::Regex;
use std::borrow::Cow;
use std::cmp::Ordering;
use std::default::Default;
use std::sync::Arc;
use crate::spinlock::{SpinLock, SpinLockGuard};
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::iter::Iterator;

/// An item will store everything that one line input will need to be operated and displayed.
///
/// What's special about an item?
/// The simplest version of an item is a line of string, but things are getting more complex:
/// - The conversion of lower/upper case is slow in rust, because it involds unicode.
/// - We may need to interpret the ANSI codes in the text.
/// - The text can be transformed and limited while searching.
///
/// About the ANSI, we made assumption that it is linewise, that means no ANSI codes will affect
/// more than one line.
#[derive(Debug)]
pub struct Item {
    // (num of run, number of index)
    index: (usize, usize),

    // The text that will be ouptut when user press `enter`
    orig_text: String,

    // The text that will shown into the screen. Can be transformed.
    text: AnsiString,

    matching_ranges: Vec<(usize, usize)>,

    // For the transformed ANSI case, the output will need another transform.
    using_transform_fields: bool,
    ansi_enabled: bool,
}

impl<'a> Item {
    pub fn new(
        orig_text: Cow<str>,
        ansi_enabled: bool,
        trans_fields: &[FieldRange],
        matching_fields: &[FieldRange],
        delimiter: &Regex,
        index: (usize, usize),
    ) -> Self {
        let using_transform_fields = !trans_fields.is_empty();

        //        transformed | ANSI             | output
        //------------------------------------------------------
        //                    +- T -> trans+ANSI | ANSI
        //                    |                  |
        //      +- T -> trans +- F -> trans      | orig
        // orig |                                |
        //      +- F -> orig  +- T -> ANSI     ==| ANSI
        //                    |                  |
        //                    +- F -> orig       | orig

        let mut ansi_parser: ANSIParser = Default::default();

        let text = if using_transform_fields && ansi_enabled {
            // ansi and transform
            ansi_parser.parse_ansi(&parse_transform_fields(delimiter, &orig_text, trans_fields))
        } else if using_transform_fields {
            // transformed, not ansi
            AnsiString::new_string(parse_transform_fields(delimiter, &orig_text, trans_fields))
        } else if ansi_enabled {
            // not transformed, ansi
            ansi_parser.parse_ansi(&orig_text)
        } else {
            // normal case
            AnsiString::new_empty()
        };

        let mut ret = Item {
            index,
            orig_text: orig_text.into_owned(),
            text,
            using_transform_fields: !trans_fields.is_empty(),
            matching_ranges: Vec::new(),
            ansi_enabled,
        };

        let matching_ranges = if !matching_fields.is_empty() {
            parse_matching_fields(delimiter, ret.get_text(), matching_fields)
        } else {
            vec![(0, ret.get_text().len())]
        };

        ret.matching_ranges = matching_ranges;
        ret
    }

    pub fn get_text(&self) -> &str {
        if !self.using_transform_fields && !self.ansi_enabled {
            &self.orig_text
        } else {
            &self.text.get_stripped()
        }
    }

    pub fn get_text_struct(&self) -> Option<&AnsiString> {
        if !self.using_transform_fields && !self.ansi_enabled {
            None
        } else {
            Some(&self.text)
        }
    }

    pub fn get_output_text(&'a self) -> Cow<'a, str> {
        if self.using_transform_fields && self.ansi_enabled {
            let mut ansi_parser: ANSIParser = Default::default();
            let text = ansi_parser.parse_ansi(&self.orig_text);
            Cow::Owned(text.into_inner())
        } else if !self.using_transform_fields && self.ansi_enabled {
            Cow::Borrowed(self.text.get_stripped())
        } else {
            Cow::Borrowed(&self.orig_text)
        }
    }

    pub fn get_index(&self) -> usize {
        self.index.1
    }

    pub fn get_full_index(&self) -> (usize, usize) {
        self.index
    }

    pub fn get_matching_ranges(&self) -> &[(usize, usize)] {
        &self.matching_ranges
    }
}

impl Clone for Item {
    fn clone(&self) -> Item {
        Item {
            index: self.index,
            orig_text: self.orig_text.clone(),
            text: self.text.clone(),
            using_transform_fields: self.using_transform_fields,
            matching_ranges: self.matching_ranges.clone(),
            ansi_enabled: self.ansi_enabled,
        }
    }
}

pub type Rank = [i64; 4]; // score, index, start, end

#[derive(PartialEq, Eq, Clone, Debug)]
#[allow(dead_code)]
pub enum MatchedRange {
    ByteRange(usize, usize), // range of bytes
    Chars(Vec<usize>),       // individual characters matched
}

#[derive(Clone, Debug)]
pub struct MatchedItem {
    pub item: Arc<Item>,
    pub rank: Rank,
    pub matched_range: Option<MatchedRange>, // range of chars that matched the pattern
}

impl MatchedItem {
    pub fn builder(item: Arc<Item>) -> Self {
        MatchedItem {
            item,
            rank: [0, 0, 0, 0],
            matched_range: None,
        }
    }

    pub fn matched_range(mut self, range: MatchedRange) -> Self {
        self.matched_range = Some(range);
        self
    }

    pub fn rank(mut self, rank: Rank) -> Self {
        self.rank = rank;
        self
    }

    pub fn build(self) -> Self {
        self
    }
}

impl Ord for MatchedItem {
    fn cmp(&self, other: &MatchedItem) -> Ordering {
        self.rank.cmp(&other.rank)
    }
}

// `PartialOrd` needs to be implemented as well.
impl PartialOrd for MatchedItem {
    fn partial_cmp(&self, other: &MatchedItem) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for MatchedItem {
    fn eq(&self, other: &MatchedItem) -> bool {
        self.rank == other.rank
    }
}

impl Eq for MatchedItem {}

const ITEM_POOL_CAPACITY : usize = 1024;

pub struct ItemPool {
    pool: SpinLock<Vec<Arc<Item>>>,
    /// number of items that was `take`n
    taken: AtomicUsize,
}

impl ItemPool {
    pub fn new() -> Self {
        Self {
            pool: SpinLock::new(Vec::with_capacity(ITEM_POOL_CAPACITY)),
            taken: AtomicUsize::new(0),
        }
    }

    pub fn len(&self) -> usize {
        let items = self.pool.lock();
        items.len()
    }

    pub fn clear(&self) {
        let mut items = self.pool.lock();
        items.clear();
        self.taken.store(0, AtomicOrdering::SeqCst);
    }

    pub fn reset(&self) {
        // lock to ensure consistency
        let items = self.pool.lock();
        self.taken.store(0, AtomicOrdering::SeqCst);
    }

    pub fn append(&self, items: &mut Vec<Arc<Item>>) {
        let mut pool = self.pool.lock();
        pool.append(items);
    }

    pub fn take(&self) -> Vec<Arc<Item>> {
        let pool = self.pool.lock();
        let len = pool.len();
        let taken = self.taken.swap(len, AtomicOrdering::SeqCst);
        let mut ret = Vec::with_capacity(len-taken);
        for item in &pool[taken..len] {
            ret.push(item.clone())
        }
        ret
    }
}
