//! The word pair: six digits, said out loud.
//!
//! A pairing code is a **numeric comparison** — two humans read the same six
//! digits off two screens and agree (`crate::pairing`). That works, and it
//! is awkward: digits are tedious to read across a room, easy to transpose,
//! and impossible to say over a phone without "no, *five* three, not three
//! five". So the same code is also rendered as two words.
//!
//! ## This carries the whole code, not a summary of it
//!
//! The list holds exactly **1000** words, and the mapping is
//! `code = index(first) * 1000 + index(second)` — a bijection with the six
//! digits, not a digest of them. That is the property the whole idea stands
//! on: two humans who compare `harbor lantern` have compared all 19.93 bits
//! of `421 733`, and an attacker bridging two pairing sessions matches the
//! pair with exactly the 10⁻⁶ per-attempt probability the digits already
//! promised. A shorter list would have been easier to write and would have
//! quietly weakened the one security property `pairing.rs` documents.
//!
//! ## How the list was chosen
//!
//! Curated, not sampled. `/usr/share/dict/words` has fifty thousand entries
//! of the right length and most of them are `aalii` and `abaca`; a word pair
//! nobody can spell is worse than the digits it replaces. So: common English
//! nouns, verbs and adjectives, four to six letters, and then a pass that
//! drops any word within one edit of a word already kept — because `flask`
//! and `flash` on two screens is exactly the comparison this exists to make
//! easy, failing.
//!
//! The list is **frozen**. Its order is the encoding; reordering it, adding
//! to it or removing from it changes what every code renders as, and two
//! nodes on different lists would show different words for the same
//! successful pairing — which reads to a user as an attack. Any change here
//! is a protocol change.

