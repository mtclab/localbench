//! ZIP creation and reading.
//!
//! Creation uses the `zip` crate's writer. Reading walks the archive's own
//! central directory instead, because a name-keyed reader silently collapses
//! entries that share a filename: a spec-legal archive with two `notes.txt`
//! entries would be listed as one file, and the other one would be gone with
//! no signal. Every entry the directory records is listed and extractable.

use flate2::{read::DeflateDecoder, write::DeflateEncoder, Compression, Crc};
use js_sys::{Array, Uint8Array};
use std::{
    collections::BTreeSet,
    io::{self, Cursor, Read, Write},
};
use wasm_bindgen::prelude::*;
use zip::{result::ZipError, write::SimpleFileOptions, CompressionMethod, DateTime, ZipWriter};

pub const MAX_ARCHIVE_ENTRY_BYTES: u64 = 512_000_000;
pub const MAX_ARCHIVE_TOTAL_BYTES: u64 = 512_000_000;

const ENCRYPTED_ARCHIVE_ERROR: &str = "This archive is password-protected, which isn't supported.";
const ENCRYPTED_ENTRY_ERROR: &str =
    "This file inside the archive is password-protected, which isn't supported.";
const ENTRY_TOO_LARGE_ERROR: &str = "This archive entry is too large to extract safely.";
const UNSUPPORTED_METHOD_ERROR: &str =
    "This file inside the archive uses a compression method this tool cannot read.";
const DAMAGED_ENTRY_ERROR: &str =
    "This file inside the archive is damaged: its contents do not match the archive's own checksum.";
const UNREADABLE_ARCHIVE_ERROR: &str = "Could not read this ZIP archive.";
const DEFLATE_LEVEL: u32 = 6;

const METHOD_STORE: u16 = 0;
const METHOD_DEFLATE: u16 = 8;

/// The upper half of code page 437, used by ZIP entries that predate the
/// UTF-8 flag. Only consulted when the raw name is not already valid UTF-8,
/// which is what every mainstream unzip tool does.
const CP437_HIGH: [char; 128] = [
    'Ç', 'ü', 'é', 'â', 'ä', 'à', 'å', 'ç', 'ê', 'ë', 'è', 'ï', 'î', 'ì', 'Ä', 'Å', 'É', 'æ', 'Æ',
    'ô', 'ö', 'ò', 'û', 'ù', 'ÿ', 'Ö', 'Ü', '¢', '£', '¥', '₧', 'ƒ', 'á', 'í', 'ó', 'ú', 'ñ', 'Ñ',
    'ª', 'º', '¿', '⌐', '¬', '½', '¼', '¡', '«', '»', '░', '▒', '▓', '│', '┤', '╡', '╢', '╖', '╕',
    '╣', '║', '╗', '╝', '╜', '╛', '┐', '└', '┴', '┬', '├', '─', '┼', '╞', '╟', '╚', '╔', '╩', '╦',
    '╠', '═', '╬', '╧', '╨', '╤', '╥', '╙', '╘', '╒', '╓', '╫', '╪', '┘', '┌', '█', '▄', '▌', '▐',
    '▀', 'α', 'ß', 'Γ', 'π', 'Σ', 'σ', 'µ', 'τ', 'Φ', 'Θ', 'Ω', 'δ', '∞', 'φ', 'ε', '∩', '≡', '±',
    '≥', '≤', '⌠', '⌡', '÷', '≈', '°', '∙', '·', '√', 'ⁿ', '²', '■', '\u{a0}',
];

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
    // `C:\evil.txt` is rooted and `C:evil.txt` is drive-relative; both escape
    // the folder the user is extracting into, so both count as unsafe.
    let has_drive_letter = bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
    name.starts_with(['/', '\\'])
        || has_drive_letter
        || name.split(['/', '\\']).any(|component| component == "..")
}

/// One entry exactly as the archive's central directory records it.
#[derive(Clone, Debug)]
struct ArchiveEntry {
    name: String,
    size: u64,
    compressed_size: u64,
    crc32: u32,
    method: u16,
    encrypted: bool,
    local_header_offset: u64,
    duplicate_name: bool,
}

impl ArchiveEntry {
    fn is_dir(&self) -> bool {
        self.name.ends_with('/') || self.name.ends_with('\\')
    }

