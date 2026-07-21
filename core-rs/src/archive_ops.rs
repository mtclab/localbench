use flate2::{write::DeflateEncoder, Compression};
use js_sys::{Array, Uint8Array};
use std::{
    collections::BTreeSet,
    io::{self, Cursor, Read, Write},
};
use wasm_bindgen::prelude::*;
use zip::{
    result::ZipError, write::SimpleFileOptions, CompressionMethod, DateTime, ZipArchive, ZipWriter,
};

pub const MAX_ARCHIVE_ENTRY_BYTES: u64 = 512_000_000;
pub const MAX_ARCHIVE_TOTAL_BYTES: u64 = 512_000_000;

const ENCRYPTED_ARCHIVE_ERROR: &str = "This archive is password-protected, which isn't supported.";
const ENTRY_TOO_LARGE_ERROR: &str = "This archive entry is too large to extract safely.";
const ARCHIVE_TOO_LARGE_ERROR: &str = "This archive is too large to extract safely.";
const DEFLATE_LEVEL: u32 = 6;

#[derive(Default)]
struct CountingWriter {
    bytes: u64,
}

impl Write for CountingWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let length = u64::try_from(buffer.len())
            .map_err(|_| io::Error::other("compressed data length overflow"))?;
        self.bytes = self
            .bytes
            .checked_add(length)
            .ok_or_else(|| io::Error::other("compressed data length overflow"))?;
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn map_zip_error(context: &str, error: ZipError) -> String {
    match error {
        ZipError::UnsupportedArchive(detail) if detail == ZipError::PASSWORD_REQUIRED => {
            ENCRYPTED_ARCHIVE_ERROR.to_owned()
        }
        ZipError::InvalidPassword => ENCRYPTED_ARCHIVE_ERROR.to_owned(),
        other => format!("{context}: {other}"),
    }
}

fn fixed_modified_time() -> Result<DateTime, String> {
    DateTime::from_date_and_time(1980, 1, 1, 0, 0, 0)
        .map_err(|error| format!("Could not set the fixed archive timestamp: {error}"))
}

fn deflated_size(bytes: &[u8]) -> Result<u64, String> {
    let mut encoder =
        DeflateEncoder::new(CountingWriter::default(), Compression::new(DEFLATE_LEVEL));
    encoder
        .write_all(bytes)
        .map_err(|error| format!("Could not measure a compressed archive entry: {error}"))?;
    encoder
        .finish()
        .map(|counter| counter.bytes)
        .map_err(|error| format!("Could not finish measuring an archive entry: {error}"))
}

fn duplicate_name(name: &str, suffix: u64) -> String {
    let separator = name.rfind(['/', '\\']);
    let (directory, filename) = separator
        .map(|index| name.split_at(index + 1))
        .unwrap_or(("", name));
    let extension = filename
        .rfind('.')
        .filter(|index| *index > 0)
        .unwrap_or(filename.len());
    format!(
        "{directory}{}-{suffix}{}",
        &filename[..extension],
        &filename[extension..]
    )
}

fn unique_names(names: impl IntoIterator<Item = String>) -> Vec<String> {
    // A BTreeSet avoids randomized hash seeding: archive creation performs no
    // entropy or clock reads, even though only membership checks are needed.
    let mut used = BTreeSet::new();
    let mut output = Vec::new();

    for supplied_name in names {
        let original = if supplied_name.is_empty() {
            "file".to_owned()
        } else {
            supplied_name
        };
        let mut candidate = original.clone();
        let mut suffix = 1_u64;
        while !used.insert(candidate.clone()) {
            candidate = duplicate_name(&original, suffix);
            suffix = suffix.saturating_add(1);
        }
        output.push(candidate);
    }

    output
}

