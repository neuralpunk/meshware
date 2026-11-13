//! Text that came from somebody else, made safe to print (SEC-9).
//!
//! A pairing screen printed the mesh name and the approver's name from an
//! unauthenticated message, raw, right above the six-digit code the two
//! people are about to compare. Nothing stripped control characters, so a
//! machine in the middle could send a name carrying cursor movement and
//! paint a code of its own over the real one — which defeats the only thing
//! pairing asks a person to check. Node names travel the same way into
//! every table the product prints.
//!
//! One rule, here, for all of it: no C0 or C1 control characters, no
//! Unicode direction or zero-width controls, and a length cap.

/// Longest node or mesh name kept, in characters.
pub const MAX_NAME_CHARS: usize = 64;

/// Would printing `c` do something other than show a character?
fn is_hazard(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{061C}'
                | '\u{200B}'..='\u{200F}'
                | '\u{2028}'..='\u{202E}'
                | '\u{2060}'..='\u{2069}'
                | '\u{FEFF}'
        )
}

/// Is `s` already safe to print, and no longer than `max_chars`?
pub fn is_clean(s: &str, max_chars: usize) -> bool {
    s.chars().count() <= max_chars && !s.chars().any(is_hazard)
}

/// `s` with every hazardous character removed and cut to `max_chars`, an
/// ellipsis marking the cut.
pub fn clean(s: &str, max_chars: usize) -> String {
    let kept: Vec<char> = s.chars().filter(|c| !is_hazard(*c)).collect();
    if kept.len() <= max_chars {
        return kept.into_iter().collect();
    }
    let mut out: String = kept[..max_chars.saturating_sub(1)].iter().collect();
    out.push('…');
    out
}

/// Why `name` cannot be a node's name, if it cannot.
pub fn name_problem(name: &str) -> Option<String> {
    if name.trim().is_empty() {
        return Some("a node name must not be empty".into());
    }
    if name.chars().any(is_hazard) {
        return Some(
            "a node name must not contain control, direction or zero-width characters".into(),
        );
    }
    if name.chars().count() > MAX_NAME_CHARS {
        return Some(format!(
            "a node name must be at most {MAX_NAME_CHARS} characters"
        ));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The attack the rule exists for: a name that moves the cursor up and
    /// prints a code of its own.
    #[test]
    fn a_name_cannot_repaint_the_screen() {
        let evil = "laptop\x1b[2A\r  123 456\x1b[K\u{202E}";
        let shown = clean(evil, MAX_NAME_CHARS);
        assert!(!shown.contains('\x1b'));
        assert!(!shown.contains('\r'));
        assert!(!shown.contains('\u{202E}'));
        assert_eq!(shown, "laptop[2A  123 456[K");
        assert!(!is_clean(evil, MAX_NAME_CHARS));
        assert!(name_problem(evil).is_some());
    }

    #[test]
    fn ordinary_names_pass_untouched() {
        for name in ["alpha", "gpu-07.lab", "Büro-Rechner", "東京-1", "a b"] {
            assert!(is_clean(name, MAX_NAME_CHARS), "{name}");
            assert_eq!(clean(name, MAX_NAME_CHARS), name);
            assert!(name_problem(name).is_none(), "{name}");
        }
    }

    #[test]
    fn long_text_is_cut_on_a_character_boundary() {
        let long = "é".repeat(100);
        let shown = clean(&long, 10);
        assert_eq!(shown.chars().count(), 10);
        assert!(shown.ends_with('…'));
        assert!(name_problem(&long).is_some());
        assert!(name_problem("   ").is_some());
    }

    #[test]
    fn zero_width_and_bidi_controls_are_removed() {
        for c in [
            '\u{200B}', '\u{200D}', '\u{2066}', '\u{2069}', '\u{FEFF}', '\u{061C}',
        ] {
            let s = format!("ab{c}cd");
            assert_eq!(clean(&s, 64), "abcd", "{:?}", c);
        }
    }
}
