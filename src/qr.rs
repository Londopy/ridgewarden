//! QR codes (ISO/IEC 18004:2015) for invites: byte mode, error correction level M, or L
//! when M won't fit, versions 1 to 40, and the mask picked by the standard's penalty rules.

use std::fmt::Write as _;

use anyhow::{Result, bail};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    L,
    M,
}

impl Level {
    fn row(self) -> usize {
        match self {
            Level::L => 0,
            Level::M => 1,
        }
    }

    /// The two level bits in the format information (table 12).
    fn format_bits(self) -> u32 {
        match self {
            Level::L => 1,
            Level::M => 0,
        }
    }
}

/// Error correction codewords per block, by level and version (table 9; index 0 unused).
const ECC_PER_BLOCK: [[u8; 41]; 2] = [
    [
        0, 7, 10, 15, 20, 26, 18, 20, 24, 30, 18, 20, 24, 26, 30, 22, 24, 28, 30, 28, 28, 28, 28,
        30, 30, 26, 28, 30, 30, 30, 30, 30, 30, 30, 30, 30, 30, 30, 30, 30, 30,
    ],
    [
        0, 10, 16, 26, 18, 24, 16, 18, 22, 22, 26, 30, 22, 22, 24, 24, 28, 28, 26, 26, 26, 26, 28,
        28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28,
    ],
];

/// Error correction blocks, by level and version (table 9).
const BLOCKS: [[u8; 41]; 2] = [
    [
        0, 1, 1, 1, 1, 1, 2, 2, 2, 2, 4, 4, 4, 4, 4, 6, 6, 6, 6, 7, 8, 8, 9, 9, 10, 12, 12, 12, 13,
        14, 15, 16, 17, 18, 19, 19, 20, 21, 22, 24, 25,
    ],
    [
        0, 1, 1, 1, 2, 2, 4, 4, 4, 5, 5, 5, 8, 9, 9, 10, 10, 11, 13, 14, 16, 17, 17, 18, 20, 21,
        23, 25, 26, 28, 29, 31, 33, 35, 37, 38, 40, 43, 45, 47, 49,
    ],
];

/// Modules around the symbol that must stay light so a reader can find it.
const QUIET: usize = 4;

pub struct Qr {
    pub version: u8,
    pub level: Level,
    pub mask: u8,
    /// modules per side
    pub size: usize,
    dark: Vec<bool>,
}

/// Encode `data` in the smallest symbol that holds it, at level M if any version can.
pub fn encode(data: &[u8]) -> Result<Qr> {
    for level in [Level::M, Level::L] {
        if let Some(v) = (1..=40).find(|&v| fits(data.len(), v, level)) {
            return encode_with(data, v, level, None);
        }
    }
    bail!(
        "{} bytes is too long for a QR code (2953 at most)",
        data.len()
    )
}

/// Encode at a given version and level; with `mask` None, the best-scoring mask is used.
pub fn encode_with(data: &[u8], version: u8, level: Level, mask: Option<u8>) -> Result<Qr> {
    if !(1..=40).contains(&version) || mask.is_some_and(|m| m > 7) {
        bail!("no QR version {version} or mask {mask:?}");
    }
    if !fits(data.len(), version, level) {
        bail!("{} bytes do not fit in QR version {version}", data.len());
    }
    let codewords = interleave(&pad(data, version, level), version, level);
    let mut g = Grid::new(version);
    g.place(&codewords);
    let mask = mask.unwrap_or_else(|| {
        (0..8)
            .min_by_key(|&m| {
                let mut t = g.clone();
                t.apply_mask(m);
                t.penalty()
            })
            .unwrap_or(0)
    });
    g.apply_mask(mask);
    // format and version areas stay light while masks are scored (7.8.3), then get drawn
    g.draw_format(format_bits(level, mask));
    g.draw_version(version);
    let s = g.size;
    g.set(8, s - 8, true);
    Ok(Qr {
        version,
        level,
        mask,
        size: g.size,
        dark: g.dark,
    })
}

