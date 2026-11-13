#!/usr/bin/env python3
"""Decode meshware's QR symbols the way a scanner would, sharing no code
with the encoder that made them.

`crates/mesh-core/src/qr.rs` has its own round-trip test, but that test
imports the encoder's own `mask_at` and `reserved` — so a symbol built with
the row/column arguments swapped would encode and decode consistently and
scan as nothing. This decoder is written from ISO/IEC 18004 independently
and in another language, and it does the two things that catch that class of
bug:

  * it reads the **mask from the symbol's own format bits**, rather than
    being told, so the format information has to be correct;
  * it checks the **Reed-Solomon syndromes are zero**, which is what a real
    scanner's error correction does and what proves the codeword ordering,
    the generator polynomial and the padding are all right.

Usage:
    cargo run -q -p mesh-core --example qr-dump -- 'mw-pair:123456' \
        | python3 scripts/verify-qr.py
"""
import sys

GF_EXP = [0] * 512
GF_LOG = [0] * 256


def _init_gf():
    x = 1
    for i in range(255):
        GF_EXP[i] = x
        GF_LOG[x] = i
        x <<= 1
        if x & 0x100:
            x ^= 0x11D
    for i in range(255, 512):
        GF_EXP[i] = GF_EXP[i - 255]


_init_gf()


def gf_mul(a, b):
    if a == 0 or b == 0:
        return 0
    return GF_EXP[GF_LOG[a] + GF_LOG[b]]


# ISO 18004 Table 25, level M, versions 1-3: (data codewords, ec codewords).
SPECS = {21: (16, 10), 25: (28, 16), 29: (44, 26)}
ALIGN = {21: None, 25: 18, 29: 22}


def mask_fn(mask, row, col):
    """ISO 18004 Table 10. `row` is i, `col` is j."""
    i, j = row, col
    return [
        (i + j) % 2 == 0,
        i % 2 == 0,
        j % 3 == 0,
        (i + j) % 3 == 0,
        (i // 2 + j // 3) % 2 == 0,
        (i * j) % 2 + (i * j) % 3 == 0,
        ((i * j) % 2 + (i * j) % 3) % 2 == 0,
        ((i + j) % 2 + (i * j) % 3) % 2 == 0,
    ][mask]


def function_modules(n):
    """Every module that is not data, worked out from the geometry."""
    f = [[False] * n for _ in range(n)]

    def mark(x, y):
        if 0 <= x < n and 0 <= y < n:
            f[y][x] = True

    # Finders plus separators: an 8x8 region at each of three corners.
    for cx, cy in ((0, 0), (n - 8, 0), (0, n - 8)):
        for dy in range(8):
            for dx in range(8):
                mark(cx + dx, cy + dy)
    # Timing.
    for i in range(n):
        mark(i, 6)
        mark(6, i)
    # Format information (both copies) and the dark module.
    for i in range(9):
        mark(i, 8)
        mark(8, i)
    for i in range(8):
        mark(n - 1 - i, 8)
        mark(8, n - 1 - i)
    # Alignment.
    c = ALIGN[n]
    if c is not None:
        for dy in range(-2, 3):
            for dx in range(-2, 3):
                mark(c + dx, c + dy)
    return f


def read_format(grid, n):
    """The mask index, from the format bits around the top-left finder.

    Checks the BCH is valid rather than trusting the bits: a symbol whose
    format information is corrupt is one a scanner rejects outright.
    """
    bits = 0
    positions = [(8, i) for i in range(6)] + [(8, 7), (8, 8), (7, 8)] + [
        (14 - i, 8) for i in range(9, 15)
    ]
    for idx, (x, y) in enumerate(positions):
        if grid[y][x]:
            bits |= 1 << idx
    bits ^= 0b101_0100_0001_0010
    # Verify the BCH(15,5) remainder is zero.
    rem = bits
    for i in range(14, 9, -1):
        if rem & (1 << i):
            rem ^= 0b101_0011_0111 << (i - 10)
    if rem != 0:
        raise SystemExit(f"format bits fail their BCH check (remainder {rem:#x})")
    level = (bits >> 13) & 0b11
    if level != 0b00:
        raise SystemExit(f"expected error level M (00), symbol says {level:02b}")
    return (bits >> 10) & 0b111


def read_codewords(grid, n, mask):
    """Walk the data modules and return the codeword bytes."""
    fn = function_modules(n)
    bits = []
    col = n - 1
    upward = True
    while col > 0:
        if col == 6:
            col -= 1
        for step in range(n):
            y = n - 1 - step if upward else step
            for dx in (0, 1):
                x = col - dx
                if fn[y][x]:
                    continue
                bit = grid[y][x] ^ mask_fn(mask, y, x)
                bits.append(1 if bit else 0)
        col -= 2
        upward = not upward
    out = bytearray()
    for i in range(0, len(bits) - 7, 8):
        b = 0
        for k in range(8):
            b = (b << 1) | bits[i + k]
        out.append(b)
    return bytes(out)


def syndromes_zero(codewords, ec_len):
    """A real scanner's check: every syndrome of a clean codeword is zero."""
    for i in range(ec_len):
        acc = 0
        for b in codewords:
            acc = gf_mul(acc, GF_EXP[i]) ^ b
        if acc != 0:
            return False
    return True


def decode(grid):
    n = len(grid)
    if n not in SPECS:
        raise SystemExit(f"unexpected symbol size {n}")
    data_len, ec_len = SPECS[n]
    mask = read_format(grid, n)
    words = read_codewords(grid, n, mask)[: data_len + ec_len]
    if not syndromes_zero(words, ec_len):
        raise SystemExit("Reed-Solomon syndromes are non-zero: this symbol is corrupt")
    mode = words[0] >> 4
    if mode != 0b0100:
        raise SystemExit(f"expected byte mode (0100), got {mode:04b}")
    length = ((words[0] & 0x0F) << 4) | (words[1] >> 4)
    payload = bytearray()
    for i in range(length):
        hi = words[1 + i] & 0x0F
        lo = words[2 + i] >> 4
        payload.append((hi << 4) | lo)
    return mask, bytes(payload)


def main():
    text = sys.stdin.read().splitlines()
    checked = 0
    i = 0
    while i < len(text):
        if not text[i].startswith("PAYLOAD "):
            i += 1
            continue
        expected = text[i][len("PAYLOAD "):]
        i += 1
        grid = []
        while text[i] != "END":
            grid.append([c == "1" for c in text[i]])
            i += 1
        mask, got = decode(grid)
        got = got.decode("utf-8", "replace")
        status = "ok" if got == expected else "MISMATCH"
        print(f"{status:9} {len(grid)}x{len(grid)} mask={mask} {got!r}")
        if got != expected:
            raise SystemExit(f"decoded {got!r}, encoder was asked for {expected!r}")
        checked += 1
    if checked == 0:
        raise SystemExit("no symbols on stdin")
    print(f"\n{checked} symbol(s) decoded cleanly, syndromes zero.")


if __name__ == "__main__":
    main()