fn create_archive_with_limit(
    entries: Vec<(String, Vec<u8>)>,
    total_limit: u64,
) -> Result<Vec<u8>, String> {
    if entries.is_empty() {
        return Err("Choose at least one file to archive.".to_owned());
    }

    let total_bytes = entries.iter().try_fold(0_u64, |total, (_, bytes)| {
        let length = u64::try_from(bytes.len())
            .map_err(|_| "These files are too large to archive safely.".to_owned())?;
        total
            .checked_add(length)
            .ok_or_else(|| "These files are too large to archive safely.".to_owned())
    })?;
    if total_bytes > total_limit {
        return Err("These files are too large to archive safely.".to_owned());
    }

    let names = unique_names(entries.iter().map(|(name, _)| name.clone()));
    let modified_time = fixed_modified_time()?;
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));

    for ((_, bytes), name) in entries.into_iter().zip(names) {
        let input_size = u64::try_from(bytes.len())
            .map_err(|_| "This file is too large to archive safely.".to_owned())?;
        let method = if deflated_size(&bytes)? < input_size {
            CompressionMethod::Deflated
        } else {
            CompressionMethod::Stored
        };
        let mut options = SimpleFileOptions::default()
            .compression_method(method)
            .last_modified_time(modified_time);
        if method == CompressionMethod::Deflated {
            options = options.compression_level(Some(i64::from(DEFLATE_LEVEL)));
        }

        writer
            .start_file(name, options)
            .map_err(|error| map_zip_error("Could not add a file to the archive", error))?;
        writer
            .write_all(&bytes)
            .map_err(|error| format!("Could not write a file to the archive: {error}"))?;
    }

    writer
        .finish()
        .map(|cursor| cursor.into_inner())
        .map_err(|error| map_zip_error("Could not finish the archive", error))
}

/// Pure core: build a deterministic Store+Deflate archive from ordered entries.
pub(crate) fn create_archive(entries: Vec<(String, Vec<u8>)>) -> Result<Vec<u8>, String> {
    create_archive_with_limit(entries, MAX_ARCHIVE_TOTAL_BYTES)
}

fn is_unsafe_path(name: &str) -> bool {
    let bytes = name.as_bytes();
    let has_windows_root = bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'/' | b'\\');
    name.starts_with(['/', '\\'])
        || has_windows_root
        || name.split(['/', '\\']).any(|component| component == "..")
}