impl Qr {
    pub fn get(&self, x: usize, y: usize) -> bool {
        self.dark[y * self.size + x]
    }

    fn get_quiet(&self, x: usize, y: usize) -> bool {
        let inside = |c: usize| (QUIET..QUIET + self.size).contains(&c);
        inside(x) && inside(y) && self.get(x - QUIET, y - QUIET)
    }

    /// Two modules per character cell with half blocks, forced black on white so it scans
    /// whatever colors the terminal uses.
    pub fn to_terminal(&self) -> String {
        let side = self.size + 2 * QUIET;
        let mut out = String::new();
        for y in (0..side).step_by(2) {
            out.push_str("\x1b[30;107m");
            for x in 0..side {
                let top = self.get_quiet(x, y);
                let bottom = y + 1 < side && self.get_quiet(x, y + 1);
                out.push(match (top, bottom) {
                    (true, true) => '\u{2588}',
                    (true, false) => '\u{2580}',
                    (false, true) => '\u{2584}',
                    (false, false) => ' ',
                });
            }
            out.push_str("\x1b[0m\n");
        }
        out
    }

    /// A scalable SVG, dark modules on a white square, one path per row of runs.
    pub fn to_svg(&self, label: &str) -> String {
        let side = self.size + 2 * QUIET;
        let mut path = String::new();
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
                let _ = write!(
                    path,
                    "M{} {}h{}v1h-{}z",
                    start + QUIET,
                    y + QUIET,
                    x - start,
                    x - start
                );
            }
        }
        format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {side} {side}\" role=\"img\" \
             shape-rendering=\"crispEdges\" class=qr><title>{}</title>\
             <rect width=\"{side}\" height=\"{side}\" fill=\"#fff\"/><path fill=\"#000\" d=\"{path}\"/></svg>",
            crate::util::esc(label)
        )
    }
}

fn char_count_bits(version: u8) -> usize {
    if version < 10 { 8 } else { 16 }
}

fn fits(len: usize, version: u8, level: Level) -> bool {
    4 + char_count_bits(version) + 8 * len <= 8 * data_codewords(version, level)
}

/// Modules left for data and error correction once the function patterns are drawn.
fn raw_modules(version: u8) -> usize {
    let v = version as usize;
    let mut n = (16 * v + 128) * v + 64;
    if v >= 2 {
        let align = v / 7 + 2;
        n -= (25 * align - 10) * align - 55;
        if v >= 7 {
            n -= 36;
        }
    }
    n
}

fn data_codewords(version: u8, level: Level) -> usize {
    let v = version as usize;
    let l = level.row();
    raw_modules(version) / 8 - ECC_PER_BLOCK[l][v] as usize * BLOCKS[l][v] as usize
}

/// Mode indicator, count, the bytes, terminator and pad codewords (7.4).
fn pad(data: &[u8], version: u8, level: Level) -> Vec<u8> {
    let cap = 8 * data_codewords(version, level);
    let mut bits: Vec<bool> = Vec::with_capacity(cap);
    let mut push = |value: usize, n: usize| {
        for i in (0..n).rev() {
            bits.push((value >> i) & 1 == 1);
        }
    };
    push(0b0100, 4);
    push(data.len(), char_count_bits(version));
    for &b in data {
        push(b as usize, 8);
    }
    let terminator = (cap - bits.len()).min(4);
    bits.extend(std::iter::repeat_n(false, terminator));
    while !bits.len().is_multiple_of(8) {
        bits.push(false);
    }
    let mut out: Vec<u8> = bits
        .chunks(8)
        .map(|c| c.iter().fold(0u8, |acc, &b| acc << 1 | b as u8))
        .collect();
    for pad in [0xEC, 0x11].into_iter().cycle() {
        if out.len() >= cap / 8 {
            break;
        }
        out.push(pad);
    }
    out
}

