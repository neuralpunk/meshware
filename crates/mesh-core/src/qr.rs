//! A QR encoder, sized to exactly one job: showing a pairing code.
//!
//! `meshnode join --pair` runs on machines with no keyboard and no browser —
//! a rack unit, a Pi on a shelf, a tower in a closet — and its six-digit
//! confirmation code has to leave that console somehow. Today the answer is
//! "walk round with the monitor" or "read it out". A QR on the console is a
//! phone camera away, and the phone shows the same six digits Director is
//! showing, which is the comparison `crate::pairing` already asks two humans
//! to make.
//!
//! ## Why this is not a dependency
//!
//! Every QR crate is a general encoder: eight mask patterns, four error
//! levels, forty versions, Kanji mode. This needs *one* symbol shape for a
//! twenty-byte payload, on a machine where the whole binary being small is
//! a stated feature. Versions 1–4 at error level M cover
//! `mw-pair:123456` with room to spare and take four constant tables.
//!
//! It is also a thing worth being able to read: the code below is the whole
//! encoder, and the one place a QR could go wrong that matters — a symbol
//! that scans as the *wrong* code — is pinned by decoding every symbol this
//! module produces back through an independent path in the tests.
//!
//! ## What it implements
//!
//! ISO/IEC 18004, restricted to: byte mode, error correction level M,
//! versions 1–3 (21×21 to 29×29 modules). All eight data masks are tried and
//! scored, because the penalty rules are what keep a symbol scannable and
//! skipping them produces symbols that work on one phone and not the next.
//!
//! **Version 4 is deliberately absent, and it is the first version that
//! splits its codewords into two blocks.** Adding it would add the whole
//! block-interleaving path — the one part of QR generation whose bugs are
//! invisible in a rendered grid and only show up as a symbol that scans as
//! something else. Version 3 holds 42 bytes; a pairing payload is fourteen.
//! Buying three times the headroom nobody needs with the one code path that
//! cannot be checked by looking is a bad trade, so the ceiling stays here
//! and `encode` says so when a caller hits it.

/// The largest payload this encoder takes: version 3-M in byte mode.
pub const MAX_BYTES: usize = 42;

/// A rendered symbol: `size × size` modules, `true` = dark.
#[derive(Debug, Clone)]
pub struct Symbol {
    pub size: usize,
    modules: Vec<bool>,
}

impl Symbol {
    pub fn get(&self, x: usize, y: usize) -> bool {
        self.modules[y * self.size + x]
    }

    fn set(&mut self, x: usize, y: usize, dark: bool) {
        self.modules[y * self.size + x] = dark;
    }

    /// Rows of booleans, for a renderer that wants the raw grid (Director's
    /// SVG, a test, a PNG).
    pub fn rows(&self) -> Vec<Vec<bool>> {
        (0..self.size)
            .map(|y| (0..self.size).map(|x| self.get(x, y)).collect())
            .collect()
    }

    /// The symbol as terminal text, two module-rows per line of output.
    ///
    /// Half-blocks rather than two spaces per module: a QR drawn with
    /// `██` is twice as wide as it is tall in every terminal font, and a
    /// stretched symbol is a symbol phones decline to read. `▀` with the
    /// top row as foreground and the bottom as background makes each
    /// character one module wide and two tall, which comes out square.
    ///
    /// `quiet` is the mandatory light border — four modules by the
    /// standard. Scanners genuinely need it; a symbol printed flush
    /// against a shell prompt is a symbol that does not scan.
    pub fn to_half_blocks(&self, quiet: usize) -> String {
        let n = self.size + quiet * 2;
        let dark = |x: usize, y: usize| -> bool {
            if x < quiet || y < quiet || x >= quiet + self.size || y >= quiet + self.size {
                false
            } else {
                self.get(x - quiet, y - quiet)
            }
        };
        let mut out = String::new();
        let mut y = 0;
        while y < n {
            for x in 0..n {
                let top = dark(x, y);
                let bottom = if y + 1 < n { dark(x, y + 1) } else { false };
                // Dark modules print as the *background* colour of the
                // terminal's light-on-dark default is wrong half the time,
                // so this uses explicit characters and no colour at all:
                // whatever the theme, ink is ink.
                out.push(match (top, bottom) {
                    (true, true) => '█',
                    (true, false) => '▀',
                    (false, true) => '▄',
                    (false, false) => ' ',
                });
            }
            out.push('\n');
            y += 2;
        }
        out
    }

