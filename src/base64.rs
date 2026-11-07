//! Base64, for setting the clipboard with OSC 52.

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn symbol(group: u32, shift: u32) -> char {
    let index = usize::try_from((group >> shift) & 0x3f).unwrap_or(0);
    ALPHABET.get(index).map_or('A', |byte| char::from(*byte))
}

pub fn encode(data: &[u8]) -> String {
    let mut text = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let byte = |index: usize| chunk.get(index).map_or(0, |byte| u32::from(*byte));
        let group = (byte(0) << 16) | (byte(1) << 8) | byte(2);
        text.push(symbol(group, 18));
        text.push(symbol(group, 12));
        text.push(if chunk.len() > 1 {
            symbol(group, 6)
        } else {
            '='
        });
        text.push(if chunk.len() > 2 {
            symbol(group, 0)
        } else {
            '='
        });
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_the_rfc_4648_test_vectors() {
        for (input, output) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(encode(input.as_bytes()), output);
        }
    }
}
