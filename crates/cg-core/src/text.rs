//! Position helpers. Columns and string truncation use UTF-16 code units, matching the
//! upstream kernel (design doc 4.2); byte offsets are what tree-sitter and slicing use.

/// Byte offset of the start of every line (`\n` terminated, `\r` stays in the line).
pub fn line_starts(src: &str) -> Vec<u32> {
    let mut v = Vec::with_capacity(src.len() / 32 + 2);
    v.push(0);
    for i in memchr_iter(b'\n', src.as_bytes()) {
        v.push((i + 1) as u32);
    }
    v
}

fn memchr_iter(needle: u8, hay: &[u8]) -> impl Iterator<Item = usize> + '_ {
    hay.iter().enumerate().filter_map(move |(i, b)| (*b == needle).then_some(i))
}

/// UTF-16 column of byte column `byte_col` within `line` (a single line of text).
pub fn col16(line: &str, byte_col: usize) -> u32 {
    let end = byte_col.min(line.len());
    if line.is_ascii() {
        return end as u32;
    }
    let mut n = 0u32;
    for (i, c) in line.char_indices() {
        if i >= end {
            break;
        }
        n += c.len_utf16() as u32;
    }
    n
}

/// Byte column for UTF-16 column `col16` within `line`.
pub fn byte_col(line: &str, col16: u32) -> usize {
    if line.is_ascii() {
        return (col16 as usize).min(line.len());
    }
    let mut n = 0u32;
    for (i, c) in line.char_indices() {
        if n >= col16 {
            return i;
        }
        n += c.len_utf16() as u32;
    }
    line.len()
}

/// Truncate `s` to at most `max_units` UTF-16 code units on a char boundary.
pub fn truncate16(s: &str, max_units: usize) -> &str {
    let mut n = 0usize;
    for (i, c) in s.char_indices() {
        n += c.len_utf16();
        if n > max_units {
            return &s[..i];
        }
    }
    s
}

/// Pack a (line, utf16 column) position so positions compare with `<`.
#[inline]
pub const fn pack_pos(line: u32, col16: u32) -> u64 {
    ((line as u64) << 32) | col16 as u64
}

/// Inverse of [`pack_pos`]: (line, utf16 column).
#[inline]
pub const fn unpack_pos(pos: u64) -> (u32, u32) {
    ((pos >> 32) as u32, pos as u32)
}