/// Split into blocks, add each block's error correction, and interleave (7.5, 7.6).
fn interleave(data: &[u8], version: u8, level: Level) -> Vec<u8> {
    let (v, l) = (version as usize, level.row());
    let blocks = BLOCKS[l][v] as usize;
    let ecc_len = ECC_PER_BLOCK[l][v] as usize;
    let total = raw_modules(version) / 8;
    let short = blocks - total % blocks;
    let short_data = total / blocks - ecc_len;
    let divisor = rs_divisor(ecc_len);
    let mut split = vec![];
    let mut at = 0;
    for i in 0..blocks {
        let len = short_data + usize::from(i >= short);
        let d = &data[at..at + len];
        at += len;
        split.push((d, rs_remainder(d, &divisor)));
    }
    let mut out = Vec::with_capacity(total);
    for i in 0..=short_data {
        out.extend(split.iter().filter_map(|(d, _)| d.get(i)));
    }
    for i in 0..ecc_len {
        out.extend(split.iter().map(|(_, e)| e[i]));
    }
    out
}

/// Multiply in GF(256) with the QR field polynomial x^8 + x^4 + x^3 + x^2 + 1.
fn gf_mul(mut a: u8, mut b: u8) -> u8 {
    let mut r = 0;
    while b != 0 {
        if b & 1 == 1 {
            r ^= a;
        }
        let carry = a & 0x80 != 0;
        a <<= 1;
        if carry {
            a ^= 0x1D;
        }
        b >>= 1;
    }
    r
}

/// The generator polynomial (x - a^0)(x - a^1)...(x - a^(n-1)), highest term dropped.
fn rs_divisor(degree: usize) -> Vec<u8> {
    let mut g = vec![0u8; degree];
    g[degree - 1] = 1;
    let mut root = 1u8;
    for _ in 0..degree {
        for j in 0..degree {
            g[j] = gf_mul(g[j], root);
            if j + 1 < degree {
                g[j] ^= g[j + 1];
            }
        }
        root = gf_mul(root, 2);
    }
    g
}

fn rs_remainder(data: &[u8], divisor: &[u8]) -> Vec<u8> {
    let mut r = vec![0u8; divisor.len()];
    for &b in data {
        let factor = b ^ r[0];
        r.rotate_left(1);
        let last = r.len() - 1;
        r[last] = 0;
        for (x, &g) in r.iter_mut().zip(divisor) {
            *x ^= gf_mul(g, factor);
        }
    }
    r
}

/// 15 format bits: level and mask, BCH(15,5) protected, XOR 101010000010010 (7.9.1).
fn format_bits(level: Level, mask: u8) -> u32 {
    let data = level.format_bits() << 3 | mask as u32;
    let mut rem = data;
    for _ in 0..10 {
        rem = (rem << 1) ^ ((rem >> 9) * 0x537);
    }
    (data << 10 | rem) ^ 0x5412
}

/// 18 version bits, BCH(18,6) protected (7.10).
fn version_bits(version: u8) -> u32 {
    let v = version as u32;
    let mut rem = v;
    for _ in 0..12 {
        rem = (rem << 1) ^ ((rem >> 11) * 0x1F25);
    }
    v << 12 | rem
}

fn alignment_centres(version: u8) -> Vec<usize> {
    if version == 1 {
        return vec![];
    }
    let v = version as usize;
    let n = v / 7 + 2;
    let size = 4 * v + 17;
    let step = if v == 32 {
        26
    } else {
        (v * 4 + n * 2 + 1) / (n * 2 - 2) * 2
    };
    let mut c: Vec<usize> = (0..n - 1).map(|i| size - 7 - i * step).collect();
    c.push(6);
    c.reverse();
    c
}

fn masked(mask: u8, x: usize, y: usize) -> bool {
    match mask {
        0 => (x + y).is_multiple_of(2),
        1 => y.is_multiple_of(2),
        2 => x.is_multiple_of(3),
        3 => (x + y).is_multiple_of(3),
        4 => (x / 3 + y / 2).is_multiple_of(2),
        5 => x * y % 2 + x * y % 3 == 0,
        6 => (x * y % 2 + x * y % 3).is_multiple_of(2),
        _ => ((x + y) % 2 + x * y % 3).is_multiple_of(2),
    }
}

