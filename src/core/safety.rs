//! Checks on what an unknown sender asks us to do, made before the user (or a trusted-device rule)
//! accepts a transfer. The file-name rule lives in `rqs_lib::safety`, which also applies it when
//! the files are created, so the core and the library cannot disagree.

pub use rqs_lib::safety::unsafe_file_name;

#[cfg(test)]
mod tests {
    use rqs_lib::safety::MAX_NAME_BYTES;

    use super::*;

    #[test]
    fn plain_names_are_fine() {
        for name in [
            "photo.jpg",
            "Video 6.mp4",
            "relazione finale (2).pdf",
            ".bashrc",
            "a.b.c",
        ] {
            assert!(!unsafe_file_name(name), "{name}");
        }
    }

    #[test]
    fn paths_and_odd_names_are_not() {
        let long = "x".repeat(MAX_NAME_BYTES + 1);
        for name in [
            "",
            ".",
            "..",
            "../x",
            "a/b",
            "/etc/passwd",
            "..\\x",
            "a\0b",
            "line\nbreak",
            "photo\u{202E}gpj.exe",
            "zero\u{200B}width",
            long.as_str(),
        ] {
            assert!(unsafe_file_name(name), "{name:?}");
        }
    }
}
