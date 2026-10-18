//! Source-file decoding.
//!
//! Most repositories are UTF-8, but legacy trees (GBK / Shift-JIS / Latin-1
//! comments and string literals) are common. `read_to_string` rejects them
//! outright, which silently drops the whole file from the index. Here
//! UTF-8 stays the fast path; anything else is BOM-sniffed or detected with
//! `chardetng` and decoded (malformed sequences become U+FFFD).

use std::io;
use std::path::Path;

/// Legacy classic-Mac files use a lone `\r` as the line terminator. Rust,
/// tree-sitter and ripgrep only split on `\n`, so such a file collapses into a
/// single line. When the text has `\r` but no `\n` at all, rewrite every `\r`
/// to `\n` (same byte length, so offsets are unchanged). `\r\n` and mixed
/// files are left untouched.
pub fn normalize_lone_cr(text: String) -> String {
    if text.contains('\r') && !text.contains('\n') {
        text.replace('\r', "\n")
    } else {
        text
    }
}

/// Byte-level twin of [`normalize_lone_cr`], in place.
pub fn normalize_lone_cr_bytes(bytes: &mut [u8]) {
    if bytes.contains(&b'\r') && !bytes.contains(&b'\n') {
        for b in bytes.iter_mut().filter(|b| **b == b'\r') {
            *b = b'\n';
        }
    }
}

/// Decode raw file bytes to text. Never fails. Lone-`\r` line endings are
/// normalized to `\n` (see [`normalize_lone_cr`]).
pub fn decode_source(bytes: Vec<u8>) -> String {
    normalize_lone_cr(decode_source_raw(bytes))
}

fn decode_source_raw(bytes: Vec<u8>) -> String {
    // UTF-16 text is often valid UTF-8 too (ASCII + NUL), so check its BOM first.
    if bytes.starts_with(&[0xFF, 0xFE]) || bytes.starts_with(&[0xFE, 0xFF]) {
        if let Some((encoding, bom_len)) = encoding_rs::Encoding::for_bom(&bytes) {
            let (text, _) = encoding.decode_without_bom_handling(&bytes[bom_len..]);
            return text.into_owned();
        }
    }
    let bytes = match String::from_utf8(bytes) {
        Ok(mut text) => {
            if text.starts_with('\u{feff}') {
                text.drain(..'\u{feff}'.len_utf8());
            }
            return text;
        }
        Err(err) => err.into_bytes(),
    };
    if let Some((encoding, bom_len)) = encoding_rs::Encoding::for_bom(&bytes) {
        let (text, _) = encoding.decode_without_bom_handling(&bytes[bom_len..]);
        return text.into_owned();
    }
    let mut detector = chardetng::EncodingDetector::new(chardetng::Iso2022JpDetection::Deny);
    detector.feed(&bytes, true);
    let mut encoding = detector.guess(None, chardetng::Utf8Detection::Deny);
    // chardetng is unreliable on short samples: a few GBK ideographs in an
    // ASCII file are typically guessed as EUC-KR (or Big5/SJIS). When the
    // guess is one of those and the bytes decode cleanly as GB18030 into
    // *only* CJK ideographs / CJK punctuation (no Hangul, no kana), prefer GB18030.
    if matches!(encoding.name(), "EUC-KR" | "Big5" | "Shift_JIS" | "EUC-JP")
        && gb18030_ideographs_only(&bytes)
    {
        encoding = encoding_rs::GB18030;
    }
    let (text, _, _) = encoding.decode(&bytes);
    text.into_owned()
}

fn gb18030_ideographs_only(bytes: &[u8]) -> bool {
    let Some(text) =
        encoding_rs::GB18030.decode_without_bom_handling_and_without_replacement(bytes)
    else {
        return false;
    };
    let mut any = false;
    for c in text.chars().filter(|c| !c.is_ascii()) {
        let ok = matches!(c as u32,
            0x4E00..=0x9FFF | 0x3400..=0x4DBF | 0x3000..=0x303F | 0xFF00..=0xFFEF
            | 0x2018..=0x201D | 0x2026 | 0x00B7 | 0x2014);
        if !ok {
            return false;
        }
        any = true;
    }
    any
}

/// Read a source file, decoding non-UTF-8 content instead of failing.
pub fn read_source(path: &Path) -> io::Result<String> {
    std::fs::read(path).map(decode_source)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_passthrough_and_bom_strip() {
        assert_eq!(decode_source(b"fn a() {}".to_vec()), "fn a() {}");
        assert_eq!(decode_source(b"\xEF\xBB\xBFx".to_vec()), "x");
    }

    #[test]
    fn gbk_is_decoded() {
        // "// 中文注释\nint main_func() { return 0; }\n" in GBK.
        let (bytes, _, _) =
            encoding_rs::GBK.encode("// 中文注释，这是一个测试\nint main_func() { return 0; }\n");
        let text = decode_source(bytes.into_owned());
        assert!(text.contains("main_func"));
        assert!(text.contains("中文注释"));
    }

    #[test]
    fn utf16_bom_is_decoded_even_when_ascii_only() {
        let mut le = vec![0xFF, 0xFE];
        for u in "a\nb\n".encode_utf16() {
            le.extend_from_slice(&u.to_le_bytes());
        }
        assert_eq!(decode_source(le), "a\nb\n");
    }

    #[test]
    fn short_gbk_not_misdetected_as_euc_kr() {
        for src in ["// 你好 SysLog(gbkgo)\n", "x = 1 // 中文\n", "// 你好\n"] {
            let (bytes, _, _) = encoding_rs::GBK.encode(src);
            assert_eq!(decode_source(bytes.into_owned()), src);
        }
    }

    #[test]
    fn valid_utf8_never_transcoded() {
        let s = "// 你好 SysLog\n";
        assert_eq!(decode_source(s.as_bytes().to_vec()), s);
    }

    #[test]
    fn lone_cr_is_normalized_but_crlf_and_mixed_are_not() {
        assert_eq!(decode_source(b"a\rb\rc\r".to_vec()), "a\nb\nc\n");
        assert_eq!(decode_source(b"a\r\nb\r\n".to_vec()), "a\r\nb\r\n");
        assert_eq!(decode_source(b"a\rb\nc".to_vec()), "a\rb\nc");
        let mut le = vec![0xFF, 0xFE];
        for u in "a\rb\r".encode_utf16() {
            le.extend_from_slice(&u.to_le_bytes());
        }
        assert_eq!(decode_source(le), "a\nb\n");
    }

    #[test]
    fn lone_cr_python_symbols_get_real_lines() {
        let dir = std::env::temp_dir().join(format!("astrolabe-lonecr-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("old.py");
        std::fs::write(
            &f,
            "def alpha():\r    return 1\r\rdef beta():\r    return 2\r",
        )
        .unwrap();
        let src = read_source(&f).unwrap();
        let pool = crate::parse::ParserPool::new();
        let p = pool
            .parse(
                crate::types::Language::Python,
                &crate::types::RelPath::new("old.py"),
                &src,
            )
            .unwrap();
        let lines: Vec<(String, u32)> = p
            .symbols
            .iter()
            .map(|s| (s.name.clone(), s.start_line))
            .collect();
        assert_eq!(lines, vec![("alpha".into(), 1), ("beta".into(), 4)]);
    }

    #[test]
    fn latin1_is_decoded() {
        let text = decode_source(b"# caf\xE9\ndef hello(): pass\n".to_vec());
        assert!(text.contains("def hello()"));
        assert!(text.contains("caf\u{e9}"));
    }
}