#[derive(Clone)]
struct Grid {
    size: usize,
    dark: Vec<bool>,
    function: Vec<bool>,
}

impl Grid {
    /// A symbol with its function patterns drawn and the format and version areas
    /// reserved (light).
    fn new(version: u8) -> Self {
        let size = 4 * version as usize + 17;
        let mut g = Grid {
            size,
            dark: vec![false; size * size],
            function: vec![false; size * size],
        };
        for i in 0..size {
            g.set(6, i, i % 2 == 0);
            g.set(i, 6, i % 2 == 0);
        }
        for (x, y) in [(3, 3), (size - 4, 3), (3, size - 4)] {
            g.finder(x, y);
        }
        let c = alignment_centres(version);
        let last = c.len().saturating_sub(1);
        for (i, &x) in c.iter().enumerate() {
            for (j, &y) in c.iter().enumerate() {
                if !matches!((i, j), (0, 0)) && !(i == 0 && j == last) && !(i == last && j == 0) {
                    g.alignment(x, y);
                }
            }
        }
        g.draw_format(0);
        if version >= 7 {
            g.draw_version_bits(0);
        }
        g.set(8, size - 8, false);
        g
    }

    fn set(&mut self, x: usize, y: usize, dark: bool) {
        let i = y * self.size + x;
        self.dark[i] = dark;
        self.function[i] = true;
    }

    /// Finder pattern plus its light separator, clipped at the edges.
    fn finder(&mut self, cx: usize, cy: usize) {
        for dy in -4i32..=4 {
            for dx in -4i32..=4 {
                let (x, y) = (cx as i32 + dx, cy as i32 + dy);
                if (0..self.size as i32).contains(&x) && (0..self.size as i32).contains(&y) {
                    let d = dx.abs().max(dy.abs());
                    self.set(x as usize, y as usize, d != 2 && d != 4);
                }
            }
        }
    }

    fn alignment(&mut self, cx: usize, cy: usize) {
        for dy in -2i32..=2 {
            for dx in -2i32..=2 {
                let d = dx.abs().max(dy.abs());
                self.set((cx as i32 + dx) as usize, (cy as i32 + dy) as usize, d != 1);
            }
        }
    }

    fn draw_format(&mut self, bits: u32) {
        let s = self.size;
        let bit = |i: u32| (bits >> i) & 1 == 1;
        for i in 0..6 {
            self.set(8, i as usize, bit(i));
        }
        self.set(8, 7, bit(6));
        self.set(8, 8, bit(7));
        self.set(7, 8, bit(8));
        for i in 9..15 {
            self.set(14 - i as usize, 8, bit(i));
        }
        for i in 0..8 {
            self.set(s - 1 - i as usize, 8, bit(i));
        }
        for i in 8..15 {
            self.set(8, s - 15 + i as usize, bit(i));
        }
    }

    fn draw_version(&mut self, version: u8) {
        if version >= 7 {
            self.draw_version_bits(version_bits(version));
        }
    }

    fn draw_version_bits(&mut self, bits: u32) {
        for i in 0..18 {
            let dark = (bits >> i) & 1 == 1;
            let (a, b) = (self.size - 11 + i % 3, i / 3);
            self.set(a, b, dark);
            self.set(b, a, dark);
        }
    }

    /// Codeword bits in two-module columns, right to left, alternately up and down (7.7.3).
    fn place(&mut self, codewords: &[u8]) {
        let s = self.size;
        let total = codewords.len() * 8;
        let mut i = 0;
        let mut right = s - 1;
        loop {
            if right == 6 {
                right = 5;
            }
            let upward = (right + 1) & 2 == 0;
            for vert in 0..s {
                let y = if upward { s - 1 - vert } else { vert };
                for x in [right, right - 1] {
                    if !self.function[y * s + x] && i < total {
                        self.dark[y * s + x] = (codewords[i / 8] >> (7 - i % 8)) & 1 == 1;
                        i += 1;
                    }
                }
            }
            if right < 2 {
                break;
            }
            right -= 2;
        }
    }

