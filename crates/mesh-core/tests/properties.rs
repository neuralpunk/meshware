//! Properties of the parsers a person's typing reaches.
//!
//! Brace expansion, SSH targets and selectors all take text straight from
//! a form or a command line, and the hand-picked cases in their unit tests
//! are the shapes somebody thought of. These are the shapes nobody did:
//! random input never panics, bounded input stays bounded, and what a
//! parser prints back parses to the same thing.
#![allow(clippy::unwrap_used)]

use mesh_core::deploy::{Target, expand, expand_all};
use mesh_core::membership::Selector;
use proptest::prelude::*;

proptest! {
    /// Anything a person can type into a target field is either expanded
    /// or refused with a sentence — never a panic, never more than the cap.
    #[test]
    fn expansion_never_panics_and_stays_bounded(s in "[a-z0-9@.{},/:_-]{0,32}") {
        if let Ok(v) = expand(&s) {
            prop_assert!(!v.is_empty());
            prop_assert!(v.len() <= 4096);
            // A plain word comes back as itself.
            if !s.contains('{') {
                prop_assert_eq!(v, vec![s.clone()]);
            }
        }
    }

    /// A numeric range expands to exactly its count, in order, with no
    /// duplicates, whichever way round it was written.
    #[test]
    fn a_range_expands_to_its_count(lo in 0i64..300, hi in 0i64..300) {
        let v = expand(&format!("n{{{lo}..{hi}}}")).unwrap();
        let count = (lo - hi).unsigned_abs() as usize + 1;
        prop_assert_eq!(v.len(), count);
        let mut seen = std::collections::BTreeSet::new();
        for x in &v {
            prop_assert!(seen.insert(x.clone()), "duplicate {x}");
        }
        let first = format!("n{lo}");
        let last = format!("n{hi}");
        prop_assert_eq!(v.first().map(String::as_str), Some(first.as_str()));
        prop_assert_eq!(v.last().map(String::as_str), Some(last.as_str()));
    }

    /// `expand_all` on any text is a list with no duplicates and no blanks.
    #[test]
    fn a_pasted_list_has_no_duplicates(s in "[a-z0-9@.{},/ \n#-]{0,64}") {
        if let Ok(v) = expand_all(&[s]) {
            let mut seen = std::collections::BTreeSet::new();
            for x in &v {
                prop_assert!(!x.trim().is_empty());
                prop_assert!(seen.insert(x.clone()));
            }
        }
    }

    /// What `Target::label` prints parses back to the same target.
    #[test]
    fn a_target_label_round_trips(
        user in proptest::option::of("[a-z][a-z0-9_-]{0,11}"),
        host in "[a-z][a-z0-9.-]{0,23}",
        port in 1u16..=65535,
    ) {
        let raw = match &user {
            Some(u) => format!("{u}@{host}:{port}"),
            None => format!("{host}:{port}"),
        };
        let t = Target::parse(&raw).unwrap();
        prop_assert_eq!(&t.user, &user);
        prop_assert_eq!(&t.host, &host);
        prop_assert_eq!(t.port, port);
        let again = Target::parse(&t.label()).unwrap();
        prop_assert_eq!(again, t);
    }

    /// An IPv6 literal, bracketed or bare, comes through as the address.
    #[test]
    fn an_ipv6_target_round_trips(a in proptest::arbitrary::any::<std::net::Ipv6Addr>(), port in 1u16..=65535) {
        let t = Target::parse(&format!("ops@[{a}]:{port}")).unwrap();
        prop_assert_eq!(t.host.parse::<std::net::Ipv6Addr>().unwrap(), a);
        prop_assert_eq!(t.port, port);
        let again = Target::parse(&t.label()).unwrap();
        prop_assert_eq!(again, t);
        let bare = Target::parse(&a.to_string()).unwrap();
        prop_assert_eq!(bare.port, 22);
    }

    /// Any string is a target or a sentence.
    #[test]
    fn any_string_is_a_target_or_a_sentence(s in "\\PC{0,40}") {
        let _ = Target::parse(&s);
    }

    /// Any string is a selector, and asking it about itself never panics.
    #[test]
    fn any_string_is_a_selector(s in "\\PC{0,48}") {
        let sel = Selector::parse(&s);
        let _ = sel.is_unbounded();
        let _ = format!("{sel:?}");
    }

    /// `any` is unbounded however it is decorated with `|`.
    #[test]
    fn any_stays_unbounded_under_any_or(extra in "[a-z0-9=]{1,12}") {
        let trailing = format!("any|{extra}");
        let leading = format!("{extra}|any");
        prop_assert!(Selector::parse(&trailing).is_unbounded());
        prop_assert!(Selector::parse(&leading).is_unbounded());
    }
}