    fn is_readable(&self) -> bool {
        !self.encrypted && matches!(self.method, METHOD_STORE | METHOD_DEFLATE)
    }
}

fn read_u16(bytes: &[u8], at: usize) -> Option<u16> {
    let slice = bytes.get(at..at + 2)?;
    Some(u16::from_le_bytes([slice[0], slice[1]]))
}

fn read_u32(bytes: &[u8], at: usize) -> Option<u32> {
    let slice = bytes.get(at..at + 4)?;
    Some(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

fn read_u64(bytes: &[u8], at: usize) -> Option<u64> {
    let slice = bytes.get(at..at + 8)?;
    let mut value = [0_u8; 8];
    value.copy_from_slice(slice);
    Some(u64::from_le_bytes(value))
}

/// Decode a stored filename. The UTF-8 flag is honoured, and so is the fact
/// that most modern tools write UTF-8 without setting it; only a name that is
/// not valid UTF-8 falls back to code page 437.
fn decode_entry_name(raw: &[u8], utf8_flag: bool) -> String {
    if utf8_flag {
        return String::from_utf8_lossy(raw).into_owned();
    }
    if let Ok(text) = std::str::from_utf8(raw) {
        return text.to_owned();
    }
    raw.iter()
        .map(|byte| {
            if byte.is_ascii() {
                char::from(*byte)
            } else {
                CP437_HIGH[usize::from(*byte) - 128]
            }
        })
        .collect()
}

/// Locate the end-of-central-directory record, which is the only fixed point
/// in a ZIP file and always sits at the end (before an optional comment).
fn find_end_of_central_directory(bytes: &[u8]) -> Option<usize> {
    let window = bytes.len().min(u16::MAX as usize + 22);
    let start = bytes.len() - window;
    (start..=bytes.len().checked_sub(22)?).rev().find(|at| {
        read_u32(bytes, *at) == Some(0x0605_4b50)
            && read_u16(bytes, at + 20).map(usize::from) == Some(bytes.len() - (at + 22))
    })
}

/// Apply a ZIP64 extended-information extra field, which carries the real
/// sizes and offset whenever the 32-bit fields are saturated.
fn apply_zip64_extra(extra: &[u8], entry: &mut ArchiveEntry, saturated: [bool; 3]) {
    let mut position = 0;
    while let (Some(id), Some(length)) = (read_u16(extra, position), read_u16(extra, position + 2))
    {
        let start = position + 4;
        let end = start + usize::from(length);
        if end > extra.len() {
            return;
        }
        if id == 0x0001 {
            let field = &extra[start..end];
            let mut at = 0;
            // The fields appear in a fixed order, but only for the values that
            // were saturated in the fixed-size record.
            for (index, is_saturated) in saturated.iter().enumerate() {
                if !is_saturated {
                    continue;
                }
                let Some(value) = read_u64(field, at) else {
                    return;
                };
                match index {
                    0 => entry.size = value,
                    1 => entry.compressed_size = value,
                    _ => entry.local_header_offset = value,
                }
                at += 8;
            }
            return;
        }
        position = end;
    }
}

/// Read every entry the archive's central directory declares, in directory
/// order, including entries whose names repeat.
fn read_central_directory(bytes: &[u8]) -> Result<(Vec<ArchiveEntry>, u64), String> {
    let eocd = find_end_of_central_directory(bytes).ok_or(UNREADABLE_ARCHIVE_ERROR.to_owned())?;
    let mut entry_count = u64::from(read_u16(bytes, eocd + 10).unwrap_or(0));
    let mut directory_size = u64::from(read_u32(bytes, eocd + 12).unwrap_or(0));
    let mut directory_offset = u64::from(read_u32(bytes, eocd + 16).unwrap_or(0));

    // ZIP64: the real counts live in a separate record just before the locator.
    if entry_count == u64::from(u16::MAX)
        || directory_size == u64::from(u32::MAX)
        || directory_offset == u64::from(u32::MAX)
    {
        let search_start = eocd.saturating_sub(128);
        if let Some(zip64) = (search_start..eocd)
            .rev()
            .find(|at| read_u32(bytes, *at) == Some(0x0606_4b50))
        {
            entry_count = read_u64(bytes, zip64 + 32).unwrap_or(entry_count);
            directory_size = read_u64(bytes, zip64 + 40).unwrap_or(directory_size);
            directory_offset = read_u64(bytes, zip64 + 48).unwrap_or(directory_offset);
        }
    }

    // An archive with nothing in it is valid, just empty.
    if entry_count == 0 && directory_size == 0 {
        return Ok((Vec::new(), 0));
    }

    // A self-extracting archive has a prefix, so the recorded offset is not a
    // file position. Prefer the position the directory's own size implies.
    let is_directory_start = |position: u64| {
        usize::try_from(position)
            .ok()
            .and_then(|position| read_u32(bytes, position))
            == Some(0x0201_4b50)
    };
    let implied_start = (eocd as u64).checked_sub(directory_size);
    let directory_start = implied_start
        .filter(|position| is_directory_start(*position))
        .or(Some(directory_offset).filter(|position| is_directory_start(*position)))
        .ok_or(UNREADABLE_ARCHIVE_ERROR.to_owned())?;
    let archive_offset = directory_start.saturating_sub(directory_offset);

    let mut entries = Vec::new();
    let mut position = usize::try_from(directory_start).map_err(|_| UNREADABLE_ARCHIVE_ERROR)?;
    // Trust the declared count, but never read past the directory itself: a
    // truncated or over-stated directory must stop at its own end.
    let directory_end = usize::try_from(directory_start.saturating_add(directory_size))
        .unwrap_or(bytes.len())
        .min(bytes.len());
    while (entries.len() as u64) < entry_count && position < directory_end {
        if read_u32(bytes, position) != Some(0x0201_4b50) {
            break;
        }
        let flags = read_u16(bytes, position + 8).ok_or(UNREADABLE_ARCHIVE_ERROR)?;
        let method = read_u16(bytes, position + 10).ok_or(UNREADABLE_ARCHIVE_ERROR)?;
        let crc32 = read_u32(bytes, position + 16).ok_or(UNREADABLE_ARCHIVE_ERROR)?;
        let compressed_size = read_u32(bytes, position + 20).ok_or(UNREADABLE_ARCHIVE_ERROR)?;
        let size = read_u32(bytes, position + 24).ok_or(UNREADABLE_ARCHIVE_ERROR)?;
        let name_length =
            usize::from(read_u16(bytes, position + 28).ok_or(UNREADABLE_ARCHIVE_ERROR)?);
        let extra_length =
            usize::from(read_u16(bytes, position + 30).ok_or(UNREADABLE_ARCHIVE_ERROR)?);
        let comment_length =
            usize::from(read_u16(bytes, position + 32).ok_or(UNREADABLE_ARCHIVE_ERROR)?);
        let local_header_offset = read_u32(bytes, position + 42).ok_or(UNREADABLE_ARCHIVE_ERROR)?;

        let name_start = position + 46;
        let raw_name = bytes
            .get(name_start..name_start + name_length)
            .ok_or(UNREADABLE_ARCHIVE_ERROR)?;
        let extra = bytes
            .get(name_start + name_length..name_start + name_length + extra_length)
            .ok_or(UNREADABLE_ARCHIVE_ERROR)?;

        let mut entry = ArchiveEntry {
            name: decode_entry_name(raw_name, flags & 0x800 != 0),
            size: u64::from(size),
            compressed_size: u64::from(compressed_size),
            crc32,
            method,
            encrypted: flags & 1 != 0,
            local_header_offset: u64::from(local_header_offset).saturating_add(archive_offset),
            duplicate_name: false,
        };
        let saturated = [
            size == u32::MAX,
            compressed_size == u32::MAX,
            local_header_offset == u32::MAX,
        ];
        if saturated.iter().any(|value| *value) {
            let before = entry.local_header_offset;
            apply_zip64_extra(extra, &mut entry, saturated);
            if entry.local_header_offset != before {
                entry.local_header_offset =
                    entry.local_header_offset.saturating_add(archive_offset);
            }
        }
        entries.push(entry);

        position = name_start + name_length + extra_length + comment_length;
    }

    if entries.is_empty() && entry_count > 0 {
        return Err(UNREADABLE_ARCHIVE_ERROR.to_owned());
    }

    // Flag repeated names so the interface can say which downloads share one.
    for index in 0..entries.len() {
        let repeated = entries
            .iter()
            .enumerate()
            .any(|(other, entry)| other != index && entry.name == entries[index].name);
        entries[index].duplicate_name = repeated;
    }

    let total_size = entries
        .iter()
        .fold(0_u64, |total, entry| total.saturating_add(entry.size));
    Ok((entries, total_size))
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

/// Pure core: return the entry metadata needed by the extract interface, with
/// one JSON object per entry the archive actually contains and a `warnings`
/// list stating anything that will keep an entry from being extracted.
///
/// Listing tells the whole truth up front: an entry that cannot be extracted
/// says so here, rather than looking fine until the download is attempted.
pub(crate) fn list_archive(bytes: &[u8]) -> Result<String, String> {
    if bytes.is_empty() {
        return Err("Choose a ZIP archive to open.".to_owned());
    }

    let (entries, total_size) = read_central_directory(bytes)?;
    let mut output = String::from("{\"entries\":[");

    for (index, entry) in entries.iter().enumerate() {
        if index != 0 {
            output.push(',');
        }
        output.push_str("{\"name\":");
        push_json_string(&mut output, &entry.name);
        output.push_str(",\"size\":");
        output.push_str(&entry.size.to_string());
        output.push_str(",\"compressed\":");
        output.push_str(&entry.compressed_size.to_string());
        output.push_str(",\"is_dir\":");
        output.push_str(if entry.is_dir() { "true" } else { "false" });
        output.push_str(",\"unsafe_path\":");
        output.push_str(if is_unsafe_path(&entry.name) {
            "true"
        } else {
            "false"
        });
        output.push_str(",\"encrypted\":");
        output.push_str(if entry.encrypted { "true" } else { "false" });
        output.push_str(",\"duplicate_name\":");
        output.push_str(if entry.duplicate_name {
            "true"
        } else {
            "false"
        });
        output.push_str(",\"extractable\":");
        let extractable = entry.is_readable() && entry.size <= MAX_ARCHIVE_ENTRY_BYTES;
        output.push_str(if extractable { "true" } else { "false" });
        output.push('}');
    }
    output.push_str("],\"total_size\":");
    output.push_str(&total_size.to_string());
    output.push_str(",\"warnings\":[");

    let mut warnings: Vec<String> = Vec::new();
    if entries.iter().any(|entry| entry.duplicate_name) {
        warnings.push(
            "This archive contains several files with the same name. They are listed separately and each one downloads its own contents.".to_owned(),
        );
    }
    if entries.iter().any(|entry| entry.encrypted) {
        warnings.push(
            "Some files in this archive are password-protected and cannot be extracted here. The other files are unaffected.".to_owned(),
        );
    }
    if entries
        .iter()
        .any(|entry| !matches!(entry.method, METHOD_STORE | METHOD_DEFLATE))
    {
        warnings.push(
            "Some files in this archive use a compression method this tool cannot read (only Store and Deflate are supported). The other files are unaffected.".to_owned(),
        );
    }
    if entries
        .iter()
        .any(|entry| entry.size > MAX_ARCHIVE_ENTRY_BYTES)
    {
        warnings.push(format!(
            "Some files in this archive are larger than the {} MB this tool can extract in one piece. The other files are unaffected.",
            MAX_ARCHIVE_ENTRY_BYTES / 1_000_000
        ));
    }
    for (index, warning) in warnings.iter().enumerate() {
        if index != 0 {
            output.push(',');
        }
        push_json_string(&mut output, warning);
    }

    output.push_str("]}");
    Ok(output)
}

/// Inflate or copy one entry's bytes, refusing to allocate past the cap.
fn read_entry_data(data: &[u8], entry: &ArchiveEntry, limit: u64) -> Result<Vec<u8>, String> {
    let extracted = match entry.method {
        METHOD_STORE => {
            if u64::try_from(data.len()).unwrap_or(u64::MAX) > limit {
                return Err(ENTRY_TOO_LARGE_ERROR.to_owned());
            }
            data.to_vec()
        }
        METHOD_DEFLATE => {
            let mut bounded = DeflateDecoder::new(data).take(limit.saturating_add(1));
            let mut extracted = Vec::new();
            bounded
                .read_to_end(&mut extracted)
                .map_err(|error| format!("Could not extract this archive entry: {error}"))?;
            if u64::try_from(extracted.len()).unwrap_or(u64::MAX) > limit {
                return Err(ENTRY_TOO_LARGE_ERROR.to_owned());
            }
            extracted
        }
        _ => return Err(UNSUPPORTED_METHOD_ERROR.to_owned()),
    };

    // The archive states each entry's checksum. Checking it turns silent
    // corruption into an honest error instead of a damaged download.
    let mut crc = Crc::new();
    crc.update(&extracted);
    if crc.sum() != entry.crc32 {
        return Err(DAMAGED_ENTRY_ERROR.to_owned());
    }
    Ok(extracted)
}

fn extract_entry_with_limit(bytes: &[u8], index: u32, entry_limit: u64) -> Result<Vec<u8>, String> {
    if bytes.is_empty() {
        return Err("Choose a ZIP archive to open.".to_owned());
    }

    let (entries, _) = read_central_directory(bytes)?;
    let selected = usize::try_from(index)
        .ok()
        .and_then(|index| entries.get(index))
        .ok_or_else(|| "That archive entry does not exist.".to_owned())?;

    // One entry's size is what bounds this extraction. The archive's total
    // uncompressed size is irrelevant here: refusing to hand over a 5 KB file
    // because it sits in a big archive is a limit that protects nobody.
    if selected.encrypted {
        return Err(ENCRYPTED_ENTRY_ERROR.to_owned());
    }
    if selected.size > entry_limit {
        return Err(ENTRY_TOO_LARGE_ERROR.to_owned());
    }

    let header = usize::try_from(selected.local_header_offset)
        .map_err(|_| UNREADABLE_ARCHIVE_ERROR.to_owned())?;
    if read_u32(bytes, header) != Some(0x0403_4b50) {
        return Err(UNREADABLE_ARCHIVE_ERROR.to_owned());
    }
    let name_length = usize::from(read_u16(bytes, header + 26).ok_or(UNREADABLE_ARCHIVE_ERROR)?);
    let extra_length = usize::from(read_u16(bytes, header + 28).ok_or(UNREADABLE_ARCHIVE_ERROR)?);
    let data_start = header + 30 + name_length + extra_length;
    let data_end = usize::try_from(selected.compressed_size)
        .ok()
        .and_then(|length| data_start.checked_add(length))
        .ok_or(UNREADABLE_ARCHIVE_ERROR.to_owned())?;
    let data = bytes
        .get(data_start..data_end)
        .ok_or(UNREADABLE_ARCHIVE_ERROR.to_owned())?;

    read_entry_data(data, selected, entry_limit)
}

/// Pure core: extract one entry by its stable listing index with bounded reads.
pub(crate) fn extract_entry(bytes: &[u8], index: u32) -> Result<Vec<u8>, String> {
    extract_entry_with_limit(bytes, index, MAX_ARCHIVE_ENTRY_BYTES)
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
    // Fixtures are parsed with the independent `zip` reader on purpose: the
    // creation path is checked against a second implementation, not against
    // the reader this module ships.
    use zip::ZipArchive;

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

    /// A hand-rolled ZIP writer for fixtures the `zip` crate will not produce:
    /// repeated names, legacy code-page names, unsupported methods.
    struct RawEntry {
        name: Vec<u8>,
        data: Vec<u8>,
        flags: u16,
        method: u16,
    }

    impl RawEntry {
        fn stored(name: &[u8], data: &[u8]) -> Self {
            Self {
                name: name.to_vec(),
                data: data.to_vec(),
                flags: 0,
                method: METHOD_STORE,
            }
        }
    }

    fn build_raw_zip(entries: &[RawEntry]) -> Vec<u8> {
        let mut output = Vec::new();
        let mut directory = Vec::new();
        let mut offsets = Vec::new();

        for entry in entries {
            let mut crc = Crc::new();
            crc.update(&entry.data);
            let checksum = crc.sum();
            offsets.push(output.len() as u32);

            output.extend_from_slice(&0x0403_4b50_u32.to_le_bytes());
            output.extend_from_slice(&20_u16.to_le_bytes());
            output.extend_from_slice(&entry.flags.to_le_bytes());
            output.extend_from_slice(&entry.method.to_le_bytes());
            output.extend_from_slice(&0_u16.to_le_bytes());
            output.extend_from_slice(&0x21_u16.to_le_bytes());
            output.extend_from_slice(&checksum.to_le_bytes());
            output.extend_from_slice(&(entry.data.len() as u32).to_le_bytes());
            output.extend_from_slice(&(entry.data.len() as u32).to_le_bytes());
            output.extend_from_slice(&(entry.name.len() as u16).to_le_bytes());
            output.extend_from_slice(&0_u16.to_le_bytes());
            output.extend_from_slice(&entry.name);
            output.extend_from_slice(&entry.data);
        }

        for (entry, offset) in entries.iter().zip(&offsets) {
            let mut crc = Crc::new();
            crc.update(&entry.data);
            directory.extend_from_slice(&0x0201_4b50_u32.to_le_bytes());
            directory.extend_from_slice(&20_u16.to_le_bytes());
            directory.extend_from_slice(&20_u16.to_le_bytes());
            directory.extend_from_slice(&entry.flags.to_le_bytes());
            directory.extend_from_slice(&entry.method.to_le_bytes());
            directory.extend_from_slice(&0_u16.to_le_bytes());
            directory.extend_from_slice(&0x21_u16.to_le_bytes());
            directory.extend_from_slice(&crc.sum().to_le_bytes());
            directory.extend_from_slice(&(entry.data.len() as u32).to_le_bytes());
            directory.extend_from_slice(&(entry.data.len() as u32).to_le_bytes());
            directory.extend_from_slice(&(entry.name.len() as u16).to_le_bytes());
            directory.extend_from_slice(&0_u16.to_le_bytes());
            directory.extend_from_slice(&0_u16.to_le_bytes());
            directory.extend_from_slice(&0_u16.to_le_bytes());
            directory.extend_from_slice(&0_u16.to_le_bytes());
            directory.extend_from_slice(&0_u32.to_le_bytes());
            directory.extend_from_slice(&offset.to_le_bytes());
            directory.extend_from_slice(&entry.name);
        }

        let directory_offset = output.len() as u32;
        let directory_size = directory.len() as u32;
        output.extend_from_slice(&directory);
        output.extend_from_slice(&0x0605_4b50_u32.to_le_bytes());
        output.extend_from_slice(&0_u16.to_le_bytes());
        output.extend_from_slice(&0_u16.to_le_bytes());
        output.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        output.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        output.extend_from_slice(&directory_size.to_le_bytes());
        output.extend_from_slice(&directory_offset.to_le_bytes());
        output.extend_from_slice(&0_u16.to_le_bytes());
        output
    }

    // A spec-legal archive can hold two files with the same name (JARs and
    // `zip -g` produce them). Listing two of three entries and handing back
    // the wrong bytes for one of them loses a file with no signal at all.
    #[test]
    fn repeated_names_are_all_listed_and_each_one_extracts_its_own_bytes() {
        let archive = build_raw_zip(&[
            RawEntry::stored(b"notes.txt", b"FIRST-A"),
            RawEntry::stored(b"keepme.txt", b"KEEP"),
            RawEntry::stored(b"notes.txt", b"SECOND-B"),
        ]);

        let report = list_archive(&archive).expect("archive should list");
        assert_eq!(
            report.matches("\"name\":").count(),
            3,
            "every entry in the directory must be listed, got {report}"
        );
        assert!(report.contains("\"duplicate_name\":true"));
        assert!(report.contains("same name"));
        assert_eq!(
            extract_entry(&archive, 0).expect("first entry should extract"),
            b"FIRST-A"
        );
        assert_eq!(
            extract_entry(&archive, 1).expect("second entry should extract"),
            b"KEEP"
        );
        assert_eq!(
            extract_entry(&archive, 2).expect("third entry should extract"),
            b"SECOND-B",
            "the shadowed entry must yield its own contents"
        );
    }

    // Names written as UTF-8 without the UTF-8 flag are the norm; treating
    // them as code page 437 turns every accented filename into mojibake.
    #[test]
    fn filenames_are_decoded_as_utf8_first_and_code_page_437_only_as_a_fallback() {
        let archive = build_raw_zip(&[
            RawEntry::stored("äöü.txt".as_bytes(), b"one"),
            RawEntry::stored(&[0x84, 0x94, 0x81, b'.', b't', b'x', b't'], b"two"),
        ]);
        let report = list_archive(&archive).expect("archive should list");

        assert!(
            report.contains("\"äöü.txt\""),
            "a UTF-8 name must survive, got {report}"
        );
        assert_eq!(
            report.matches("äöü.txt").count(),
            2,
            "a legacy code-page name must decode to the same text, got {report}"
        );
    }

    #[test]
    fn an_unreadable_compression_method_is_flagged_instead_of_hidden() {
        let archive = build_raw_zip(&[
            RawEntry {
                name: b"packed.bin".to_vec(),
                data: b"zstd-ish".to_vec(),
                flags: 0,
                method: 93,
            },
            RawEntry::stored(b"plain.txt", b"fine"),
        ]);

        let report = list_archive(&archive).expect("archive should list");
        assert!(report.contains("\"name\":\"packed.bin\""));
        assert!(report.contains("\"extractable\":false"));
        assert!(report.contains("compression method"));
        assert_eq!(
            extract_entry(&archive, 0).expect_err("an unreadable entry must be refused"),
            UNSUPPORTED_METHOD_ERROR
        );
        assert_eq!(
            extract_entry(&archive, 1).expect("the readable entry must still extract"),
            b"fine"
        );
    }

    #[test]
    fn a_damaged_entry_is_reported_instead_of_silently_handed_over() {
        let mut archive = build_raw_zip(&[RawEntry::stored(b"notes.txt", b"original text")]);
        let at = archive
            .windows(13)
            .position(|window| window == b"original text")
            .expect("fixture data should be present");
        archive[at] = b'X';

        assert_eq!(
            extract_entry(&archive, 0).expect_err("a corrupted entry must be refused"),
            DAMAGED_ENTRY_ERROR
        );
    }

    #[test]
    fn an_empty_archive_lists_as_empty_instead_of_failing() {
        let archive = build_raw_zip(&[]);
        let report = list_archive(&archive).expect("an empty archive is still an archive");
        assert!(report.contains("\"entries\":[]"), "got {report}");
        assert!(extract_entry(&archive, 0).is_err());
    }

    #[test]
    fn zip64_archives_round_trip() {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        let options = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Stored)
            .large_file(true)
            .last_modified_time(fixed_modified_time().expect("fixed timestamp should be valid"));
        writer
            .start_file("big.txt", options)
            .expect("entry should start");
        writer.write_all(b"zip64 body").expect("bytes should write");
        let archive = writer.finish().expect("archive should finish").into_inner();

        let report = list_archive(&archive).expect("zip64 archive should list");
        assert!(report.contains("\"name\":\"big.txt\""));
        assert_eq!(
            extract_entry(&archive, 0).expect("zip64 entry should extract"),
            b"zip64 body"
        );
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
    fn compressible_bytes_use_deflate_and_come_back_byte_identically() {
        let body = vec![b'x'; 16_384];
        let archive_bytes =
            create_archive(vec![("repeat.txt".to_owned(), body.clone())]).expect("should build");
        let mut archive =
            ZipArchive::new(Cursor::new(archive_bytes.clone())).expect("should parse");
        let file = archive.by_index_raw(0).expect("entry should exist");
        assert_eq!(file.compression(), CompressionMethod::Deflated);
        assert!(file.compressed_size() < file.size());
        drop(file);

        // The common case: a compressed entry has to inflate back to exactly
        // what went in, through the shipped extract path.
        assert_eq!(
            extract_entry(&archive_bytes, 0).expect("deflated entry should extract"),
            body
        );
    }

    #[test]
    fn extraction_stops_at_the_cap_even_when_the_declared_size_is_false() {
        let archive = lie_about_first_entry_size(raw_archive(&[("large.txt", &[b'z'; 128])]), 1);
        let error = extract_entry_with_limit(&archive, 0, 64)
            .expect_err("entry should exceed the test cap");
        assert_eq!(error, ENTRY_TOO_LARGE_ERROR);
    }

    // The public entry point must carry the shipped cap, not whatever a test
    // passes in: rewiring `extract_entry` to an unlimited cap has to fail here.
    #[test]
    fn the_public_extract_path_uses_the_shipped_entry_cap() {
        let over_cap = u32::try_from(MAX_ARCHIVE_ENTRY_BYTES + 1).expect("cap fits in a u32");
        let archive = lie_about_first_entry_size(raw_archive(&[("huge.txt", b"small")]), over_cap);
        assert_eq!(
            extract_entry(&archive, 0).expect_err("an over-cap entry must be refused"),
            ENTRY_TOO_LARGE_ERROR
        );
    }

    // A big archive is not a reason to refuse a small file inside it. This is
    // the goal the user is after: click Download, get the file.
    #[test]
    fn a_small_entry_extracts_from_an_archive_with_a_huge_declared_total() {
        let mut archive = raw_archive(&[("tiny.txt", b"five!"), ("huge.bin", b"x")]);
        // Restate the second entry's uncompressed size as 600 MB, which is
        // what a photo archive's real total looks like.
        let second_central = archive
            .windows(4)
            .enumerate()
            .filter(|(_, window)| *window == b"PK\x01\x02")
            .map(|(at, _)| at)
            .nth(1)
            .expect("fixture should have two central headers");
        archive[second_central + 24..second_central + 28]
            .copy_from_slice(&600_000_000_u32.to_le_bytes());

        let report = list_archive(&archive).expect("archive should list");
        assert!(
            report.contains("\"total_size\":600000005"),
            "listing must state the real total, got {report}"
        );
        assert_eq!(
            extract_entry(&archive, 0).expect("a small entry must still extract"),
            b"five!"
        );
    }

    #[test]
    fn listing_flags_unsafe_paths() {
        let archive = raw_archive(&[
            ("../evil.txt", b"one"),
            ("/abs.txt", b"two"),
            ("a/b.txt", b"three"),
            ("C:\\escape.txt", b"four"),
            ("a\\..\\..\\windows.txt", b"five"),
            ("C:relative.txt", b"six"),
        ]);
        let report = list_archive(&archive).expect("archive should list");
        // Backslash-separated traversal and drive-relative paths escape the
        // extraction folder just as surely as `../` and `C:\\` do.
        assert!(report.contains(
            "\"name\":\"a\\\\..\\\\..\\\\windows.txt\",\"size\":4,\"compressed\":4,\"is_dir\":false,\"unsafe_path\":true"
        ));
        assert!(report.contains(
            "\"name\":\"C:relative.txt\",\"size\":3,\"compressed\":3,\"is_dir\":false,\"unsafe_path\":true"
        ));
        assert!(report.contains("\"name\":\"../evil.txt\""));
        assert!(report.contains("\"name\":\"../evil.txt\",\"size\":3,\"compressed\":3,\"is_dir\":false,\"unsafe_path\":true"));
        assert!(report.contains("\"name\":\"/abs.txt\",\"size\":3,\"compressed\":3,\"is_dir\":false,\"unsafe_path\":true"));
        assert!(report.contains("\"name\":\"a/b.txt\",\"size\":5,\"compressed\":5,\"is_dir\":false,\"unsafe_path\":false"));
        assert!(report.contains("\"name\":\"C:\\\\escape.txt\",\"size\":4,\"compressed\":4,\"is_dir\":false,\"unsafe_path\":true"));
    }

    // One locked file must not lock the whole archive: it is listed, flagged,
    // and refused on its own, while its neighbours still extract.
    #[test]
    fn one_encrypted_entry_does_not_block_the_rest_of_the_archive() {
        let archive = mark_first_entry_encrypted(raw_archive(&[
            ("secret.txt", b"classified"),
            ("public.txt", b"open"),
        ]));

        let report = list_archive(&archive).expect("listing must still work");
        assert!(report.contains("\"name\":\"secret.txt\""));
        assert!(report.contains("\"encrypted\":true"));
        assert!(report.contains("password-protected"));
        assert_eq!(
            extract_entry(&archive, 0).expect_err("the locked entry must be refused"),
            ENCRYPTED_ENTRY_ERROR
        );
        assert_eq!(
            extract_entry(&archive, 1).expect("the unlocked entry must still extract"),
            b"open"
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