    fn apply_mask(&mut self, mask: u8) {
        let s = self.size;
        for y in 0..s {
            for x in 0..s {
                if !self.function[y * s + x] && masked(mask, x, y) {
                    self.dark[y * s + x] ^= true;
                }
            }
        }
    }

    /// Penalty score of 7.8.3.1: runs, 2x2 blocks, finder-like patterns, dark balance.
    fn penalty(&self) -> usize {
        let s = self.size;
        let at = |x: usize, y: usize| self.dark[y * s + x];
        let mut score = 0;
        for i in 0..s {
            let row: Vec<bool> = (0..s).map(|x| at(x, i)).collect();
            let col: Vec<bool> = (0..s).map(|y| at(i, y)).collect();
            score += runs_penalty(&row) + runs_penalty(&col);
            score += finder_penalty(&row) + finder_penalty(&col);
        }
        for y in 1..s {
            for x in 1..s {
                let c = at(x, y);
                if at(x - 1, y) == c && at(x, y - 1) == c && at(x - 1, y - 1) == c {
                    score += 3;
                }
            }
        }
        let dark = self.dark.iter().filter(|&&d| d).count();
        let total = s * s;
        score + 10 * ((20 * dark).abs_diff(10 * total) / total)
    }
}

/// 3 for a run of five same-colored modules, plus 1 for each module past five.
fn runs_penalty(line: &[bool]) -> usize {
    let mut score = 0;
    let mut run = 1;
    for i in 1..=line.len() {
        if i < line.len() && line[i] == line[i - 1] {
            run += 1;
            continue;
        }
        if run >= 5 {
            score += run - 2;
        }
        run = 1;
    }
    score
}