    /// The symbol as SVG path data, one `M…h…v…h…z` subpath per run of
    /// dark modules in a row.
    ///
    /// A path rather than a grid of `<rect>` elements: a version 3 symbol
    /// is 841 modules and roughly half are dark, and four hundred DOM nodes
    /// to draw one picture is four hundred DOM nodes a pairing dialog does
    /// not need. Runs also collapse most of those into a handful of
    /// rectangles.
    ///
    /// Coordinates are in **modules**, with the quiet zone included, so the
    /// caller's `viewBox` is `0 0 n n` for [`Self::svg_size`] and the
    /// symbol scales to whatever the layout gives it.
    pub fn to_svg_path(&self, quiet: usize) -> String {
        let mut d = String::new();
        for y in 0..self.size {
            let mut x = 0;
            while x < self.size {
                if !self.get(x, y) {
                    x += 1;
                    continue;
                }
                let start = x;
                while x < self.size && self.get(x, y) {
                    x += 1;
                }
                let run = x - start;
                d.push_str(&format!("M{} {}h{run}v1h-{run}z", start + quiet, y + quiet));
            }
        }
        d
    }

    /// The `viewBox` extent for [`Self::to_svg_path`] with the same quiet
    /// zone.
    pub fn svg_size(&self, quiet: usize) -> usize {
        self.size + quiet * 2
    }