/// Words the pair is drawn from. Exactly 1000, sorted, distinct.
pub const WORDS: [&str; 1000] = [
    "acorn", "adapt", "agile", "alder", "alert", "amend", "anvil", "apple", "apron", "aqua",
    "arch", "arena", "arrive", "ascend", "aspen", "assist", "attach", "attend", "attic", "auburn",
    "avenue", "axle", "azure", "bacon", "badger", "bagel", "bamboo", "banana", "banner", "barley",
    "barn", "barrel", "basil", "basket", "bathe", "batter", "bazaar", "beach", "bear", "beauty",
    "beaver", "beckon", "beetle", "begin", "beige", "belt", "bend", "berry", "birch", "bird",
    "bison", "black", "blade", "bloom", "blue", "bluff", "board", "boot", "borrow", "bottle",
    "bounce", "bounty", "branch", "brave", "bread", "breeze", "brew", "briar", "brick", "bright",
    "bronze", "broth", "brown", "brush", "bucket", "buckle", "build", "bull", "bunker", "button",
    "cabin", "cable", "cactus", "calf", "camel", "camera", "canal", "candid", "candle", "candy",
    "canoe", "canvas", "canyon", "carton", "carve", "case", "castle", "cavern", "cedar", "center",
    "chain", "chalk", "chapel", "chasm", "cheer", "cheese", "cherry", "chest", "chick", "chief",
    "chili", "chisel", "chive", "choose", "chorus", "church", "cider", "cinema", "city", "civil",
    "clamp", "clause", "clean", "cliff", "climb", "cling", "clock", "close", "cloud", "clover",
    "coarse", "coast", "cobalt", "cobra", "cocoa", "coffee", "coil", "collar", "colony", "colt",
    "comb", "comet", "condor", "cookie", "copper", "coral", "cork", "cosmic", "cotton", "coyote",
    "crab", "crane", "crater", "crawl", "crayon", "cream", "create", "credit", "creek", "cress",
    "crisp", "crocus", "crow", "crumb", "crust", "crypt", "curry", "custom", "cyan", "cycle",
    "dagger", "dahlia", "daisy", "dance", "daring", "date", "decide", "deer", "deft", "degree",
    "delta", "dense", "depart", "deploy", "depot", "desert", "design", "desk", "detail", "detect",
    "devote", "dial", "diary", "dingo", "direct", "dodo", "domain", "donkey", "door", "dough",
    "dove", "drake", "draw", "drawer", "drift", "drive", "drum", "duck", "dune", "duty", "dwell",
    "eager", "eagle", "early", "earth", "easy", "ebony", "echo", "edit", "effort", "egret",
    "ember", "emblem", "employ", "enable", "enact", "endure", "energy", "engage", "engine",
    "enjoy", "enlist", "enrich", "epic", "equal", "escort", "essay", "estate", "ethic", "extend",
    "fabric", "fair", "falcon", "famous", "fancy", "farm", "fast", "fasten", "favor", "fawn",
    "fence", "fennel", "fern", "ferret", "fetch", "field", "fierce", "figure", "file", "filter",
    "finch", "finish", "fish", "fjord", "flag", "flask", "flea", "flick", "float", "flood",
    "flour", "flower", "fluent", "flute", "focus", "folder", "fond", "forest", "formal", "fort",
    "forum", "foster", "foyer", "frame", "frank", "free", "fresh", "frog", "frost", "fudge",
    "fungus", "future", "galaxy", "garage", "garden", "garlic", "gather", "gauge", "gaze", "gecko",
    "genius", "gentle", "geyser", "giant", "gibbon", "ginger", "give", "glad", "glen", "glide",
    "glory", "glove", "gnat", "gold", "golden", "gong", "gopher", "gorge", "govern", "grain",
    "grape", "grass", "gravel", "gray", "green", "grind", "grotto", "grouse", "guard", "gulf",
    "gust", "habit", "hail", "hamlet", "hangar", "happy", "harbor", "hardy", "hare", "hasty",
    "hawk", "hazel", "hearty", "heavy", "hedge", "helmet", "herb", "heron", "hidden", "high",
    "hill", "hinge", "hippo", "hoist", "hollow", "holly", "honest", "honey", "honor", "hoop",
    "hope", "hornet", "horse", "hostel", "hound", "hover", "humble", "humor", "hunt", "hyena",
    "ibex", "ibis", "ideal", "idiom", "ignite", "iguana", "image", "index", "indigo", "inform",
    "inhale", "inlet", "inner", "invent", "invite", "island", "isle", "ivory", "jackal", "jade",
    "jaguar", "jelly", "join", "joyful", "juice", "jump", "jungle", "kale", "keen", "kelp",
    "khaki", "kindle", "kiosk", "kite", "kneel", "knife", "knit", "knob", "knock", "koala",
    "label", "ladder", "lagoon", "lake", "lamb", "lark", "lasso", "latch", "laurel", "lava",
    "lawful", "leaf", "learn", "leash", "ledger", "leech", "legacy", "legend", "lemon", "lemur",
    "lens", "lentil", "lesson", "lever", "lichen", "light", "lilac", "lily", "lime", "linden",
    "lion", "listen", "lively", "lizard", "llama", "lobby", "locate", "locker", "lodge", "loft",
    "logic", "lone", "lotus", "loyal", "lucid", "lucky", "lumber", "lupin", "lynx", "lyric",
    "macaw", "magic", "magnet", "magpie", "main", "maize", "manage", "mango", "manor", "mantis",
    "mantle", "maple", "marble", "margin", "market", "marmot", "maroon", "marsh", "marten",
    "marvel", "mask", "master", "mauve", "meadow", "medal", "meet", "melody", "melon", "memory",
    "mercy", "merge", "merit", "mesa", "meteor", "method", "mighty", "mild", "millet", "mingle",
    "mink", "mirror", "mist", "mitten", "mobile", "modern", "modest", "moment", "moon", "moose",
    "mosque", "moss", "moth", "motive", "motto", "mount", "mule", "museum", "myrtle", "name",
    "napkin", "native", "nature", "navy", "nebula", "nectar", "needle", "nettle", "newt", "niche",
    "nimble", "noble", "notion", "novel", "nuance", "nutmeg", "oasis", "obtain", "ocean", "ocelot",
    "ochre", "offer", "office", "olive", "omelet", "onion", "opera", "option", "orange", "orbit",
    "orchid", "order", "origin", "oriole", "osprey", "otter", "oval", "oven", "oyster", "paddle",
    "paint", "palace", "palm", "panda", "panel", "papaya", "paper", "parcel", "pardon", "parlor",
    "parrot", "pasta", "pastel", "pastry", "path", "patio", "pause", "peace", "pearl", "pebble",
    "pecan", "pencil", "pepper", "perch", "period", "permit", "pewter", "phrase", "piano",
    "pickle", "pier", "pigeon", "pika", "pillar", "pilot", "pine", "pistol", "pitch", "pizza",
    "placid", "plain", "planet", "plank", "play", "plaza", "please", "pledge", "pliers", "plow",
    "plum", "plunge", "pocket", "poem", "poise", "policy", "polish", "polite", "pollen", "ponder",
    "pony", "poodle", "poplar", "poppy", "portal", "possum", "potato", "pouch", "praise", "prawn",
    "prime", "prism", "privet", "prompt", "proof", "proper", "proud", "prove", "puffin", "pulley",
    "puma", "purple", "purse", "push", "puzzle", "python", "quail", "quaint", "quarry", "quartz",
    "quay", "query", "quest", "quick", "quilt", "quince", "quokka", "quota", "rabbit", "rack",
    "radio", "radish", "raft", "random", "rapids", "ratel", "raven", "ravine", "razor", "ready",
    "reap", "reason", "recall", "record", "reed", "refine", "reform", "regal", "regard", "region",
    "relief", "relish", "remain", "remark", "remedy", "remind", "remove", "render", "renew",
    "renown", "repair", "repeat", "reply", "rescue", "reside", "resort", "rest", "result",
    "return", "reveal", "review", "revise", "rhino", "rhythm", "rice", "riddle", "ridge", "ring",
    "ritual", "river", "roach", "robin", "robust", "roll", "rook", "rose", "rowan", "ruby",
    "rudder", "rugged", "ruin", "ruler", "rural", "russet", "sage", "salad", "salmon", "salsa",
    "salt", "sand", "sauce", "saying", "scale", "scarce", "scarf", "scene", "scheme", "school",
    "screen", "scroll", "seal", "search", "secret", "secure", "seek", "select", "sepia", "serene",
    "series", "serve", "shape", "shark", "shawl", "sheep", "shelf", "shield", "shine", "shop",
    "short", "shovel", "shrew", "shrimp", "shrub", "sienna", "sieve", "signal", "silent", "silo",
    "silver", "simple", "sketch", "skip", "skunk", "slate", "sled", "sleet", "sleeve", "slope",
    "sloth", "slug", "small", "smart", "smile", "smooth", "snail", "snake", "snow", "soar",
    "social", "solace", "solid", "somber", "sonnet", "soup", "source", "spade", "speak", "spice",
    "spider", "spin", "spirit", "sponge", "spool", "spore", "spread", "spring", "spruce", "square",
    "squid", "stag", "stalk", "stanza", "staple", "statue", "steady", "steel", "stem", "stern",
    "still", "stir", "stone", "stoop", "stork", "stout", "strait", "strap", "stream", "street",
    "strict", "stride", "strong", "stud", "studio", "sturdy", "style", "subtle", "suburb",
    "sudden", "sugar", "summit", "summon", "sunny", "sunset", "superb", "supply", "surge",
    "survey", "swamp", "swan", "swift", "swim", "swing", "switch", "sword", "symbol", "syrup",
    "system", "tablet", "taco", "tactic", "tailor", "talent", "tally", "tape", "tapir", "tart",
    "teapot", "temper", "temple", "tempo", "tenor", "tent", "tetra", "thank", "theory", "thesis",
    "thin", "thorn", "thread", "thrive", "throne", "thrush", "thyme", "ticket", "tide", "tiger",
    "tilt", "timber", "timely", "tiny", "title", "toad", "token", "tomato", "tongs", "tool",
    "topaz", "topic", "torch", "total", "toucan", "tour", "towel", "town", "trace", "trail",
    "treat", "trend", "trial", "trim", "tripod", "trout", "true", "trunk", "trusty", "truth",
    "tube", "tulip", "tundra", "tunnel", "turkey", "turnip", "turret", "turtle", "twig", "twine",
    "undo", "unfold", "unite", "unlock", "uphold", "urban", "usher", "usual", "valid", "valley",
    "valve", "vane", "vapor", "vault", "verify", "verse", "vessel", "view", "violet", "violin",
    "viper", "virtue", "vision", "visit", "visor", "vital", "vivid", "voice", "vole", "volume",
    "waffle", "wagon", "wait", "wall", "wallet", "walnut", "walrus", "wander", "ward", "washer",
    "wasp", "water", "wave", "weasel", "weevil", "weigh", "whale", "wharf", "wheat", "wheel",
    "whip", "white", "wick", "widen", "willow", "wind", "window", "wire", "wisdom", "witty",
    "wolf", "wombat", "wood", "worm", "worthy", "wrap", "wren", "wrench", "yarrow", "yellow",
    "yogurt", "yoke", "young", "zebra", "zenith", "zipper",
];