/// 40 for each dark-light-dark-dark-dark-light-dark with four light modules (or the
/// symbol's edge) on one side.
fn finder_penalty(line: &[bool]) -> usize {
    const PATTERN: [bool; 7] = [true, false, true, true, true, false, true];
    let n = line.len();
    let light = |r: std::ops::Range<usize>| line[r].iter().all(|&d| !d);
    let mut score = 0;
    let mut i = 0;
    while i + 7 <= n {
        if line[i..i + 7] != PATTERN {
            i += 1;
            continue;
        }
        if i == 0 || i == n - 7 || light(i.saturating_sub(4)..i) || light(i + 7..(i + 11).min(n)) {
            score += 40;
            i += 7;
        } else {
            i += 4;
        }
    }
    score
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FNV-1a over the symbol as '0'/'1' rows, top to bottom; the vectors below were made
    /// the same way from segno 1.6.6 by scripts/qr_vectors.py.
    fn fingerprint(q: &Qr) -> u64 {
        let mut h: u64 = 0xcbf29ce484222325;
        for y in 0..q.size {
            for x in 0..q.size {
                h ^= if q.get(x, y) { b'1' } else { b'0' } as u64;
                h = h.wrapping_mul(0x100000001b3);
            }
        }
        h
    }

    #[test]
    fn reed_solomon_matches_the_standards_worked_example() {
        // ISO/IEC 18004 annex I: "01234567" as version 1-M
        let data = [
            0x10, 0x20, 0x0C, 0x56, 0x61, 0x80, 0xEC, 0x11, 0xEC, 0x11, 0xEC, 0x11, 0xEC, 0x11,
            0xEC, 0x11,
        ];
        assert_eq!(
            rs_remainder(&data, &rs_divisor(10)),
            [0xA5, 0x24, 0xD4, 0xC1, 0xED, 0x36, 0xC7, 0x87, 0x2C, 0x55]
        );
    }

    #[test]
    fn format_and_version_bits_match_the_standard_tables() {
        // annex C: level M mask 0, and level L mask 0 and 7
        assert_eq!(format_bits(Level::M, 0), 0b101010000010010);
        assert_eq!(format_bits(Level::L, 0), 0b111011111000100);
        assert_eq!(format_bits(Level::L, 7), 0b110100101110110);
        // annex D: versions 7 and 40
        assert_eq!(version_bits(7), 0b000111110010010100);
        assert_eq!(version_bits(40), 0b101000110001101001);
    }

    #[test]
    fn capacities_match_table_7() {
        let byte_cap = |v, l| (1..3000).take_while(|&n| fits(n, v, l)).last().unwrap();
        assert_eq!(byte_cap(1, Level::M), 14);
        assert_eq!(byte_cap(1, Level::L), 17);
        assert_eq!(byte_cap(10, Level::M), 213);
        assert_eq!(byte_cap(40, Level::M), 2331);
        assert_eq!(byte_cap(40, Level::L), 2953);
        assert!(encode(&[b'a'; 2953]).is_ok());
        assert!(encode(&[b'a'; 2954]).is_err());
    }

    #[test]
    fn every_version_matches_segno_with_a_fixed_mask() {
        for (level, want) in [(Level::L, FIXED_L), (Level::M, FIXED_M)] {
            for v in 1..=40u8 {
                let q = encode_with(b"ridgewarden", v, level, Some(v % 8)).unwrap();
                assert_eq!(
                    fingerprint(&q),
                    want[v as usize - 1],
                    "{level:?} version {v}"
                );
                assert_eq!(decode(&q), b"ridgewarden");
            }
        }
    }

    /// Same text generator as the script that made `AUTO`.
    fn text(n: usize) -> Vec<u8> {
        const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        (0..n)
            .map(|i| A[(i * 37 + n * 11 + (i * i) % 29) % 64])
            .collect()
    }

    #[test]
    fn version_level_and_mask_choice_match_segno() {
        for (n, version, level, mask, want) in AUTO {
            let q = encode(&text(n)).unwrap();
            assert_eq!(
                (q.version, q.level, q.mask),
                (version, level, mask),
                "{n} bytes"
            );
            assert_eq!(fingerprint(&q), want, "{n} bytes");
            assert_eq!(decode(&q), text(n));
        }
    }

    #[test]
    fn full_symbols_decode_at_every_version() {
        for level in [Level::L, Level::M] {
            for v in 1..=40u8 {
                let cap = (1..3000).take_while(|&n| fits(n, v, level)).last().unwrap();
                let data: Vec<u8> = (0..cap).map(|i| (i * 131 + v as usize) as u8).collect();
                let q = encode_with(&data, v, level, None).unwrap();
                assert_eq!(decode(&q), data, "{level:?} version {v}");
            }
        }
    }

    #[test]
    fn invite_sized_text_renders() {
        let q = encode(&text(600)).unwrap();
        let t = q.to_terminal();
        let side = q.size + 2 * QUIET;
        assert_eq!(t.lines().count(), side.div_ceil(2));
        let svg = q.to_svg("a <b>");
        assert!(svg.starts_with("<svg") && svg.contains("a &lt;b&gt;"));
        assert!(svg.contains(&format!("viewBox=\"0 0 {side} {side}\"")));
    }

    /// Remainder of `v` divided by the GF(2) polynomial `g`.
    fn gf2_rem(mut v: u32, g: u32) -> u32 {
        let deg = 31 - g.leading_zeros();
        while v != 0 && 31 - v.leading_zeros() >= deg {
            v ^= g << (31 - v.leading_zeros() - deg);
        }
        v
    }

    /// Read a symbol back from its modules: format information, unmasking, codeword
    /// order, block syndromes and the byte segment. Only which modules are function
    /// patterns, the mask formulas and the block table come from the encoder.
    fn decode(q: &Qr) -> Vec<u8> {
        let s = q.size;
        let version = ((s - 17) / 4) as u8;
        let mut near = [(0usize, 0usize); 15];
        let mut far = [(0usize, 0usize); 15];
        for i in 0..15 {
            near[i] = match i {
                0..=5 => (8, i),
                6 => (8, 7),
                7 => (8, 8),
                8 => (7, 8),
                _ => (14 - i, 8),
            };
            far[i] = if i < 8 {
                (s - 1 - i, 8)
            } else {
                (8, s - 15 + i)
            };
        }
        let read = |pos: &[(usize, usize); 15]| {
            (0..15).fold(0u32, |acc, i| acc | (q.get(pos[i].0, pos[i].1) as u32) << i)
        };
        let fmt = read(&near);
        assert_eq!(fmt, read(&far), "both format copies agree");
        let fmt = fmt ^ 0x5412;
        assert_eq!(gf2_rem(fmt, 0x537), 0, "format BCH checks");
        let level = match fmt >> 13 {
            1 => Level::L,
            0 => Level::M,
            l => panic!("level bits {l}"),
        };
        let mask = (fmt >> 10 & 7) as u8;
        assert!(q.get(8, s - 8), "dark module");
        if version >= 7 {
            let mut bits = 0u32;
            for i in 0..18 {
                bits |= (q.get(s - 11 + i % 3, i / 3) as u32) << i;
                assert_eq!(q.get(s - 11 + i % 3, i / 3), q.get(i / 3, s - 11 + i % 3));
            }
            assert_eq!(bits >> 12, version as u32);
            assert_eq!(gf2_rem(bits, 0x1F25), 0, "version BCH checks");
        }

        let function = Grid::new(version).function;
        let mut bits = vec![];
        let mut col = s as isize - 1;
        let mut up = true;
        while col > 0 {
            if col == 6 {
                col -= 1;
            }
            let ys: Vec<usize> = if up {
                (0..s).rev().collect()
            } else {
                (0..s).collect()
            };
            for y in ys {
                for x in [col as usize, col as usize - 1] {
                    if !function[y * s + x] {
                        bits.push(q.get(x, y) ^ masked(mask, x, y));
                    }
                }
            }
            up = !up;
            col -= 2;
        }
        let codewords: Vec<u8> = bits
            .as_chunks::<8>()
            .0
            .iter()
            .map(|c| c.iter().fold(0u8, |a, &b| a << 1 | b as u8))
            .collect();

        let (v, l) = (version as usize, level.row());
        let blocks = BLOCKS[l][v] as usize;
        let ecc = ECC_PER_BLOCK[l][v] as usize;
        let total = codewords.len();
        let short = blocks - total % blocks;
        let lens: Vec<usize> = (0..blocks)
            .map(|i| total / blocks - ecc + usize::from(i >= short))
            .collect();
        let mut split: Vec<Vec<u8>> = vec![vec![]; blocks];
        let mut it = codewords.iter();
        for i in 0..=lens[blocks - 1] {
            for (b, len) in lens.iter().enumerate() {
                if i < *len {
                    split[b].push(*it.next().unwrap());
                }
            }
        }
        for _ in 0..ecc {
            for b in split.iter_mut() {
                b.push(*it.next().unwrap());
            }
        }
        let mut data: Vec<u8> = vec![];
        for (b, len) in split.iter().zip(&lens) {
            let mut alpha = 1u8;
            for _ in 0..ecc {
                let syndrome = b.iter().fold(0u8, |acc, &c| gf_mul(acc, alpha) ^ c);
                assert_eq!(syndrome, 0, "block syndromes are zero");
                alpha = gf_mul(alpha, 2);
            }
            data.extend(&b[..*len]);
        }

        let bit = |i: usize| -> u8 { (data[i / 8] >> (7 - i % 8)) & 1 };
        let take =
            |from: usize, n: usize| (from..from + n).fold(0usize, |a, i| a << 1 | bit(i) as usize);
        assert_eq!(take(0, 4), 0b0100, "byte mode");
        let count_bits = if version < 10 { 8 } else { 16 };
        let n = take(4, count_bits);
        (0..n)
            .map(|i| take(4 + count_bits + 8 * i, 8) as u8)
            .collect()
    }

    const FIXED_L: [u64; 40] = [
        0x867d88d3eff54427,
        0x2d6d4bec539025b9,
        0xc257b7f3bf9cbf23,
        0xc8a568c8dfdc3a0f,
        0x607b15dd58ad5a9c,
        0x2f4f3a1d3339bb13,
        0xaf53724ce156b16b,
        0x2b21ca25a871d9a3,
        0xaf92f62e82ca45f4,
        0xba6b0ed184e59353,
        0x86f3984210b83231,
        0xed3ef81be5503b49,
        0x26a69cb11d4acca1,
        0xcb023204dd9d4643,
        0xeb12c7fc25762b56,
        0xbeb542d9df5c01ef,
        0xdddbc38df34b4bc6,
        0x688b566a58cf3b15,
        0x79b2491946980873,
        0x1890526acd810d7f,
        0xad46d3878f8b4a64,
        0xaf8ad42ea0bf68a5,
        0x282986c9edfd31a3,
        0x8a5441aa647de507,
        0xf59570ee1284f29e,
        0x7c137b42620ade8d,
        0xa7d2d2a8f0a02e4b,
        0xa89e9950e0a16558,
        0xb39c2993c1f3e088,
        0x1ace499d3a12e44e,
        0x3e31a3fb99e93ed6,
        0x9d2c1a3ece225a01,
        0x43989182b72dd168,
        0x53d072d90bbb631f,
        0x443a0f8d051f321f,
        0x96fb2c58e1739315,
        0xb1717ba9064bffcd,
        0xc2af28a40b343d69,
        0xc6a393ea1969e926,
        0x7d25d6cd318d0027,
    ];
    const FIXED_M: [u64; 40] = [
        0x0b92d647e7b80771,
        0xd79c7eaaf437280f,
        0x6f9aea3d270445ab,
        0xf8ebdfb63f80e03b,
        0xd8b1a8096dab63e2,
        0x589811fd3b68aa0f,
        0xae04ed8f43f81215,
        0x8715fe4aab60bcc7,
        0xee28587c616ea240,
        0xe9e83f903bbb08eb,
        0x3cab0ccd031d696b,
        0x71366ec510cbfc03,
        0x5bf6c7e4cc2f272f,
        0x5a0bbd1e6b95025b,
        0x701e520b8ecd84d6,
        0xbacbb97174166eb7,
        0x3c6eb7efd54e27e6,
        0xd61aa81cda1f7d9f,
        0x1c995986ca2f5249,
        0xe388c35fce6e886f,
        0x4fe304f18bf25874,
        0x938bf8dc4be53d6b,
        0x70e01962e4004999,
        0xbb775b759d6fe675,
        0x9d21cbf54319e94c,
        0xc0612ecde3d75647,
        0xb97fc7d7ed498729,
        0xe9d57ded290b2b72,
        0xa8b1324c664364d8,
        0x23ae165ffb773a58,
        0x9f00767cd3315350,
        0x5fe7af72b610e737,
        0x6ba6ef74b8e28a70,
        0xa903fbb9c936ef63,
        0x38439438034ce401,
        0xa607bad08d3d592f,
        0xe3640208df11d08f,
        0x770553a0840e418d,
        0x9dc3cc32677c49be,
        0xa81718319181e2f7,
    ];
    const AUTO: [(usize, u8, Level, u8, u64); 12] = [
        (1, 1, Level::M, 0, 0x1e1845c8c89ae567),
        (14, 1, Level::M, 7, 0x2eb99d3cd515bddb),
        (15, 2, Level::M, 4, 0xc0f11b54d6e87edd),
        (40, 3, Level::M, 3, 0xee7a3e86a24f411b),
        (100, 6, Level::M, 4, 0xe72b9395ae78a42e),
        (300, 13, Level::M, 1, 0xf8c658f8a0413954),
        (700, 21, Level::M, 4, 0xe00d3f9f1c4eef4f),
        (1200, 29, Level::M, 2, 0xcff4f7b694085cb1),
        (1800, 35, Level::M, 7, 0xf3f282bcb14b9b3d),
        (2331, 40, Level::M, 2, 0x8136590355d5b29d),
        (2332, 36, Level::L, 2, 0xe64c145ef76b8375),
        (2900, 40, Level::L, 2, 0x0652a2128533697b),
    ];
}