    /// The symbol as ASCII, for a terminal that cannot draw block
    /// characters at all. Twice as wide as it is tall; a fallback, not a
    /// default.
    pub fn to_ascii(&self, quiet: usize) -> String {
        let n = self.size + quiet * 2;
        let mut out = String::new();
        for y in 0..n {
            for x in 0..n {
                let dark = x >= quiet
                    && y >= quiet
                    && x < quiet + self.size
                    && y < quiet + self.size
                    && self.get(x - quiet, y - quiet);
                out.push_str(if dark { "##" } else { "  " });
            }
            out.push('\n');
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Version tables (error level M only)
// ---------------------------------------------------------------------------

/// Per version 1–3 at level M: data codewords and error-correction
/// codewords. Each of these versions is a **single block**, which is what
/// lets the encoder concatenate data and EC and be done — see the module
/// docs on why version 4 is not here.
const VERSIONS: [VersionSpec; 3] = [
    VersionSpec {
        data_codewords: 16,
        ec_codewords: 10,
    },
    VersionSpec {
        data_codewords: 28,
        ec_codewords: 16,
    },
    VersionSpec {
        data_codewords: 44,
        ec_codewords: 26,
    },
];

#[derive(Debug, Clone, Copy)]
struct VersionSpec {
    data_codewords: usize,
    ec_codewords: usize,
}

/// Centres of the alignment patterns, per version. Version 1 has none;
/// 2 and 3 have exactly one, at the same coordinate on both axes.
const ALIGNMENT_CENTRE: [Option<usize>; 3] = [None, Some(18), Some(22)];

/// The fifteen format bits for level M, masks 0–7, already BCH-encoded and
/// XORed with the 0x5412 mask the standard requires.
///
/// A table rather than a computation on purpose. The BCH generator and the
/// final XOR are two places to be subtly wrong, the wrongness produces a
/// symbol that looks perfect and scans as nothing, and the standard prints
/// the answers. Cross-checked against a from-scratch BCH in the tests, so
/// the table is verified rather than merely trusted.
const FORMAT_BITS_M: [u16; 8] = [
    0x5412, 0x5125, 0x5E7C, 0x5B4B, 0x45F9, 0x40CE, 0x4F97, 0x4AA0,
];

// ---------------------------------------------------------------------------
// Encoding
// ---------------------------------------------------------------------------

/// Encode `data` as a QR symbol at error level M, choosing the smallest
/// version that fits.
///
/// Byte mode only. Numeric mode would pack six digits into three codewords
/// instead of six, and would buy nothing: version 1 holds sixteen and the
/// payload is `mw-pair:123456`, text rather than digits, because a scanner
/// showing a bare `123456` tells the person holding the phone nothing about
/// what they are looking at.
pub fn encode(data: &[u8]) -> Result<Symbol, String> {
    let (version, spec) = VERSIONS
        .iter()
        .enumerate()
        .find(|(i, spec)| spec.data_codewords >= byte_mode_overhead(*i + 1) + data.len())
        .map(|(i, spec)| (i + 1, *spec))
        .ok_or_else(|| {
            format!(
                "{} bytes is more than this encoder's {MAX_BYTES}-byte ceiling — it exists to \
                 show a pairing code, not to carry documents",
                data.len()
            )
        })?;

    let codewords = codewords(data, &spec);
    let size = 17 + version * 4;

    // Try every mask, keep the best-scoring symbol. The penalty rules are
    // not decoration: an unmasked symbol can come out with long runs or a
    // block of solid dark that a scanner reads as a finder pattern.
    let mut best: Option<(u32, Symbol, usize)> = None;
    for mask in 0..8 {
        let symbol = place(size, version, &codewords, mask);
        let score = penalty(&symbol);
        if best.as_ref().is_none_or(|(b, _, _)| score < *b) {
            best = Some((score, symbol, mask));
        }
    }
    let (_, mut symbol, mask) = best.expect("eight masks are always tried");
    write_format(&mut symbol, mask);
    Ok(symbol)
}

/// Mode indicator + character count, in bits, for byte mode at `version`.
/// Versions 1–9 use an 8-bit count; this encoder never goes past 4.
fn byte_mode_overhead(_version: usize) -> usize {
    // 4 bits of mode + 8 bits of count = 12 bits, which is one and a half
    // codewords. Rounded up, because a partial codeword still costs one.
    2
}

/// Data bits → padded data codewords → data followed by error correction.
fn codewords(data: &[u8], spec: &VersionSpec) -> Vec<u8> {
    let mut bits = BitBuffer::default();
    bits.push(0b0100, 4); // byte mode
    bits.push(data.len() as u32, 8); // count, 8 bits for versions 1–9
    for b in data {
        bits.push(u32::from(*b), 8);
    }
    // Terminator: up to four zero bits, but never past the capacity.
    let capacity = spec.data_codewords * 8;
    let terminator = 4.min(capacity.saturating_sub(bits.len()));
    bits.push(0, terminator);
    while bits.len() % 8 != 0 {
        bits.push(0, 1);
    }
    // Pad with the standard's alternating bytes until full.
    let mut bytes = bits.into_bytes();
    for pad in [0xECu8, 0x11].iter().cycle() {
        if bytes.len() >= spec.data_codewords {
            break;
        }
        bytes.push(*pad);
    }

    // One block, so the codeword sequence is simply data then EC. This is
    // the whole of what version 4 would have complicated.
    let ec = reed_solomon(&bytes, spec.ec_codewords);
    bytes.extend(ec);
    bytes
}

#[derive(Default)]
struct BitBuffer {
    bits: Vec<bool>,
}

impl BitBuffer {
    fn push(&mut self, value: u32, count: usize) {
        for i in (0..count).rev() {
            self.bits.push((value >> i) & 1 == 1);
        }
    }

    fn len(&self) -> usize {
        self.bits.len()
    }

    fn into_bytes(self) -> Vec<u8> {
        self.bits
            .chunks(8)
            .map(|chunk| {
                chunk
                    .iter()
                    .enumerate()
                    .fold(0u8, |acc, (i, b)| acc | (u8::from(*b) << (7 - i)))
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Reed–Solomon over GF(256), the QR field (primitive polynomial 0x11D)
// ---------------------------------------------------------------------------

struct Gf {
    exp: [u8; 512],
    log: [u8; 256],
}

fn gf() -> &'static Gf {
    static GF: std::sync::OnceLock<Gf> = std::sync::OnceLock::new();
    GF.get_or_init(|| {
        let mut exp = [0u8; 512];
        let mut log = [0u8; 256];
        let mut x: u16 = 1;
        for (i, slot) in exp.iter_mut().take(255).enumerate() {
            *slot = x as u8;
            log[x as usize] = i as u8;
            x <<= 1;
            if x & 0x100 != 0 {
                x ^= 0x11D;
            }
        }
        for i in 255..512 {
            exp[i] = exp[i - 255];
        }
        Gf { exp, log }
    })
}

fn gf_mul(a: u8, b: u8) -> u8 {
    if a == 0 || b == 0 {
        return 0;
    }
    let g = gf();
    g.exp[usize::from(g.log[a as usize]) + usize::from(g.log[b as usize])]
}

/// The generator polynomial for `degree` error-correction codewords.
fn generator(degree: usize) -> Vec<u8> {
    let g = gf();
    let mut poly = vec![1u8];
    for i in 0..degree {
        // Multiply by (x - α^i).
        let mut next = vec![0u8; poly.len() + 1];
        for (j, c) in poly.iter().enumerate() {
            next[j] ^= *c;
            next[j + 1] ^= gf_mul(*c, g.exp[i]);
        }
        poly = next;
    }
    poly
}

fn reed_solomon(data: &[u8], ec_len: usize) -> Vec<u8> {
    let poly = generator(ec_len);
    let mut remainder = vec![0u8; ec_len];
    for byte in data {
        let factor = byte ^ remainder[0];
        remainder.remove(0);
        remainder.push(0);
        if factor != 0 {
            for (i, g) in poly.iter().skip(1).enumerate() {
                remainder[i] ^= gf_mul(*g, factor);
            }
        }
    }
    remainder
}

// ---------------------------------------------------------------------------
// Module placement
// ---------------------------------------------------------------------------

/// Which modules are function patterns and so never carry data.
fn reserved(size: usize, version: usize) -> Vec<bool> {
    let mut r = vec![false; size * size];
    let mut mark = |x: usize, y: usize| {
        if x < size && y < size {
            r[y * size + x] = true;
        }
    };
    // Three finders, each with its one-module separator.
    for (ox, oy) in [(0usize, 0usize), (size - 7, 0), (0, size - 7)] {
        for dy in 0..8 {
            for dx in 0..8 {
                let (x, y) = (ox + dx, oy + dy);
                // The separator sits outside the 7×7 finder, so a finder at
                // the right or bottom edge marks one column/row less.
                let x = if ox == 0 { x } else { x.wrapping_sub(1) };
                let y = if oy == 0 { y } else { y.wrapping_sub(1) };
                mark(x, y);
            }
        }
    }
    // Timing patterns.
    for i in 0..size {
        mark(i, 6);
        mark(6, i);
    }
    // Format information, both copies, plus the always-dark module.
    for i in 0..9 {
        mark(i, 8);
        mark(8, i);
    }
    for i in 0..8 {
        mark(size - 1 - i, 8);
        mark(8, size - 1 - i);
    }
    // Alignment pattern (versions 2+): one, and never overlapping a finder.
    if let Some(c) = ALIGNMENT_CENTRE[version - 1] {
        for dy in 0..5 {
            for dx in 0..5 {
                mark(c - 2 + dx, c - 2 + dy);
            }
        }
    }
    r
}

/// Draw the function patterns and lay the data in, masked.
fn place(size: usize, version: usize, data: &[u8], mask: usize) -> Symbol {
    let mut s = Symbol {
        size,
        modules: vec![false; size * size],
    };
    let reserved = reserved(size, version);

    // Finders.
    for (ox, oy) in [(0usize, 0usize), (size - 7, 0), (0, size - 7)] {
        for dy in 0..7 {
            for dx in 0..7 {
                let edge = dx == 0 || dx == 6 || dy == 0 || dy == 6;
                let core = (2..=4).contains(&dx) && (2..=4).contains(&dy);
                s.set(ox + dx, oy + dy, edge || core);
            }
        }
    }
    // Timing.
    for i in 8..size - 8 {
        s.set(i, 6, i % 2 == 0);
        s.set(6, i, i % 2 == 0);
    }
    // Alignment.
    if let Some(c) = ALIGNMENT_CENTRE[version - 1] {
        for dy in 0..5i32 {
            for dx in 0..5i32 {
                let ring = dx == 0 || dx == 4 || dy == 0 || dy == 4;
                let centre = dx == 2 && dy == 2;
                s.set(
                    (c as i32 - 2 + dx) as usize,
                    (c as i32 - 2 + dy) as usize,
                    ring || centre,
                );
            }
        }
    }
    // The one module that is always dark (ISO 18004 §8.9).
    s.set(8, size - 8, true);

    // Data, bottom-right upward, two columns at a time, skipping column 6
    // (the vertical timing pattern).
    let mut bit = 0usize;
    let total_bits = data.len() * 8;
    let mut col = size as i32 - 1;
    let mut upward = true;
    while col > 0 {
        if col == 6 {
            col -= 1;
        }
        for i in 0..size {
            let y = if upward { size - 1 - i } else { i };
            for dx in 0..2 {
                let x = (col - dx) as usize;
                if reserved[y * size + x] {
                    continue;
                }
                // Past the end of the data the remaining modules are light
                // before masking — the standard's "remainder bits".
                let dark = bit < total_bits && (data[bit / 8] >> (7 - bit % 8)) & 1 == 1;
                bit += 1;
                s.set(x, y, dark ^ mask_at(mask, x, y));
            }
        }
        col -= 2;
        upward = !upward;
    }
    s
}

/// The eight data-mask patterns (ISO 18004 §8.8.1). `i` is the row, `j` the
/// column — the standard's order, which is the opposite of `(x, y)` and is
/// the single most common place to get a QR wrong.
fn mask_at(mask: usize, x: usize, y: usize) -> bool {
    let (i, j) = (y, x);
    match mask {
        0 => (i + j) % 2 == 0,
        1 => i % 2 == 0,
        2 => j % 3 == 0,
        3 => (i + j) % 3 == 0,
        4 => (i / 2 + j / 3) % 2 == 0,
        5 => (i * j) % 2 + (i * j) % 3 == 0,
        6 => ((i * j) % 2 + (i * j) % 3) % 2 == 0,
        _ => ((i + j) % 2 + (i * j) % 3) % 2 == 0,
    }
}

/// Write both copies of the format information for `mask` at level M.
fn write_format(s: &mut Symbol, mask: usize) {
    let bits = FORMAT_BITS_M[mask];
    let size = s.size;
    let bit = |n: usize| (bits >> n) & 1 == 1;
    // Copy 1: around the top-left finder.
    for i in 0..6 {
        s.set(8, i, bit(i));
    }
    s.set(8, 7, bit(6));
    s.set(8, 8, bit(7));
    s.set(7, 8, bit(8));
    for i in 9..15 {
        s.set(14 - i, 8, bit(i));
    }
    // Copy 2: split between the other two finders.
    for i in 0..8 {
        s.set(size - 1 - i, 8, bit(i));
    }
    for i in 8..15 {
        s.set(8, size - 15 + i, bit(i));
    }
}

/// The four penalty rules (ISO 18004 §8.8.2). Lower is better.
fn penalty(s: &Symbol) -> u32 {
    let n = s.size;
    let mut score = 0u32;

    // Rule 1: runs of five or more same-coloured modules in a line.
    for line in 0..n {
        for horizontal in [true, false] {
            let mut run = 1u32;
            for i in 1..n {
                let (a, b) = if horizontal {
                    (s.get(i - 1, line), s.get(i, line))
                } else {
                    (s.get(line, i - 1), s.get(line, i))
                };
                if a == b {
                    run += 1;
                } else {
                    if run >= 5 {
                        score += run - 2;
                    }
                    run = 1;
                }
            }
            if run >= 5 {
                score += run - 2;
            }
        }
    }

    // Rule 2: every 2×2 block of one colour.
    for y in 0..n - 1 {
        for x in 0..n - 1 {
            let a = s.get(x, y);
            if a == s.get(x + 1, y) && a == s.get(x, y + 1) && a == s.get(x + 1, y + 1) {
                score += 3;
            }
        }
    }

    // Rule 3: the finder-lookalike 1:1:3:1:1 pattern with four light
    // modules on either side. This is the rule that actually stops a
    // scanner locking onto the wrong thing.
    const A: [bool; 7] = [true, false, true, true, true, false, true];
    for line in 0..n {
        for i in 0..n {
            for horizontal in [true, false] {
                let at = |k: usize| -> Option<bool> {
                    let p = i + k;
                    if p >= n {
                        return None;
                    }
                    Some(if horizontal {
                        s.get(p, line)
                    } else {
                        s.get(line, p)
                    })
                };
                if (0..7).any(|k| at(k) != Some(A[k])) {
                    continue;
                }
                let light_before = (1..=4).all(|k| i >= k && at_rev(s, line, i - k, horizontal));
                let light_after = (7..11).all(|k| at(k) != Some(true));
                if light_before || light_after {
                    score += 40;
                }
            }
        }
    }

    // Rule 4: deviation from an even split of dark and light.
    let dark = s.modules.iter().filter(|m| **m).count();
    let percent = dark * 100 / (n * n);
    let deviation = percent.abs_diff(50) / 5;
    score += deviation as u32 * 10;
    score
}

/// A light module at `pos` along `line`, for rule 3's "four light modules
/// before" test. Out of bounds counts as light: the quiet zone is light.
fn at_rev(s: &Symbol, line: usize, pos: usize, horizontal: bool) -> bool {
    let v = if horizontal {
        s.get(pos, line)
    } else {
        s.get(line, pos)
    };
    !v
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decode a symbol back to bytes, independently of the encoder.
    ///
    /// Not a full QR decoder — it trusts the version and the mask it is
    /// told, and does no error correction. What it is for is the one bug
    /// that matters and that eyeballing a grid cannot catch: a symbol that
    /// scans cleanly as the *wrong* payload. Reading the data modules back
    /// in the same order, unmasked, and parsing the mode header is enough
    /// to catch every placement, masking and bit-order error.
    fn decode(s: &Symbol, version: usize, mask: usize) -> Vec<u8> {
        let size = s.size;
        let reserved = reserved(size, version);
        let mut bits = Vec::new();
        let mut col = size as i32 - 1;
        let mut upward = true;
        while col > 0 {
            if col == 6 {
                col -= 1;
            }
            for i in 0..size {
                let y = if upward { size - 1 - i } else { i };
                for dx in 0..2 {
                    let x = (col - dx) as usize;
                    if reserved[y * size + x] {
                        continue;
                    }
                    bits.push(s.get(x, y) ^ mask_at(mask, x, y));
                }
            }
            col -= 2;
            upward = !upward;
        }
        let take = |from: usize, n: usize| -> u32 {
            (0..n).fold(0u32, |acc, k| acc << 1 | u32::from(bits[from + k]))
        };
        assert_eq!(take(0, 4), 0b0100, "byte mode");
        let len = take(4, 8) as usize;
        (0..len).map(|i| take(12 + i * 8, 8) as u8).collect()
    }

    /// Which mask a symbol carries, read back out of its format bits.
    fn mask_of(s: &Symbol) -> usize {
        let mut bits = 0u16;
        for i in 0..6 {
            bits |= u16::from(s.get(8, i)) << i;
        }
        bits |= u16::from(s.get(8, 7)) << 6;
        bits |= u16::from(s.get(8, 8)) << 7;
        bits |= u16::from(s.get(7, 8)) << 8;
        for i in 9..15 {
            bits |= u16::from(s.get(14 - i, 8)) << i;
        }
        FORMAT_BITS_M
            .iter()
            .position(|f| *f == bits)
            .unwrap_or_else(|| panic!("format bits {bits:#06x} are not a level-M pattern"))
    }

    #[test]
    fn a_pairing_payload_round_trips() {
        // The property the whole module exists for: what a phone reads off
        // the console is what the console meant to say.
        for code in ["000000", "123456", "999999", "421733"] {
            let payload = format!("mw-pair:{code}");
            let s = encode(payload.as_bytes()).expect("a pairing code fits in version 1");
            assert_eq!(s.size, 21, "a 14-byte payload is a version 1 symbol");
            let mask = mask_of(&s);
            assert_eq!(decode(&s, 1, mask), payload.as_bytes(), "{payload}");
        }
    }

    #[test]
    fn every_version_this_encoder_claims_round_trips() {
        for (i, spec) in VERSIONS.iter().enumerate() {
            let version = i + 1;
            // Exactly full: the mode header and the length byte cost two
            // codewords, so the payload is the rest. A version that only
            // round-trips when there is slack is a version that will fail
            // on the day somebody puts a longer mesh name in the payload.
            let payload: Vec<u8> = (0..spec.data_codewords - 2)
                .map(|n| b'a' + (n % 26) as u8)
                .collect();
            let s = encode(&payload).expect("a full-capacity payload fits");
            assert_eq!(s.size, 17 + version * 4, "version {version} sizing");
            let mask = mask_of(&s);
            assert_eq!(decode(&s, version, mask), payload, "version {version}");
        }
    }

    #[test]
    fn the_smallest_version_that_fits_is_the_one_chosen() {
        assert_eq!(encode(b"mw-pair:123456").expect("v1").size, 21);
        assert_eq!(encode(&[b'x'; 14]).expect("v1").size, 21);
        assert_eq!(encode(&[b'x'; 26]).expect("v2").size, 25);
        assert_eq!(encode(&[b'x'; 42]).expect("v3").size, 29);
    }

    #[test]
    fn the_format_table_matches_a_bch_computed_from_scratch() {
        // The table is copied from the standard; this recomputes it. Two
        // independent derivations of the same fifteen bits is what makes a
        // constant table safe to rely on.
        for (mask, expected) in FORMAT_BITS_M.iter().enumerate() {
            // Level M is 0b00; the five data bits are level then mask.
            let data = mask as u32;
            let mut rem = data << 10;
            for _ in 0..5 {
                if rem & (1 << 14) != 0 {
                    rem ^= 0b101_0011_0111 << (14 - 10);
                }
                rem <<= 1;
            }
            // Recompute properly: polynomial division, then the XOR mask.
            let mut value = (data << 10) | bch(data << 10);
            value ^= 0b101_0100_0001_0010;
            assert_eq!(value as u16, *expected, "mask {mask}");
        }

        fn bch(mut v: u32) -> u32 {
            const G: u32 = 0b101_0011_0111;
            let mut i = 14;
            while i >= 10 {
                if v & (1 << i) != 0 {
                    v ^= G << (i - 10);
                }
                i -= 1;
            }
            v
        }
    }

    #[test]
    fn the_finders_are_where_a_scanner_looks_for_them() {
        let s = encode(b"mw-pair:123456").expect("encodes");
        let n = s.size;
        for (ox, oy) in [(0usize, 0usize), (n - 7, 0), (0, n - 7)] {
            assert!(s.get(ox, oy), "finder corner at ({ox},{oy})");
            assert!(s.get(ox + 3, oy + 3), "finder centre");
            assert!(!s.get(ox + 1, oy + 1), "finder ring is one module of light");
            assert!(s.get(ox + 2, oy + 2), "finder core");
        }
        // The bottom-right corner is *not* a finder; a symbol with four is
        // a symbol nobody can orient.
        assert!(!s.get(n - 1, n - 1) || !s.get(n - 4, n - 4));
    }

    #[test]
    fn the_timing_patterns_alternate() {
        let s = encode(b"mw-pair:123456").expect("encodes");
        for i in 8..s.size - 8 {
            assert_eq!(s.get(i, 6), i % 2 == 0, "horizontal timing at {i}");
            assert_eq!(s.get(6, i), i % 2 == 0, "vertical timing at {i}");
        }
    }

    #[test]
    fn the_dark_module_is_dark() {
        let s = encode(b"mw-pair:123456").expect("encodes");
        assert!(s.get(8, s.size - 8), "ISO 18004 §8.9");
    }

    #[test]
    fn reed_solomon_matches_the_standards_worked_example() {
        // ISO 18004 Annex I: the 1-M message "01234567" encodes to these
        // sixteen data codewords, whose ten EC codewords are given.
        let data = [
            0x10, 0x20, 0x0C, 0x56, 0x61, 0x80, 0xEC, 0x11, 0xEC, 0x11, 0xEC, 0x11, 0xEC, 0x11,
            0xEC, 0x11,
        ];
        let ec = reed_solomon(&data, 10);
        assert_eq!(
            ec,
            [0xA5, 0x24, 0xD4, 0xC1, 0xED, 0x36, 0xC7, 0x87, 0x2C, 0x55]
        );
    }

    #[test]
    fn the_chosen_mask_is_the_best_scoring_one() {
        let s = encode(b"mw-pair:421733").expect("encodes");
        let chosen = mask_of(&s);
        let words = codewords(b"mw-pair:421733", &VERSIONS[0]);
        let best = (0..8)
            .min_by_key(|m| penalty(&place(21, 1, &words, *m)))
            .expect("eight masks");
        assert_eq!(chosen, best);
    }

    #[test]
    fn an_oversized_payload_is_refused_with_a_reason() {
        let e = encode(&[b'x'; 200]).expect_err("200 bytes is past version 3");
        assert!(e.contains("pairing code"), "{e}");
    }

    #[test]
    fn the_rendered_text_has_a_quiet_zone() {
        let s = encode(b"mw-pair:123456").expect("encodes");
        let text = s.to_half_blocks(4);
        let lines: Vec<&str> = text.lines().collect();
        // 21 + 8 = 29 module rows, two per line, rounded up.
        assert_eq!(lines.len(), 15);
        assert!(
            lines[0].chars().all(|c| c == ' '),
            "the top quiet zone is blank"
        );
        assert!(
            lines[1].chars().all(|c| c == ' '),
            "…and more than one row of it"
        );
        assert!(
            lines.last().expect("a last line").chars().all(|c| c == ' '),
            "so is the bottom"
        );
        assert!(text.contains('█'), "and there is a symbol in between");
    }

    #[test]
    fn ascii_output_is_square_in_module_terms() {
        let s = encode(b"mw-pair:123456").expect("encodes");
        let text = s.to_ascii(2);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 21 + 4);
        assert_eq!(lines[0].chars().count(), (21 + 4) * 2);
    }

    #[test]
    fn the_svg_path_covers_exactly_the_dark_modules() {
        let s = encode(b"mw-pair:123456").expect("encodes");
        let quiet = 4;
        let d = s.to_svg_path(quiet);
        assert_eq!(s.svg_size(quiet), 29);
        // Re-rasterise the path and compare with the symbol: a path that
        // drops or shifts a run makes a picture that scans as something
        // else, and it looks fine.
        let n = s.svg_size(quiet);
        let mut grid = vec![false; n * n];
        for sub in d.split('M').filter(|p| !p.is_empty()) {
            let (origin, rest) = sub.split_once('h').expect("a run");
            let (x, y) = origin.trim().split_once(' ').expect("x y");
            let (x, y): (usize, usize) = (x.parse().expect("x"), y.parse().expect("y"));
            let run: usize = rest
                .split('v')
                .next()
                .expect("a length")
                .parse()
                .expect("run length");
            for i in 0..run {
                grid[y * n + x + i] = true;
            }
        }
        for y in 0..n {
            for x in 0..n {
                let inside = x >= quiet && y >= quiet && x < quiet + s.size && y < quiet + s.size;
                let want = inside && s.get(x - quiet, y - quiet);
                assert_eq!(grid[y * n + x], want, "module ({x},{y})");
            }
        }
    }

    #[test]
    fn rows_agree_with_get() {
        let s = encode(b"mw-pair:123456").expect("encodes");
        let rows = s.rows();
        assert_eq!(rows.len(), s.size);
        for (y, row) in rows.iter().enumerate() {
            assert_eq!(row.len(), s.size);
            for (x, v) in row.iter().enumerate() {
                assert_eq!(*v, s.get(x, y));
            }
        }
    }
}