/// The two words for a six-digit code.
///
/// Returns `None` for anything that is not six digits: the mapping is only
/// defined over the code space `sas_code` produces, and inventing a
/// rendering for a malformed code would put words on a screen that no other
/// screen can match.
pub fn words_for(code: &str) -> Option<(&'static str, &'static str)> {
    if code.len() != 6 || !code.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n: usize = code.parse().ok()?;
    Some((WORDS[n / 1000], WORDS[n % 1000]))
}

/// The pair as one string: `harbor lantern`.
pub fn phrase(code: &str) -> Option<String> {
    words_for(code).map(|(a, b)| format!("{a} {b}"))
}

/// The code a word pair stands for — the inverse of [`words_for`].
///
/// This is what makes the pair *usable* rather than decorative: a person
/// reading `harbor lantern` off a headless machine's console can type it
/// where a code is asked for, and a scanner or a script can check the two
/// renderings against each other without a human in the middle.
pub fn code_for(phrase: &str) -> Option<String> {
    let mut parts = phrase.split_whitespace();
    let first = parts.next()?.to_ascii_lowercase();
    let second = parts.next()?.to_ascii_lowercase();
    if parts.next().is_some() {
        return None;
    }
    // Binary search is sound because the list is sorted, and the test below
    // is what keeps it that way.
    let a = WORDS.binary_search(&first.as_str()).ok()?;
    let b = WORDS.binary_search(&second.as_str()).ok()?;
    Some(format!("{:06}", a * 1000 + b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn the_list_is_exactly_a_thousand_sorted_distinct_words() {
        assert_eq!(WORDS.len(), 1000, "the mapping is base 1000");
        assert_eq!(
            WORDS.iter().collect::<HashSet<_>>().len(),
            1000,
            "a repeated word would make two codes render identically"
        );
        let mut sorted = WORDS;
        sorted.sort_unstable();
        assert_eq!(sorted, WORDS, "code_for binary-searches this list");
        for w in WORDS {
            assert!(
                (4..=6).contains(&w.len()) && w.bytes().all(|b| b.is_ascii_lowercase()),
                "{w} is not a plain four-to-six-letter word"
            );
        }
    }

    #[test]
    fn no_two_words_are_one_edit_apart() {
        // The property the curation pass exists to produce: `flask` and
        // `flash` on two screens defeats the whole point of the pair.
        fn one_edit(a: &str, b: &str) -> bool {
            let (a, b) = (a.as_bytes(), b.as_bytes());
            if a.len() == b.len() {
                return a.iter().zip(b).filter(|(x, y)| x != y).count() == 1;
            }
            if a.len().abs_diff(b.len()) != 1 {
                return false;
            }
            let (short, long) = if a.len() < b.len() { (a, b) } else { (b, a) };
            (0..long.len()).any(|i| {
                let mut trimmed = long.to_vec();
                trimmed.remove(i);
                trimmed == short
            })
        }
        for (i, a) in WORDS.iter().enumerate() {
            for b in &WORDS[i + 1..] {
                assert!(!one_edit(a, b), "{a} and {b} are one edit apart");
            }
        }
    }

    #[test]
    fn every_code_round_trips() {
        // Exhaustive: the space is a million and the mapping is the whole
        // security claim, so there is no reason to sample it.
        for n in 0..1_000_000u32 {
            let code = format!("{n:06}");
            let phrase = phrase(&code).expect("every six-digit code has a pair");
            assert_eq!(code_for(&phrase).as_deref(), Some(code.as_str()));
        }
    }

    #[test]
    fn distinct_codes_get_distinct_pairs() {
        assert_ne!(phrase("000000"), phrase("000001"));
        assert_ne!(phrase("000000"), phrase("001000"));
        assert_eq!(phrase("000000"), Some(format!("{} {}", WORDS[0], WORDS[0])));
        assert_eq!(
            phrase("999999"),
            Some(format!("{} {}", WORDS[999], WORDS[999]))
        );
    }

    #[test]
    fn a_malformed_code_gets_no_words_rather_than_wrong_ones() {
        assert_eq!(words_for(""), None);
        assert_eq!(words_for("12345"), None);
        assert_eq!(words_for("1234567"), None);
        assert_eq!(words_for("12345x"), None);
        assert_eq!(words_for("12 345"), None);
    }

    #[test]
    fn a_pair_is_read_back_case_and_space_insensitively() {
        let code = "421733";
        let phrase = phrase(code).expect("a pair");
        let (a, b) = phrase.split_once(' ').expect("two words");
        assert_eq!(
            code_for(&format!("  {}   {}  ", a.to_uppercase(), b)).as_deref(),
            Some(code),
            "somebody typing it back should not have to match whitespace"
        );
    }

    #[test]
    fn nonsense_is_refused() {
        assert_eq!(code_for("harbor"), None, "one word is not a pair");
        assert_eq!(code_for("harbor lantern extra"), None);
        assert_eq!(code_for("zzzz qqqq"), None, "not in the list");
        assert_eq!(code_for(""), None);
    }
}