fn push_json_string(output: &mut String, value: &str) {
    output.push('"');
    for character in value.chars() {
        match character {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\u{08}' => output.push_str("\\b"),
            '\u{0c}' => output.push_str("\\f"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            control if control <= '\u{1f}' => {
                use std::fmt::Write as _;
                let _ = write!(output, "\\u{:04x}", u32::from(control));
            }
            other => output.push(other),
        }
    }
    output.push('"');
}

/// Pure core: return the entry metadata needed by the extract interface.
pub(crate) fn list_archive(bytes: &[u8]) -> Result<String, String> {
    if bytes.is_empty() {
        return Err("Choose a ZIP archive to open.".to_owned());
    }

    let mut archive = ZipArchive::new(Cursor::new(bytes))
        .map_err(|error| map_zip_error("Could not read this ZIP archive", error))?;
    let mut output = String::from("{\"entries\":[");
    let mut total_size = 0_u64;

    for index in 0..archive.len() {
        let file = archive
            .by_index_raw(index)
            .map_err(|error| map_zip_error("Could not read an archive entry", error))?;
        if file.encrypted() {
            return Err(ENCRYPTED_ARCHIVE_ERROR.to_owned());
        }
        total_size = total_size.saturating_add(file.size());

        if index != 0 {
            output.push(',');
        }
        output.push_str("{\"name\":");
        push_json_string(&mut output, file.name());
        output.push_str(",\"size\":");
        output.push_str(&file.size().to_string());
        output.push_str(",\"compressed\":");
        output.push_str(&file.compressed_size().to_string());
        output.push_str(",\"is_dir\":");
        output.push_str(if file.is_dir() { "true" } else { "false" });
        output.push_str(",\"unsafe_path\":");
        output.push_str(if is_unsafe_path(file.name()) {
            "true"
        } else {
            "false"
        });
        output.push('}');
    }

    // Saturating the declared total is deliberate: listing remains bounded by
    // the archive itself, while extraction independently rejects over-cap sums.
    let _archive_exceeds_total_limit = total_size > MAX_ARCHIVE_TOTAL_BYTES;
    output.push_str("]}");
    Ok(output)
}

fn extract_entry_with_limits(
    bytes: &[u8],
    index: u32,
    entry_limit: u64,
    total_limit: u64,
) -> Result<Vec<u8>, String> {
    if bytes.is_empty() {
        return Err("Choose a ZIP archive to open.".to_owned());
    }

    let mut archive = ZipArchive::new(Cursor::new(bytes))
        .map_err(|error| map_zip_error("Could not read this ZIP archive", error))?;
    let selected_index =
        usize::try_from(index).map_err(|_| "That archive entry does not exist.".to_owned())?;
    if selected_index >= archive.len() {
        return Err("That archive entry does not exist.".to_owned());
    }

    let mut total_size = 0_u64;
    let mut selected_size = None;
    for entry_index in 0..archive.len() {
        let file = archive
            .by_index_raw(entry_index)
            .map_err(|error| map_zip_error("Could not read an archive entry", error))?;
        if file.encrypted() {
            return Err(ENCRYPTED_ARCHIVE_ERROR.to_owned());
        }
        total_size = total_size.saturating_add(file.size());
        if entry_index == selected_index {
            selected_size = Some(file.size());
        }
    }

    if total_size > total_limit {
        return Err(ARCHIVE_TOO_LARGE_ERROR.to_owned());
    }
    if selected_size.is_some_and(|size| size > entry_limit) {
        return Err(ENTRY_TOO_LARGE_ERROR.to_owned());
    }

    let mut file = archive
        .by_index(selected_index)
        .map_err(|error| map_zip_error("Could not open this archive entry", error))?;
    let mut limited = (&mut file).take(entry_limit.saturating_add(1));
    let mut extracted = Vec::new();
    limited
        .read_to_end(&mut extracted)
        .map_err(|error| format!("Could not extract this archive entry: {error}"))?;
    if u64::try_from(extracted.len()).unwrap_or(u64::MAX) > entry_limit {
        return Err(ENTRY_TOO_LARGE_ERROR.to_owned());
    }
    Ok(extracted)
}

/// Pure core: extract one entry by its stable listing index with bounded reads.
pub(crate) fn extract_entry(bytes: &[u8], index: u32) -> Result<Vec<u8>, String> {
    extract_entry_with_limits(
        bytes,
        index,
        MAX_ARCHIVE_ENTRY_BYTES,
        MAX_ARCHIVE_TOTAL_BYTES,
    )
}

/// Build a deterministic Store+Deflate ZIP from parallel name and byte arrays.
#[wasm_bindgen]
pub fn create_zip(names: Vec<String>, buffers: Array) -> Result<Vec<u8>, JsValue> {
    if names.len() != buffers.length() as usize {
        return Err(JsValue::from_str(
            "Every archive filename must have one matching file buffer.",
        ));
    }
    let entries = names
        .into_iter()
        .zip(buffers.iter())
        .map(|(name, bytes)| (name, Uint8Array::new(&bytes).to_vec()))
        .collect();
    create_archive(entries).map_err(|error| JsValue::from_str(&error))
}

/// List a ZIP archive's entries as hand-serialized JSON.
#[wasm_bindgen]
pub fn list_zip(bytes: &[u8]) -> Result<String, JsValue> {
    list_archive(bytes).map_err(|error| JsValue::from_str(&error))
}

/// Extract one ZIP entry by its index in list order.
#[wasm_bindgen]
pub fn extract_zip_entry(bytes: &[u8], index: u32) -> Result<Vec<u8>, JsValue> {
    extract_entry(bytes, index).map_err(|error| JsValue::from_str(&error))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw_archive(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        let options = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Stored)
            .last_modified_time(fixed_modified_time().expect("fixed timestamp should be valid"));
        for (name, bytes) in entries {
            writer
                .start_file(*name, options)
                .expect("fixture entry should start");
            writer.write_all(bytes).expect("fixture bytes should write");
        }
        writer
            .finish()
            .expect("fixture archive should finish")
            .into_inner()
    }

    fn mark_first_entry_encrypted(mut bytes: Vec<u8>) -> Vec<u8> {
        bytes[6] |= 1;
        let central = bytes
            .windows(4)
            .position(|window| window == b"PK\x01\x02")
            .expect("fixture should contain a central header");
        bytes[central + 8] |= 1;
        bytes
    }

    fn lie_about_first_entry_size(mut bytes: Vec<u8>, declared_size: u32) -> Vec<u8> {
        bytes[22..26].copy_from_slice(&declared_size.to_le_bytes());
        let central = bytes
            .windows(4)
            .position(|window| window == b"PK\x01\x02")
            .expect("fixture should contain a central header");
        bytes[central + 24..central + 28].copy_from_slice(&declared_size.to_le_bytes());
        bytes
    }

    #[test]
    fn round_trips_two_entries_byte_identically() {
        let first = b"private notes\n".to_vec();
        let second = vec![0, 1, 2, 3, 254, 255];
        let archive = create_archive(vec![
            ("notes.txt".to_owned(), first.clone()),
            ("data.bin".to_owned(), second.clone()),
        ])
        .expect("archive should be created");
        let report = list_archive(&archive).expect("archive should list");

        assert!(report.contains("{\"name\":\"notes.txt\",\"size\":14,\"compressed\":"));
        assert!(report.contains("\"is_dir\":false,\"unsafe_path\":false"));
        assert!(report.contains("{\"name\":\"data.bin\",\"size\":6,\"compressed\":"));
        assert_eq!(
            extract_entry(&archive, 0).expect("first should extract"),
            first
        );
        assert_eq!(
            extract_entry(&archive, 1).expect("second should extract"),
            second
        );
        println!(
            "round-trip example: 2 files (20 input bytes) -> {} ZIP bytes -> both byte-identical",
            archive.len()
        );
    }

    #[test]
    fn creation_is_byte_deterministic() {
        let entries = vec![
            ("a.txt".to_owned(), vec![b'a'; 4_096]),
            ("b.bin".to_owned(), (0_u8..=255).collect()),
        ];
        assert_eq!(
            create_archive(entries.clone()).expect("first archive should build"),
            create_archive(entries).expect("second archive should build")
        );
    }

    #[test]
    fn created_entries_use_the_dos_epoch() {
        let bytes = create_archive(vec![("clock.txt".to_owned(), b"secret".to_vec())])
            .expect("archive should build");
        let mut archive = ZipArchive::new(Cursor::new(bytes)).expect("archive should parse");
        let file = archive.by_index_raw(0).expect("entry should exist");
        assert_eq!(
            file.last_modified(),
            Some(fixed_modified_time().expect("valid time"))
        );
    }

    #[test]
    fn already_compressed_style_bytes_are_stored_without_growth() {
        let mut state = 0x1234_5678_u32;
        let bytes = (0..65_536)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (state >> 24) as u8
            })
            .collect::<Vec<_>>();
        let archive_bytes =
            create_archive(vec![("noise.bin".to_owned(), bytes.clone())]).expect("should build");
        let mut archive = ZipArchive::new(Cursor::new(archive_bytes)).expect("should parse");
        let file = archive.by_index_raw(0).expect("entry should exist");
        assert_eq!(file.compression(), CompressionMethod::Stored);
        assert_eq!(file.compressed_size(), bytes.len() as u64);
    }

    #[test]
    fn compressible_bytes_use_deflate() {
        let archive_bytes = create_archive(vec![("repeat.txt".to_owned(), vec![b'x'; 16_384])])
            .expect("should build");
        let mut archive = ZipArchive::new(Cursor::new(archive_bytes)).expect("should parse");
        let file = archive.by_index_raw(0).expect("entry should exist");
        assert_eq!(file.compression(), CompressionMethod::Deflated);
        assert!(file.compressed_size() < file.size());
    }

    #[test]
    fn extraction_stops_at_the_streaming_cap_even_when_size_is_false() {
        let archive = lie_about_first_entry_size(raw_archive(&[("large.txt", &[b'z'; 128])]), 1);
        let error = extract_entry_with_limits(&archive, 0, 64, 1_024)
            .expect_err("entry should exceed the test cap");
        assert_eq!(error, ENTRY_TOO_LARGE_ERROR);
    }

    #[test]
    fn extraction_rejects_an_over_cap_declared_total() {
        let archive = raw_archive(&[("a", &[0; 40]), ("b", &[0; 40])]);
        let error = extract_entry_with_limits(&archive, 0, 128, 64)
            .expect_err("declared total should exceed the test cap");
        assert_eq!(error, ARCHIVE_TOO_LARGE_ERROR);
    }

    #[test]
    fn listing_flags_unsafe_paths() {
        let archive = raw_archive(&[
            ("../evil.txt", b"one"),
            ("/abs.txt", b"two"),
            ("a/b.txt", b"three"),
            ("C:\\escape.txt", b"four"),
        ]);
        let report = list_archive(&archive).expect("archive should list");
        assert!(report.contains("\"name\":\"../evil.txt\""));
        assert!(report.contains("\"name\":\"../evil.txt\",\"size\":3,\"compressed\":3,\"is_dir\":false,\"unsafe_path\":true"));
        assert!(report.contains("\"name\":\"/abs.txt\",\"size\":3,\"compressed\":3,\"is_dir\":false,\"unsafe_path\":true"));
        assert!(report.contains("\"name\":\"a/b.txt\",\"size\":5,\"compressed\":5,\"is_dir\":false,\"unsafe_path\":false"));
        assert!(report.contains("\"name\":\"C:\\\\escape.txt\",\"size\":4,\"compressed\":4,\"is_dir\":false,\"unsafe_path\":true"));
    }

    #[test]
    fn encrypted_entries_return_the_password_message() {
        let archive = mark_first_entry_encrypted(raw_archive(&[("secret.txt", b"classified")]));
        assert_eq!(
            list_archive(&archive).expect_err("listing must reject encryption"),
            ENCRYPTED_ARCHIVE_ERROR
        );
        assert_eq!(
            extract_entry(&archive, 0).expect_err("extract must reject encryption"),
            ENCRYPTED_ARCHIVE_ERROR
        );
    }

    #[test]
    fn malformed_empty_and_truncated_inputs_never_succeed() {
        let valid = raw_archive(&[("ok.txt", b"okay")]);
        for bytes in [
            Vec::new(),
            b"not a zip".to_vec(),
            valid[..valid.len() / 2].to_vec(),
        ] {
            assert!(list_archive(&bytes).is_err());
            assert!(extract_entry(&bytes, 0).is_err());
        }
    }

    #[test]
    fn rejects_out_of_range_entry_index() {
        let archive = raw_archive(&[("only.txt", b"one")]);
        assert_eq!(
            extract_entry(&archive, 9).expect_err("index should not exist"),
            "That archive entry does not exist."
        );
    }

    #[test]
    fn rejects_empty_creation_and_over_limit_inputs() {
        assert_eq!(
            create_archive(Vec::new()).expect_err("empty creation must fail"),
            "Choose at least one file to archive."
        );
        assert_eq!(
            create_archive_with_limit(vec![("large".to_owned(), vec![0; 65])], 64)
                .expect_err("test limit should reject input"),
            "These files are too large to archive safely."
        );
    }

    #[test]
    fn duplicate_names_receive_stable_suffixes() {
        let archive = create_archive(vec![
            ("report.txt".to_owned(), b"first".to_vec()),
            ("report.txt".to_owned(), b"second".to_vec()),
            ("report-1.txt".to_owned(), b"third".to_vec()),
            ("report.txt".to_owned(), b"fourth".to_vec()),
        ])
        .expect("archive should build");
        let mut parsed = ZipArchive::new(Cursor::new(archive)).expect("archive should parse");
        let names = (0..parsed.len())
            .map(|index| {
                parsed
                    .by_index_raw(index)
                    .expect("entry should exist")
                    .name()
                    .to_owned()
            })
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                "report.txt",
                "report-1.txt",
                "report-1-1.txt",
                "report-2.txt"
            ]
        );
    }

    #[test]
    fn listing_escapes_names_as_valid_json_strings() {
        let archive = raw_archive(&[("quote\"line\n.txt", b"x")]);
        let report = list_archive(&archive).expect("archive should list");
        assert!(report.contains("\"name\":\"quote\\\"line\\n.txt\""));
        assert!(!report.contains("line\n.txt"));
    }
}
