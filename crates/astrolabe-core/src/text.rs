//! Source-file decoding.
//!
//! Most repositories are UTF-8, but legacy trees (GBK / Shift-JIS / Latin-1
//! comments and string literals) are common. `read_to_string` rejects them
//! outright, which silently drops the whole file from the index. Here
//! UTF-8 stays the fast path; anything else is BOM-sniffed or detected with
//! `chardetng` and decoded (malformed sequences become U+FFFD).

use std::io;
use std::path::Path;

/// Decode raw file bytes to text. Never fails.
pub fn decode_source(bytes: Vec<u8>) -> String {
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
    let encoding = detector.guess(None, chardetng::Utf8Detection::Deny);
    let (text, _, _) = encoding.decode(&bytes);
    text.into_owned()
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
    fn latin1_is_decoded() {
        let text = decode_source(b"# caf\xE9\ndef hello(): pass\n".to_vec());
        assert!(text.contains("def hello()"));
        assert!(text.contains("caf\u{e9}"));
    }
}
