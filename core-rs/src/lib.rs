use jpeg_encoder::{ColorType, Encoder};
use lopdf::{dictionary, Object};
use wasm_bindgen::prelude::*;
use zune_jpeg::{
    zune_core::{colorspace::ColorSpace, options::DecoderOptions},
    JpegDecoder,
};

mod archive_ops;
mod image_ops;
mod imagepdf_ops;
mod metadata_ops;

pub use archive_ops::{create_zip, extract_zip_entry, list_zip};
pub use image_ops::{compress_image, convert_image, resize_image};
pub use imagepdf_ops::images_to_pdf;
pub use metadata_ops::{inspect_metadata, scrub_metadata, verify_metadata_removed};

const ENCRYPTED_PDF_ERROR: &str = "This PDF is password-protected, so its pages can't be read.";
const MAX_DECODED_PIXELS: usize = 64_000_000;

/// Return the exact version of the compiled core.
#[wasm_bindgen]
pub fn core_version() -> String {
    env!("CARGO_PKG_VERSION").to_owned()
}

/// One statement about what an operation did to the user's file that the file
/// itself cannot show: something changed, something could not be carried over,
/// or something the interface promises did not actually happen.
///
/// Every notice is meant to be displayed. The core never changes a file in a
/// way a user would not expect without returning a notice saying so.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Notice {
    code: &'static str,
    message: String,
}

impl Notice {
    pub(crate) fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub(crate) fn code(&self) -> &'static str {
        self.code
    }
}

/// Add a notice unless the same code is already present, so a repeated
/// condition (ten images with the same problem) reads as one statement.
pub(crate) fn push_notice(notices: &mut Vec<Notice>, notice: Notice) {
    if !notices.iter().any(|existing| existing.code == notice.code) {
        notices.push(notice);
    }
}

/// The bytes an operation produced plus every notice the interface must show.
///
/// `bytes` is the file. `notices` are human-readable sentences; `notice_codes`
/// are the matching stable identifiers, in the same order, for interfaces that
/// want to style or group them.
#[wasm_bindgen]
pub struct FileResult {
    bytes: Vec<u8>,
    notices: Vec<Notice>,
}

impl FileResult {
    pub(crate) fn new(bytes: Vec<u8>, notices: Vec<Notice>) -> Self {
        Self { bytes, notices }
    }

    /// Test/native accessor: the stable notice codes in order.
    #[cfg(test)]
    pub(crate) fn codes(&self) -> Vec<&'static str> {
        self.notices.iter().map(|notice| notice.code).collect()
    }

    /// Test/native accessor: the bytes without cloning them.
    #[cfg(test)]
    pub(crate) fn bytes_ref(&self) -> &[u8] {
        &self.bytes
    }

    /// Test/native accessor: the user-facing messages in order.
    #[cfg(test)]
    pub(crate) fn messages(&self) -> Vec<&str> {
        self.notices
            .iter()
            .map(|notice| notice.message.as_str())
            .collect()
    }
}

#[wasm_bindgen]
impl FileResult {
    /// The produced file's bytes.
    #[wasm_bindgen(getter)]
    pub fn bytes(&self) -> Vec<u8> {
        self.bytes.clone()
    }

    /// Sentences to show the user about this result. May be empty.
    #[wasm_bindgen(getter)]
    pub fn notices(&self) -> Vec<String> {
        self.notices
            .iter()
            .map(|notice| notice.message.clone())
            .collect()
    }

    /// Stable identifiers matching `notices`, in the same order.
    #[wasm_bindgen(getter)]
    pub fn notice_codes(&self) -> Vec<String> {
        self.notices
            .iter()
            .map(|notice| notice.code.to_owned())
            .collect()
    }
}

/// Pure core: parse a PDF from memory and return its page count. Returns a
/// String error (never panics on malformed input) so it is testable natively —
/// JsValue only exists inside wasm, so the error type must not cross into tests.
fn page_count(bytes: &[u8]) -> Result<u32, String> {
    let document = load_pdf(bytes)?;

    u32::try_from(document.get_pages().len())
        .map_err(|_| "This PDF has too many pages to count.".to_owned())
}

/// Parse a PDF and reject encryption before any operation reads its page tree.
fn load_pdf(bytes: &[u8]) -> Result<lopdf::Document, String> {
    let document = lopdf::Document::load_mem(bytes)
        .map_err(|error| format!("Could not read this PDF: {error}"))?;

    // Encrypted PDFs have an unreadable page tree -> lopdf reports 0 pages, which
    // would be a silently wrong answer. Detect it and say so honestly instead.
    if document.trailer.get(b"Encrypt").is_ok() {
        return Err(ENCRYPTED_PDF_ERROR.to_owned());
    }

    Ok(document)
}

/// Find a page attribute, following its parent chain when the value is inherited.
fn inherited_page_attribute(
    document: &lopdf::Document,
    page_id: lopdf::ObjectId,
    key: &[u8],
) -> Option<lopdf::Object> {
    let mut current_id = page_id;
    let mut seen = std::collections::HashSet::new();

    while seen.insert(current_id) {
        let dictionary = document.get_dictionary(current_id).ok()?;
        if let Ok(value) = dictionary.get(key) {
            return Some(value.clone());
        }

        current_id = dictionary
            .get(b"Parent")
            .and_then(lopdf::Object::as_reference)
            .ok()?;
    }

    None
}

/// Follow a chain of indirect references to the object it finally names.
fn resolve<'a>(document: &'a lopdf::Document, object: &'a Object) -> Option<&'a Object> {
    let mut current = object;
    for _ in 0..32 {
        match current {
            Object::Reference(id) => current = document.objects.get(id)?,
            other => return Some(other),
        }
    }
    None
}

/// Return a document's catalog dictionary by value, before its objects move.
fn document_catalog(document: &lopdf::Document) -> Option<lopdf::Dictionary> {
    let root = document.trailer.get(b"Root").ok()?;
    match resolve(document, root)? {
        Object::Dictionary(dictionary) => Some(dictionary.clone()),
        _ => None,
    }
}

/// Catalog entries that describe one specific document and therefore cannot be
/// carried into a document assembled from several sources. Everything not
/// listed here is preserved.
fn merge_dropped_catalog_notice(key: &[u8]) -> Option<Notice> {
    match key {
        b"Metadata" => Some(Notice::new(
            "pdf-xmp-not-merged",
            "Document XMP metadata (title, author, and similar fields) described one source file, so it was not carried into the merged PDF.",
        )),
        b"StructTreeRoot" | b"MarkInfo" => Some(Notice::new(
            "pdf-tags-not-merged",
            "Accessibility tags (the tagged-PDF structure tree) describe one document's reading order and could not be combined, so the merged PDF is untagged. Pages, text, and images are unchanged.",
        )),
        b"Perms" | b"Legal" | b"DSS" => Some(Notice::new(
            "pdf-signature-dropped",
            "Digital-signature information was removed: combining PDFs changes the bytes a signature covers, so the signature could not stay valid.",
        )),
        _ => None,
    }
}

/// Flatten a PDF name tree (`/Names` leaves, `/Kids` branches) into ordered
/// key/value pairs. Returns false when the tree is malformed or too deep, so
/// callers can fall back instead of inventing entries.
fn flatten_name_tree(
    document: &lopdf::Document,
    node: &Object,
    output: &mut Vec<(Vec<u8>, Object)>,
    depth: u32,
) -> bool {
    if depth > 32 {
        return false;
    }
    let Some(Object::Dictionary(dictionary)) = resolve(document, node) else {
        return false;
    };

    let mut recognized = false;
    if let Ok(names) = dictionary.get(b"Names") {
        let Some(Object::Array(entries)) = resolve(document, names) else {
            return false;
        };
        if entries.len() % 2 != 0 {
            return false;
        }
        for pair in entries.chunks(2) {
            let Some(Object::String(key, _)) = resolve(document, &pair[0]) else {
                return false;
            };
            output.push((key.clone(), pair[1].clone()));
        }
        recognized = true;
    }
    if let Ok(kids) = dictionary.get(b"Kids") {
        let Some(Object::Array(kids)) = resolve(document, kids) else {
            return false;
        };
        for kid in kids {
            if !flatten_name_tree(document, kid, output, depth + 1) {
                return false;
            }
        }
        recognized = true;
    }

    recognized
}

/// Flatten a PDF number tree (`/Nums` leaves, `/Kids` branches) the same way.
fn flatten_number_tree(
    document: &lopdf::Document,
    node: &Object,
    output: &mut Vec<(i64, Object)>,
    depth: u32,
) -> bool {
    if depth > 32 {
        return false;
    }
    let Some(Object::Dictionary(dictionary)) = resolve(document, node) else {
        return false;
    };

    let mut recognized = false;
    if let Ok(nums) = dictionary.get(b"Nums") {
        let Some(Object::Array(entries)) = resolve(document, nums) else {
            return false;
        };
        if entries.len() % 2 != 0 {
            return false;
        }
        for pair in entries.chunks(2) {
            let Some(Object::Integer(key)) = resolve(document, &pair[0]) else {
                return false;
            };
            output.push((*key, pair[1].clone()));
        }
        recognized = true;
    }
    if let Ok(kids) = dictionary.get(b"Kids") {
        let Some(Object::Array(kids)) = resolve(document, kids) else {
            return false;
        };
        for kid in kids {
            if !flatten_number_tree(document, kid, output, depth + 1) {
                return false;
            }
        }
        recognized = true;
    }

    recognized
}

/// Build a single-node name tree holding every pair, sorted by key as the
/// spec requires.
fn name_tree(mut entries: Vec<(Vec<u8>, Object)>) -> Object {
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    let mut array = Vec::with_capacity(entries.len() * 2);
    for (key, value) in entries {
        array.push(Object::String(key, lopdf::StringFormat::Literal));
        array.push(value);
    }
    Object::Dictionary(lopdf::dictionary! { "Names" => Object::Array(array) })
}

/// Build a single-node number tree holding every pair in ascending key order.
fn number_tree(mut entries: Vec<(i64, Object)>) -> Object {
    entries.sort_by_key(|(key, _)| *key);
    let mut array = Vec::with_capacity(entries.len() * 2);
    for (key, value) in entries {
        array.push(Object::Integer(key));
        array.push(value);
    }
    Object::Dictionary(lopdf::dictionary! { "Nums" => Object::Array(array) })
}

/// Combine the `/Names` dictionaries of several documents. Each sub-entry
/// (`/Dests`, `/EmbeddedFiles`, `/JavaScript`, …) is a name tree, merged by
/// key with the first document winning a collision.
fn merge_name_dictionaries(
    document: &lopdf::Document,
    catalogs: &[lopdf::Dictionary],
    notices: &mut Vec<Notice>,
) -> Option<Object> {
    let sources = catalogs
        .iter()
        .filter_map(|catalog| catalog.get(b"Names").ok())
        .filter_map(|names| match resolve(document, names) {
            Some(Object::Dictionary(dictionary)) => Some(dictionary.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    if sources.is_empty() {
        return None;
    }

    let mut sub_keys = Vec::new();
    for source in &sources {
        for (key, _) in source.iter() {
            if !sub_keys.contains(key) {
                sub_keys.push(key.clone());
            }
        }
    }

    let mut merged = lopdf::Dictionary::new();
    for sub_key in sub_keys {
        let mut entries: Vec<(Vec<u8>, Object)> = Vec::new();
        let mut usable = true;
        for source in &sources {
            let Ok(tree) = source.get(&sub_key) else {
                continue;
            };
            let mut flattened = Vec::new();
            if !flatten_name_tree(document, tree, &mut flattened, 0) {
                usable = false;
                break;
            }
            for (key, value) in flattened {
                if entries.iter().any(|(existing, _)| *existing == key) {
                    push_notice(notices, Notice::new(
                        "pdf-named-destination-collision",
                        "Two of these PDFs use the same internal link name, so the later one was left out; links inside that file may not jump anywhere.",
                    ));
                    continue;
                }
                entries.push((key, value));
            }
        }

        if usable {
            merged.set(sub_key, name_tree(entries));
        } else if let Some(first) = sources.iter().find_map(|source| source.get(&sub_key).ok()) {
            merged.set(sub_key, first.clone());
            push_notice(notices, Notice::new(
                "pdf-names-not-merged",
                "One PDF stores its named links in a form this tool cannot combine, so only the first file's named links were kept.",
            ));
        }
    }

    if merged.is_empty() {
        None
    } else {
        Some(Object::Dictionary(merged))
    }
}

/// Combine `/PageLabels` number trees, shifting every source document's page
/// indices by where its pages landed in the merged document. Documents without
/// labels get an explicit plain-decimal range so they do not inherit the
/// previous document's labels.
fn merge_page_labels(
    document: &lopdf::Document,
    catalogs: &[lopdf::Dictionary],
    page_offsets: &[i64],
    notices: &mut Vec<Notice>,
) -> Option<Object> {
    if !catalogs.iter().any(|catalog| catalog.has(b"PageLabels")) {
        return None;
    }

    let mut entries: Vec<(i64, Object)> = Vec::new();
    for (catalog, offset) in catalogs.iter().zip(page_offsets) {
        let labels = catalog.get(b"PageLabels").ok();
        let mut flattened = Vec::new();
        let usable = labels
            .map(|labels| flatten_number_tree(document, labels, &mut flattened, 0))
            .unwrap_or(false);

        if !usable {
            if labels.is_some() {
                push_notice(notices, Notice::new(
                    "pdf-page-labels-not-merged",
                    "One PDF stores its page numbering in a form this tool cannot combine, so plain numbering was used for its pages.",
                ));
            }
            entries.push((
                *offset,
                Object::Dictionary(lopdf::dictionary! { "S" => "D" }),
            ));
            continue;
        }

        if !flattened.iter().any(|(key, _)| *key == 0) {
            entries.push((
                *offset,
                Object::Dictionary(lopdf::dictionary! { "S" => "D" }),
            ));
        }
        for (key, value) in flattened {
            entries.push((key.saturating_add(*offset), value));
        }
    }

    Some(number_tree(entries))
}

/// Resolve the page-label dictionary that applies to a zero-based page index,
/// rewriting `/St` so the label keeps the number it had in the source file.
fn page_label_for_index(entries: &[(i64, Object)], index: i64) -> Option<Object> {
    let (range_start, template) = entries
        .iter()
        .filter(|(key, _)| *key <= index)
        .max_by_key(|(key, _)| *key)?;
    let Object::Dictionary(template) = template else {
        return None;
    };

    let mut label = template.clone();
    let start = template
        .get(b"St")
        .and_then(Object::as_i64)
        .unwrap_or(1)
        .saturating_add(index - range_start);
    label.set("St", start);
    Some(Object::Dictionary(label))
}

/// Combine the top-level outline (bookmark) trees of several documents under
/// one new root so every source document's bookmarks survive the merge.
fn merge_outlines(
    document: &mut lopdf::Document,
    catalogs: &[lopdf::Dictionary],
    notices: &mut Vec<Notice>,
) -> Option<Object> {
    struct SourceOutline {
        root: Object,
        first: lopdf::ObjectId,
        last: lopdf::ObjectId,
        top_level: Vec<lopdf::ObjectId>,
        count: i64,
    }

    let mut sources = Vec::new();
    for catalog in catalogs {
        let Ok(outlines) = catalog.get(b"Outlines") else {
            continue;
        };
        let Some(Object::Dictionary(root)) = resolve(document, outlines) else {
            continue;
        };
        let (Ok(first), Ok(last)) = (
            root.get(b"First").and_then(Object::as_reference),
            root.get(b"Last").and_then(Object::as_reference),
        ) else {
            // An outline root without children carries no bookmarks.
            continue;
        };
        let declared_count = root.get(b"Count").and_then(Object::as_i64).unwrap_or(0);

        // Walk the sibling chain so every top-level item can be reparented.
        let mut top_level = Vec::new();
        let mut current = Some(first);
        while let Some(id) = current {
            if top_level.contains(&id) || top_level.len() > 100_000 {
                break;
            }
            top_level.push(id);
            current = document
                .objects
                .get(&id)
                .and_then(|object| object.as_dict().ok())
                .and_then(|item| item.get(b"Next").and_then(Object::as_reference).ok());
        }
        if top_level.last() != Some(&last) {
            push_notice(notices, Notice::new(
                "pdf-bookmarks-not-merged",
                "One PDF's bookmarks are stored in a form this tool cannot combine, so they were left out of the merged file.",
            ));
            continue;
        }

        let count = if declared_count > 0 {
            declared_count
        } else {
            i64::try_from(top_level.len()).unwrap_or(i64::MAX)
        };
        sources.push(SourceOutline {
            root: outlines.clone(),
            first,
            last,
            top_level,
            count,
        });
    }

    let first_source = sources.first()?;
    if sources.len() == 1 {
        // Nothing to splice: reference that document's own outline root, which
        // is already part of the output object graph.
        return Some(first_source.root.clone());
    }

    let root_id = document.new_object_id();
    let first = first_source.first;
    let last = sources.last()?.last;
    let count = sources
        .iter()
        .fold(0_i64, |total, source| total.saturating_add(source.count));

    for index in 0..sources.len() {
        for item_id in sources[index].top_level.clone() {
            if let Some(item) = document
                .objects
                .get_mut(&item_id)
                .and_then(|object| object.as_dict_mut().ok())
            {
                item.set("Parent", root_id);
            }
        }
        // Stitch this document's last top-level item to the next document's
        // first one so the whole chain reads as one list.
        if let Some(next) = sources.get(index + 1) {
            let (previous_last, next_first) = (sources[index].last, next.first);
            if let Some(item) = document
                .objects
                .get_mut(&previous_last)
                .and_then(|object| object.as_dict_mut().ok())
            {
                item.set("Next", next_first);
            }
            if let Some(item) = document
                .objects
                .get_mut(&next_first)
                .and_then(|object| object.as_dict_mut().ok())
            {
                item.set("Prev", previous_last);
            }
        }
    }

    document.objects.insert(
        root_id,
        Object::Dictionary(lopdf::dictionary! {
            "Type" => "Outlines",
            "First" => first,
            "Last" => last,
            "Count" => count,
        }),
    );
    Some(Object::Reference(root_id))
}

/// Combine interactive form definitions so widgets on every merged page keep a
/// live field. Field values, appearance defaults, and resources come from the
/// first form; the field lists are concatenated.
fn merge_acroforms(
    document: &lopdf::Document,
    catalogs: &[lopdf::Dictionary],
    notices: &mut Vec<Notice>,
) -> Option<Object> {
    let forms = catalogs
        .iter()
        .filter_map(|catalog| catalog.get(b"AcroForm").ok())
        .filter_map(|form| match resolve(document, form) {
            Some(Object::Dictionary(dictionary)) => Some(dictionary.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    let mut merged = forms.first()?.clone();
    if forms.len() > 1 {
        push_notice(notices, Notice::new(
            "pdf-forms-combined",
            "These PDFs each contain a fillable form. The forms were combined into one; fields that share a name across files now share a single value.",
        ));
    }

    let mut fields = Vec::new();
    let mut field_names: Vec<Vec<u8>> = Vec::new();
    let mut default_resources = lopdf::Dictionary::new();
    let mut needs_appearances = false;

    for form in &forms {
        if let Some(Object::Array(form_fields)) =
            form.get(b"Fields").ok().and_then(|f| resolve(document, f))
        {
            for field in form_fields {
                if let Some(Object::Dictionary(dictionary)) = resolve(document, field) {
                    if let Ok(Object::String(name, _)) = dictionary.get(b"T") {
                        if field_names.contains(name) {
                            push_notice(notices, Notice::new(
                                "pdf-form-field-name-collision",
                                "Some form fields in different PDFs share the same name. Filling one now fills the other, because a PDF form treats identically named fields as one field.",
                            ));
                        } else {
                            field_names.push(name.clone());
                        }
                    }
                }
                fields.push(field.clone());
            }
        }
        if let Some(Object::Dictionary(resources)) =
            form.get(b"DR").ok().and_then(|dr| resolve(document, dr))
        {
            for (key, value) in resources.iter() {
                match (
                    default_resources.get(key).ok().cloned(),
                    resolve(document, value),
                ) {
                    (Some(Object::Dictionary(mut existing)), Some(Object::Dictionary(added))) => {
                        for (sub_key, sub_value) in added.iter() {
                            if !existing.has(sub_key) {
                                existing.set(sub_key.clone(), sub_value.clone());
                            }
                        }
                        default_resources.set(key.clone(), Object::Dictionary(existing));
                    }
                    (None, _) => {
                        default_resources.set(key.clone(), value.clone());
                    }
                    _ => {}
                }
            }
        }
        needs_appearances |= form
            .get(b"NeedAppearances")
            .and_then(Object::as_bool)
            .unwrap_or(false);
        if form.has(b"SigFlags") || form.has(b"XFA") {
            push_notice(notices, Notice::new(
                "pdf-signature-dropped",
                "Digital-signature information was removed: combining PDFs changes the bytes a signature covers, so the signature could not stay valid.",
            ));
        }
    }

    merged.set("Fields", Object::Array(fields));
    if !default_resources.is_empty() {
        merged.set("DR", Object::Dictionary(default_resources));
    }
    if needs_appearances || forms.len() > 1 {
        // Field appearances were built against their own document's resources.
        // Asking the viewer to rebuild them keeps every field readable.
        merged.set("NeedAppearances", true);
    }
    merged.remove(b"SigFlags");
    merged.remove(b"XFA");
    Some(Object::Dictionary(merged))
}

/// Combine optional-content (layer) definitions so every source document's
/// layers stay switchable in the merged file.
fn merge_optional_content(
    document: &lopdf::Document,
    catalogs: &[lopdf::Dictionary],
) -> Option<Object> {
    let sources = catalogs
        .iter()
        .filter_map(|catalog| catalog.get(b"OCProperties").ok())
        .filter_map(|properties| match resolve(document, properties) {
            Some(Object::Dictionary(dictionary)) => Some(dictionary.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    let mut merged = sources.first()?.clone();

    let concatenate = |key: &[u8], sources: &[lopdf::Dictionary], from_default: bool| {
        let mut combined = Vec::new();
        for source in sources {
            let holder = if from_default {
                match source.get(b"D").ok().and_then(|d| resolve(document, d)) {
                    Some(Object::Dictionary(default)) => default.clone(),
                    _ => continue,
                }
            } else {
                source.clone()
            };
            if let Some(Object::Array(values)) = holder
                .get(key)
                .ok()
                .and_then(|value| resolve(document, value))
            {
                for value in values {
                    if !combined.contains(value) {
                        combined.push(value.clone());
                    }
                }
            }
        }
        combined
    };

    merged.set("OCGs", Object::Array(concatenate(b"OCGs", &sources, false)));
    let mut default = match merged.get(b"D").ok().and_then(|d| resolve(document, d)) {
        Some(Object::Dictionary(dictionary)) => dictionary.clone(),
        _ => lopdf::Dictionary::new(),
    };
    for key in [b"ON".as_slice(), b"OFF", b"Order", b"AS", b"RBGroups"] {
        let values = concatenate(key, &sources, true);
        if !values.is_empty() {
            default.set(key.to_vec(), Object::Array(values));
        }
    }
    merged.set("D", Object::Dictionary(default));
    merged.remove(b"Configs");
    Some(Object::Dictionary(merged))
}

/// Build every catalog entry beyond `/Type` and `/Pages` for a merged
/// document, preserving what can be preserved and stating what cannot.
fn merged_catalog_entries(
    output: &mut lopdf::Document,
    catalogs: &[lopdf::Dictionary],
    page_offsets: &[i64],
    notices: &mut Vec<Notice>,
) -> Vec<(Vec<u8>, Object)> {
    const MERGED_SEPARATELY: [&[u8]; 6] = [
        b"Type",
        b"Pages",
        b"Outlines",
        b"Names",
        b"PageLabels",
        b"AcroForm",
    ];

    let mut entries = Vec::new();
    if let Some(outlines) = merge_outlines(output, catalogs, notices) {
        entries.push((b"Outlines".to_vec(), outlines));
    }
    if let Some(names) = merge_name_dictionaries(output, catalogs, notices) {
        entries.push((b"Names".to_vec(), names));
    }
    if let Some(labels) = merge_page_labels(output, catalogs, page_offsets, notices) {
        entries.push((b"PageLabels".to_vec(), labels));
    }
    if let Some(form) = merge_acroforms(output, catalogs, notices) {
        entries.push((b"AcroForm".to_vec(), form));
    }
    if let Some(optional_content) = merge_optional_content(output, catalogs) {
        entries.push((b"OCProperties".to_vec(), optional_content));
    }

    // Everything else is carried over from the first document that has it,
    // except the entries that describe one specific document.
    for catalog in catalogs {
        for (key, value) in catalog.iter() {
            if MERGED_SEPARATELY.contains(&key.as_slice())
                || key.as_slice() == b"OCProperties"
                || entries.iter().any(|(existing, _)| existing == key)
            {
                continue;
            }
            if let Some(notice) = merge_dropped_catalog_notice(key) {
                push_notice(notices, notice);
                continue;
            }
            entries.push((key.clone(), value.clone()));
        }
    }

    entries
}

/// Pure core: combine PDFs in the supplied order and return the serialized PDF
/// together with everything the user must be told about the combination.
fn merge(mut docs: Vec<Vec<u8>>) -> Result<(Vec<u8>, Vec<Notice>), String> {
    if docs.is_empty() {
        return Err("Choose at least one PDF to merge.".to_owned());
    }

    if docs.len() == 1 {
        let bytes = docs
            .pop()
            .ok_or_else(|| "Choose at least one PDF to merge.".to_owned())?;
        load_pdf(&bytes)?;
        return Ok((bytes, Vec::new()));
    }

    let mut source_documents = docs
        .into_iter()
        .enumerate()
        .map(|(index, bytes)| {
            load_pdf(&bytes).map_err(|error| format!("PDF {}: {error}", index + 1))
        })
        .collect::<Result<Vec<_>, _>>()?;

    let output_version = source_documents
        .iter()
        .map(|document| document.version.as_str())
        .max()
        .unwrap_or("1.5")
        .max("1.5")
        .to_owned();
    let mut output = lopdf::Document::with_version(output_version);

    // Reserve the two active root IDs, then give every source a disjoint,
    // contiguous range. lopdf rewrites references while it renumbers objects.
    let pages_id = output.new_object_id();
    let catalog_id = output.new_object_id();
    let mut next_object_id = output
        .max_id
        .checked_add(1)
        .ok_or_else(|| "These PDFs contain too many objects to merge.".to_owned())?;
    let mut page_ids = Vec::new();
    let mut catalogs = Vec::with_capacity(source_documents.len());
    let mut page_offsets = Vec::with_capacity(source_documents.len());

    for document in &mut source_documents {
        let object_count = u32::try_from(document.objects.len())
            .map_err(|_| "These PDFs contain too many objects to merge.".to_owned())?;
        let following_object_id = next_object_id
            .checked_add(object_count)
            .ok_or_else(|| "These PDFs contain too many objects to merge.".to_owned())?;

        document.renumber_objects_with(next_object_id);
        next_object_id = following_object_id;

        // Read the catalog once the object IDs are final: everything it points
        // at (form, bookmarks, labels, layers) has to survive the merge.
        catalogs.push(document_catalog(document).unwrap_or_default());
        page_offsets.push(i64::try_from(page_ids.len()).unwrap_or(i64::MAX));

        let document_page_ids = document.get_pages().into_values().collect::<Vec<_>>();
        for page_id in &document_page_ids {
            // Resources and page boxes may live on an old Pages ancestor. Copy
            // inherited values onto the leaf before replacing its Parent.
            let inherited = [b"Resources".as_slice(), b"MediaBox", b"CropBox", b"Rotate"]
                .into_iter()
                .filter_map(|key| {
                    inherited_page_attribute(document, *page_id, key)
                        .map(|value| (key.to_vec(), value))
                })
                .collect::<Vec<_>>();

            let page = document
                .get_object_mut(*page_id)
                .and_then(lopdf::Object::as_dict_mut)
                .map_err(|error| format!("Could not read a PDF page: {error}"))?;
            for (key, value) in inherited {
                if !page.has(&key) {
                    page.set(key, value);
                }
            }
            page.set("Parent", pages_id);
        }

        let document_page_id_set = document_page_ids
            .iter()
            .copied()
            .collect::<std::collections::HashSet<_>>();
        for (object_id, object) in std::mem::take(&mut document.objects) {
            let is_page = document_page_id_set.contains(&object_id);
            let is_old_root = matches!(object.type_name(), Ok(b"Catalog" | b"Pages"));
            if is_page || !is_old_root {
                output.objects.insert(object_id, object);
            }
        }
        page_ids.extend(document_page_ids);
    }

    let page_count = i64::try_from(page_ids.len())
        .map_err(|_| "These PDFs contain too many pages to merge.".to_owned())?;
    output.objects.insert(
        pages_id,
        lopdf::Object::Dictionary(lopdf::dictionary! {
            "Type" => "Pages",
            "Kids" => page_ids.into_iter().map(lopdf::Object::Reference).collect::<Vec<_>>(),
            "Count" => page_count,
        }),
    );
    output.max_id = next_object_id - 1;

    // Rebuild the catalog from the sources instead of inventing an empty one:
    // a catalog with only /Type and /Pages would silently throw away the form,
    // the bookmarks, the page numbering, the named links, and the layers.
    let mut notices = Vec::new();
    let mut catalog = lopdf::dictionary! {
        "Type" => "Catalog",
        "Pages" => pages_id,
    };
    for (key, value) in merged_catalog_entries(&mut output, &catalogs, &page_offsets, &mut notices)
    {
        catalog.set(key, value);
    }
    output
        .objects
        .insert(catalog_id, lopdf::Object::Dictionary(catalog));
    output.trailer.set("Root", catalog_id);

    let mut bytes = Vec::new();
    output
        .save_to(&mut bytes)
        .map_err(|error| format!("Could not create the merged PDF: {error}"))?;
    Ok((bytes, notices))
}

/// Replace every reference to a dropped page with null, everywhere in the
/// object graph. Bookmarks and links that pointed at a removed page stop
/// pointing at it, which also lets pruning actually delete the page's content
/// instead of keeping it alive (and readable) through a stale reference.
fn null_references_to(object: &mut Object, removed: &std::collections::HashSet<lopdf::ObjectId>) {
    match object {
        Object::Reference(id) if removed.contains(id) => *object = Object::Null,
        Object::Array(array) => {
            for item in array.iter_mut() {
                null_references_to(item, removed);
            }
        }
        Object::Dictionary(dictionary) => {
            for (_, value) in dictionary.iter_mut() {
                null_references_to(value, removed);
            }
        }
        Object::Stream(stream) => {
            for (_, value) in stream.dict.iter_mut() {
                null_references_to(value, removed);
            }
        }
        _ => {}
    }
}

/// Pure core: keep source pages in the requested output order, adding a
/// clockwise rotation to each one, and return the serialized PDF together with
/// everything the user must be told about the result.
fn organize(
    bytes: &[u8],
    pages: Vec<u32>,
    rotations: Vec<i32>,
) -> Result<(Vec<u8>, Vec<Notice>), String> {
    if pages.is_empty() {
        return Err("Choose at least one page for the output PDF.".to_owned());
    }
    if pages.len() != rotations.len() {
        return Err("Every output page must have one rotation value.".to_owned());
    }
    for (index, rotation) in rotations.iter().enumerate() {
        if !matches!(rotation, 0 | 90 | 180 | 270) {
            return Err(format!(
                "Rotation for output page {} must be 0, 90, 180, or 270 degrees.",
                index + 1
            ));
        }
    }

    let mut source = load_pdf(bytes)?;
    let source_page_count = u32::try_from(source.get_pages().len())
        .map_err(|_| "This PDF has too many pages to organize.".to_owned())?;
    if source_page_count == 0 {
        return Err("This PDF has no pages to organize.".to_owned());
    }
    for page_number in &pages {
        if !(1..=source_page_count).contains(page_number) {
            return Err(format!(
                "Page {page_number} is outside this PDF's page range (1–{source_page_count})."
            ));
        }
    }

    let output_version = source.version.as_str().max("1.5").to_owned();
    let mut output = lopdf::Document::with_version(output_version);
    let pages_id = output.new_object_id();
    let catalog_id = output.new_object_id();
    let first_source_id = output
        .max_id
        .checked_add(1)
        .ok_or_else(|| "This PDF contains too many objects to organize.".to_owned())?;

    // Move every source object into a range above the two new root objects.
    // lopdf updates references, including those inside page resources/streams.
    source.renumber_objects_with(first_source_id);
    let source_pages = source.get_pages();
    let selected_source_ids = pages
        .iter()
        .map(|page_number| {
            source_pages
                .get(page_number)
                .copied()
                .ok_or_else(|| format!("Could not find page {page_number} in this PDF."))
        })
        .collect::<Result<Vec<_>, _>>()?;

    // Once a page is attached to the new flat page tree it loses its old Pages
    // ancestors. Preserve all attributes that the PDF spec allows it to inherit.
    let unique_source_ids = selected_source_ids
        .iter()
        .copied()
        .collect::<std::collections::HashSet<_>>();
    for page_id in &unique_source_ids {
        let inherited = [b"Resources".as_slice(), b"MediaBox", b"CropBox", b"Rotate"]
            .into_iter()
            .filter_map(|key| {
                inherited_page_attribute(&source, *page_id, key).map(|value| (key.to_vec(), value))
            })
            .collect::<Vec<_>>();
        let page = source
            .get_object_mut(*page_id)
            .and_then(lopdf::Object::as_dict_mut)
            .map_err(|error| format!("Could not read a PDF page: {error}"))?;
        for (key, value) in inherited {
            if !page.has(&key) {
                page.set(key, value);
            }
        }
    }

    let mut next_object_id = source
        .max_id
        .checked_add(1)
        .ok_or_else(|| "This PDF contains too many objects to organize.".to_owned())?;
    let mut used_source_ids = std::collections::HashSet::new();
    let mut output_page_ids = Vec::with_capacity(pages.len());
    let mut output_pages = Vec::with_capacity(pages.len());

    for (source_page_id, added_rotation) in selected_source_ids.into_iter().zip(rotations) {
        let mut page = source
            .get_dictionary(source_page_id)
            .map_err(|error| format!("Could not read a PDF page: {error}"))?
            .clone();
        let existing_rotation = match page.get(b"Rotate") {
            Ok(value) => value
                .as_i64()
                .map_err(|_| "A page has an invalid existing rotation value.".to_owned())?,
            Err(_) => 0,
        };
        let rotation = existing_rotation
            .checked_add(i64::from(added_rotation))
            .ok_or_else(|| "A page rotation value is too large.".to_owned())?
            .rem_euclid(360);
        page.set("Parent", pages_id);
        page.set("Rotate", rotation);

        // Reuse a selected leaf the first time. A repeated page gets a fresh
        // leaf ID while continuing to share immutable content/resources.
        let output_page_id = if used_source_ids.insert(source_page_id) {
            source_page_id
        } else {
            let duplicate_id = (next_object_id, 0);
            next_object_id = next_object_id
                .checked_add(1)
                .ok_or_else(|| "This PDF contains too many objects to organize.".to_owned())?;
            duplicate_id
        };
        output_page_ids.push(output_page_id);
        output_pages.push((output_page_id, lopdf::Object::Dictionary(page)));
    }

    // The catalog has to be read before the objects move: it points at the
    // bookmarks, page numbering, form, layers, and named links that a
    // /Type + /Pages-only catalog would throw away.
    let source_catalog = document_catalog(&source).unwrap_or_default();
    let mut notices = Vec::new();
    let mut page_labels = Vec::new();
    let has_page_labels = source_catalog
        .get(b"PageLabels")
        .map(|labels| flatten_number_tree(&source, labels, &mut page_labels, 0))
        .unwrap_or(false);
    if source_catalog.has(b"PageLabels") && !has_page_labels {
        push_notice(&mut notices, Notice::new(
            "pdf-page-labels-dropped",
            "This PDF stores its page numbering in a form this tool cannot rearrange, so the output uses plain page numbers.",
        ));
    }

    let kept_source_ids = used_source_ids.clone();
    let removed_page_ids = source_pages
        .values()
        .copied()
        .filter(|page_id| !kept_source_ids.contains(page_id))
        .collect::<std::collections::HashSet<_>>();
    let keeps_every_page_once =
        removed_page_ids.is_empty() && pages.len() == unique_source_ids.len();
    let is_original_order = keeps_every_page_once
        && pages
            .iter()
            .enumerate()
            .all(|(index, page_number)| *page_number as usize == index + 1);

    // Start with the source object graph so shared resource references remain
    // intact, replace the selected leaves, then prune everything unreachable
    // from the new catalog (including omitted pages and the old page tree).
    for (object_id, object) in std::mem::take(&mut source.objects) {
        let is_old_root = matches!(object.type_name(), Ok(b"Catalog" | b"Pages"));
        if !is_old_root {
            output.objects.insert(object_id, object);
        }
    }
    for (page_id, page) in output_pages {
        output.objects.insert(page_id, page);
    }

    // A bookmark or link pointing at a page the user removed must stop
    // pointing at it, so pruning can really delete that page's content.
    if !removed_page_ids.is_empty() {
        for object in output.objects.values_mut() {
            null_references_to(object, &removed_page_ids);
        }
        push_notice(&mut notices, Notice::new(
            "pdf-links-to-removed-pages",
            "Bookmarks or links that pointed at a page you removed no longer jump anywhere. Every other bookmark and link still works.",
        ));
    }

    let page_count = i64::try_from(output_page_ids.len())
        .map_err(|_| "This PDF has too many output pages.".to_owned())?;
    let source_index_of_output_page = pages
        .iter()
        .map(|page_number| i64::from(*page_number) - 1)
        .collect::<Vec<_>>();
    output.objects.insert(
        pages_id,
        lopdf::Object::Dictionary(lopdf::dictionary! {
            "Type" => "Pages",
            "Kids" => output_page_ids.into_iter().map(lopdf::Object::Reference).collect::<Vec<_>>(),
            "Count" => page_count,
        }),
    );

    let mut catalog = lopdf::dictionary! {
        "Type" => "Catalog",
        "Pages" => pages_id,
    };
    for (key, value) in source_catalog.iter() {
        match key.as_slice() {
            b"Type" | b"Pages" => {}
            b"PageLabels" => {
                if !has_page_labels {
                    continue;
                }
                if is_original_order {
                    catalog.set(key.clone(), value.clone());
                    continue;
                }
                // Pages moved, so an index-keyed label range no longer means
                // what it did. Give every output page the label its source
                // page carried, which survives reordering and duplication.
                let relabelled = source_index_of_output_page
                    .iter()
                    .enumerate()
                    .filter_map(|(output_index, source_index)| {
                        page_label_for_index(&page_labels, *source_index)
                            .map(|label| (i64::try_from(output_index).unwrap_or(i64::MAX), label))
                    })
                    .collect::<Vec<_>>();
                if relabelled.len() == source_index_of_output_page.len() {
                    catalog.set(key.clone(), number_tree(relabelled));
                } else {
                    push_notice(&mut notices, Notice::new(
                        "pdf-page-labels-dropped",
                        "This PDF stores its page numbering in a form this tool cannot rearrange, so the output uses plain page numbers.",
                    ));
                }
            }
            b"StructTreeRoot" | b"MarkInfo" => {
                if is_original_order {
                    catalog.set(key.clone(), value.clone());
                } else {
                    push_notice(&mut notices, Notice::new(
                        "pdf-tags-dropped",
                        "Accessibility tags were removed because they describe the original page order, and keeping them would tell screen readers to read the new file in the old order. Pages, text, and images are unchanged.",
                    ));
                }
            }
            b"Perms" | b"Legal" | b"DSS" => {
                push_notice(&mut notices, Notice::new(
                    "pdf-signature-dropped",
                    "Digital-signature information was removed: rearranging pages changes the bytes a signature covers, so the signature could not stay valid.",
                ));
            }
            _ => {
                catalog.set(key.clone(), value.clone());
            }
        }
    }
    output
        .objects
        .insert(catalog_id, lopdf::Object::Dictionary(catalog));
    output.max_id = next_object_id - 1;
    output.trailer.set("Root", catalog_id);
    output.prune_objects();

    let mut organized = Vec::new();
    output
        .save_to(&mut organized)
        .map_err(|error| format!("Could not create the organized PDF: {error}"))?;
    Ok((organized, notices))
}

#[derive(Clone, Copy)]
enum JpegColor {
    Gray,
    Rgb,
}

impl JpegColor {
    fn components(self) -> usize {
        match self {
            Self::Gray => 1,
            Self::Rgb => 3,
        }
    }

    fn decoder_color_space(self) -> ColorSpace {
        match self {
            Self::Gray => ColorSpace::Luma,
            Self::Rgb => ColorSpace::RGB,
        }
    }

    fn encoder_color_type(self) -> ColorType {
        match self {
            Self::Gray => ColorType::Luma,
            Self::Rgb => ColorType::Rgb,
        }
    }
}

/// Return whether the byte stream announces a baseline sequential DCT frame.
/// Other JPEG modes can also use PDF's DCTDecode filter, but S3 deliberately
/// leaves them untouched rather than changing a mode we have not qualified.
pub(crate) fn is_baseline_jpeg(bytes: &[u8]) -> bool {
    if !bytes.starts_with(&[0xff, 0xd8]) {
        return false;
    }

    let mut position = 2;
    while position < bytes.len() {
        if bytes[position] != 0xff {
            return false;
        }
        while position < bytes.len() && bytes[position] == 0xff {
            position += 1;
        }
        let Some(&marker) = bytes.get(position) else {
            return false;
        };
        position += 1;

        match marker {
            0xc0 => return true,
            // Any other start-of-frame mode is outside this spike's safe scope.
            0xc1..=0xcf if !matches!(marker, 0xc4 | 0xc8 | 0xcc) => return false,
            0xd8 | 0xd9 | 0x01 | 0xd0..=0xd7 => continue,
            0xda => return false,
            _ => {}
        }

        let Some(length_bytes) = bytes.get(position..position + 2) else {
            return false;
        };
        let length = usize::from(u16::from_be_bytes([length_bytes[0], length_bytes[1]]));
        if length < 2 {
            return false;
        }
        let Some(next_position) = position.checked_add(length) else {
            return false;
        };
        if next_position > bytes.len() {
            return false;
        }
        position = next_position;
    }

    false
}

/// Clamp a public quality value into the encoder's meaningful range.
///
/// The clamp lives here, not in the encoder: a caller asking for quality 0 must
/// get the lowest real quality, and the returned value is what the rest of the
/// pipeline reports and tests can check.
fn clamped_quality(quality: u8) -> u8 {
    quality.clamp(1, 100)
}

/// Decode and re-encode one qualified PDF DCT image. Any unsupported or
/// inconsistent image returns None so its original stream remains untouched.
///
/// Pixel dimensions are never changed: a scanned page recompressed here comes
/// back at its original resolution, only more coarsely quantized.
fn reencode_dct_image(stream: &lopdf::Stream, quality: u8) -> Option<(Vec<u8>, u16, u16)> {
    if stream.dict.get(b"Subtype").and_then(Object::as_name).ok() != Some(b"Image")
        || stream.dict.get(b"Filter").and_then(Object::as_name).ok() != Some(b"DCTDecode")
        || !is_baseline_jpeg(&stream.content)
    {
        return None;
    }

    let color = match stream
        .dict
        .get(b"ColorSpace")
        .and_then(Object::as_name)
        .ok()?
    {
        b"DeviceGray" => JpegColor::Gray,
        b"DeviceRGB" => JpegColor::Rgb,
        _ => return None,
    };
    if stream
        .dict
        .get(b"BitsPerComponent")
        .and_then(Object::as_i64)
        .ok()
        != Some(8)
    {
        return None;
    }

    let declared_width =
        u16::try_from(stream.dict.get(b"Width").and_then(Object::as_i64).ok()?).ok()?;
    let declared_height =
        u16::try_from(stream.dict.get(b"Height").and_then(Object::as_i64).ok()?).ok()?;
    let pixel_count = usize::from(declared_width).checked_mul(usize::from(declared_height))?;
    if declared_width == 0 || declared_height == 0 || pixel_count > MAX_DECODED_PIXELS {
        return None;
    }

    let options = DecoderOptions::default().jpeg_set_out_colorspace(color.decoder_color_space());
    let mut decoder = JpegDecoder::new_with_options(stream.content.as_slice(), options);
    let pixels = decoder.decode().ok()?;
    let info = decoder.info()?;
    if info.width != declared_width || info.height != declared_height {
        return None;
    }
    let expected_length = pixel_count.checked_mul(color.components())?;
    if pixels.len() != expected_length {
        return None;
    }

    let (width, height) = (info.width, info.height);
    let mut encoded = Vec::new();
    Encoder::new(&mut encoded, quality)
        .encode(&pixels, width, height, color.encoder_color_type())
        .ok()?;
    Some((encoded, width, height))
}

/// Pure core: recompress qualified baseline-JPEG image XObjects, remove
/// nonessential metadata, compress otherwise-unfiltered streams, and serialize.
/// A no-growth fallback guarantees that a successful result is never larger.
fn compress(bytes: &[u8], quality: u8) -> Result<(Vec<u8>, Vec<Notice>), String> {
    let mut document = load_pdf(bytes)?;
    let source_page_count = document.get_pages().len();
    let quality = clamped_quality(quality);
    let mut notices = Vec::new();
    let had_metadata = document.trailer.get(b"Info").is_ok()
        || document.trailer.get(b"Metadata").is_ok()
        || document.objects.values().any(|object| match object {
            Object::Dictionary(dictionary) => dictionary.has(b"Metadata"),
            Object::Stream(stream) => stream.dict.has(b"Metadata"),
            _ => false,
        });

    for object in document.objects.values_mut() {
        let Object::Stream(stream) = object else {
            continue;
        };
        let Some((encoded, width, height)) = reencode_dct_image(stream, quality) else {
            continue;
        };
        if encoded.len() >= stream.content.len() {
            continue;
        }

        stream.set_content(encoded);
        stream.dict.remove(b"DecodeParms");
        stream.dict.set("Width", i64::from(width));
        stream.dict.set("Height", i64::from(height));
    }

    // Metadata can appear on the catalog, pages, or other dictionaries. Info
    // is optional document metadata, so removing its trailer reference is safe.
    document.trailer.remove(b"Info");
    document.trailer.remove(b"Metadata");
    for object in document.objects.values_mut() {
        match object {
            Object::Dictionary(dictionary) => {
                dictionary.remove(b"Metadata");
            }
            Object::Stream(stream) => {
                stream.dict.remove(b"Metadata");
            }
            _ => {}
        }
    }

    document.prune_objects();
    document.compress();
    let mut compressed = Vec::new();
    document
        .save_to(&mut compressed)
        .map_err(|error| format!("Could not create the compressed PDF: {error}"))?;

    // Re-parse before returning transformed bytes. If serialization changed the
    // page tree or did not reduce total size, preserve the known-good input.
    let valid_and_smaller = compressed.len() < bytes.len()
        && load_pdf(&compressed)
            .map(|output| output.get_pages().len() == source_page_count)
            .unwrap_or(false);
    if !valid_and_smaller {
        // Returning the input unchanged means none of the advertised work
        // actually happened to the file the user downloads. Say so, rather
        // than letting "no size reduction" imply the metadata still went.
        push_notice(
            &mut notices,
            Notice::new(
                "pdf-returned-unchanged",
                if had_metadata {
                    "No size reduction was possible, so your original PDF was returned exactly as it was. Its metadata was NOT removed."
                } else {
                    "No size reduction was possible, so your original PDF was returned exactly as it was."
                },
            ),
        );
        return Ok((bytes.to_vec(), notices));
    }

    if had_metadata {
        push_notice(
            &mut notices,
            Notice::new(
                "pdf-metadata-removed",
                "Document metadata (title, author, producer, and XMP records) was removed from the compressed PDF.",
            ),
        );
    }
    Ok((compressed, notices))
}

/// Parse a PDF from memory and return its number of pages.
#[wasm_bindgen]
pub fn pdf_page_count(bytes: &[u8]) -> Result<u32, JsValue> {
    page_count(bytes).map_err(|error| JsValue::from_str(&error))
}

/// Combine PDFs in array order. Returns the merged bytes plus every notice the
/// interface must show about what could and could not be combined.
#[wasm_bindgen]
pub fn merge_pdfs(docs: js_sys::Array) -> Result<FileResult, JsValue> {
    let docs = docs
        .iter()
        .map(|bytes| js_sys::Uint8Array::new(&bytes).to_vec())
        .collect();
    merge(docs)
        .map(|(bytes, notices)| FileResult::new(bytes, notices))
        .map_err(|error| JsValue::from_str(&error))
}

/// Keep pages in the requested order, adding the parallel rotation values.
/// Returns the bytes plus every notice the interface must show.
#[wasm_bindgen]
pub fn organize_pdf(
    bytes: &[u8],
    pages: Vec<u32>,
    rotations: Vec<i32>,
) -> Result<FileResult, JsValue> {
    organize(bytes, pages, rotations)
        .map(|(bytes, notices)| FileResult::new(bytes, notices))
        .map_err(|error| JsValue::from_str(&error))
}

/// Reduce a PDF's size using only qualified, local pure-Rust codecs. Returns
/// the bytes plus a notice stating what actually happened to the file.
#[wasm_bindgen]
pub fn compress_pdf(bytes: &[u8], quality: u8) -> Result<FileResult, JsValue> {
    compress(bytes, quality)
        .map(|(bytes, notices)| FileResult::new(bytes, notices))
        .map_err(|error| JsValue::from_str(&error))
}

#[cfg(test)]
mod tests {
    use super::{
        clamped_quality, compress, merge, organize, page_count, Notice, ENCRYPTED_PDF_ERROR,
    };

    /// Byte-only wrappers: most tests care about the produced file, while the
    /// notice-carrying signature is exercised by the notice gates below.
    fn merge_bytes(docs: Vec<Vec<u8>>) -> Result<Vec<u8>, String> {
        merge(docs).map(|(bytes, _)| bytes)
    }

    fn merge_notices(docs: Vec<Vec<u8>>) -> Vec<Notice> {
        merge(docs).expect("merge should succeed").1
    }

    fn organize_bytes(
        bytes: &[u8],
        pages: Vec<u32>,
        rotations: Vec<i32>,
    ) -> Result<Vec<u8>, String> {
        organize(bytes, pages, rotations).map(|(bytes, _)| bytes)
    }

    fn organize_notices(bytes: &[u8], pages: Vec<u32>, rotations: Vec<i32>) -> Vec<Notice> {
        organize(bytes, pages, rotations)
            .expect("organize should succeed")
            .1
    }

    fn compress_bytes(bytes: &[u8], quality: u8) -> Result<Vec<u8>, String> {
        compress(bytes, quality).map(|(bytes, _)| bytes)
    }

    fn notice_codes(notices: &[Notice]) -> Vec<&'static str> {
        notices.iter().map(|notice| notice.code).collect()
    }
    use jpeg_encoder::{ColorType, Encoder};
    use lopdf::{dictionary, Document, Object, Stream};

    fn multi_page_pdf(page_count: u32) -> Vec<u8> {
        let mut document = Document::with_version("1.5");
        let pages_id = document.new_object_id();
        let page_ids = (1..=page_count)
            .map(|page_number| {
                document.add_object(dictionary! {
                    "Type" => "Page",
                    "Parent" => pages_id,
                    "MediaBox" => vec![0.into(), 0.into(), 200.into(), 200.into()],
                    "Resources" => dictionary! {},
                    "LocalbenchPageNumber" => page_number,
                })
            })
            .collect::<Vec<_>>();

        document.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => page_ids.into_iter().map(Object::Reference).collect::<Vec<_>>(),
                "Count" => page_count,
                "Rotate" => 90,
            }),
        );
        let catalog_id = document.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        document.trailer.set("Root", catalog_id);

        let mut bytes = Vec::new();
        document
            .save_to(&mut bytes)
            .expect("in-memory PDF should serialize");
        bytes
    }

    fn one_page_pdf() -> Vec<u8> {
        multi_page_pdf(1)
    }

    fn multi_page_pdf_with_contents(page_count: u32) -> Vec<u8> {
        let mut document = Document::with_version("1.5");
        let pages_id = document.new_object_id();
        let page_ids = (1..=page_count)
            .map(|page_number| {
                let content_id = document.add_object(Stream::new(
                    dictionary! {},
                    format!("BT (page-body-{page_number}) Tj ET").into_bytes(),
                ));
                document.add_object(dictionary! {
                    "Type" => "Page",
                    "Parent" => pages_id,
                    "MediaBox" => vec![0.into(), 0.into(), 200.into(), 200.into()],
                    "Resources" => dictionary! {},
                    "Contents" => content_id,
                    "LocalbenchPageNumber" => page_number,
                })
            })
            .collect::<Vec<_>>();

        document.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => page_ids.into_iter().map(Object::Reference).collect::<Vec<_>>(),
                "Count" => page_count,
            }),
        );
        let catalog_id = document.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        document.trailer.set("Root", catalog_id);

        let mut bytes = Vec::new();
        document
            .save_to(&mut bytes)
            .expect("in-memory PDF should serialize");
        bytes
    }

    /// A tersely hand-written PDF with an /Info dictionary, the way a
    /// third-party producer writes one. Rewriting it costs more bytes than
    /// dropping its metadata saves, so compression must fall back to handing
    /// the original file back untouched.
    fn tiny_metadata_pdf() -> Vec<u8> {
        let content = "BT (page-body-1) Tj ET";
        // Many small annotations keep the file object-heavy, which is what
        // makes a rewrite cost more than the metadata it removes.
        let annotation_ids = (6..70)
            .map(|id| format!("{id} 0 R"))
            .collect::<Vec<_>>()
            .join(" ");
        let mut bodies = vec![
            "<</Type/Catalog/Pages 2 0 R>>".to_owned(),
            "<</Type/Pages/Kids[3 0 R]/Count 1>>".to_owned(),
            format!(
                "<</Type/Page/Parent 2 0 R/MediaBox[0 0 200 200]/Resources<<>>/Contents 4 0 R/Annots[{annotation_ids}]>>"
            ),
            format!(
                "<</Length {}>>stream\n{content}\nendstream",
                content.len()
            ),
            "<</Author(Ada Example)/Title(Location scouting)>>".to_owned(),
        ];
        for index in 0..64 {
            bodies.push(format!(
                "<</Type/Annot/Subtype/Square/Rect[0 0 {index} {index}]>>"
            ));
        }

        let mut output = String::from("%PDF-1.5\n");
        let mut offsets = Vec::new();
        for (index, body) in bodies.iter().enumerate() {
            offsets.push(output.len());
            output.push_str(&format!("{} 0 obj{body}endobj\n", index + 1));
        }
        let xref_offset = output.len();
        output.push_str(&format!(
            "xref\n0 {}\n0000000000 65535 f \n",
            bodies.len() + 1
        ));
        for offset in &offsets {
            output.push_str(&format!("{offset:010} 00000 n \n"));
        }
        output.push_str(&format!(
            "trailer<</Size {}/Root 1 0 R/Info 5 0 R>>\nstartxref\n{xref_offset}\n%%EOF\n",
            bodies.len() + 1
        ));
        output.into_bytes()
    }

    /// A two-page PDF carrying everything a catalog can hold that users care
    /// about: a filled form, bookmarks, page labels, named destinations, a
    /// language tag, and a structure tree.
    fn rich_pdf(tag: &str) -> Vec<u8> {
        let mut document = Document::with_version("1.5");
        let pages_id = document.new_object_id();
        let outlines_id = document.new_object_id();
        let page_ids = (1..=2)
            .map(|page_number| {
                let content_id = document.add_object(Stream::new(
                    dictionary! {},
                    format!("BT (marker-{tag}-page-{page_number}) Tj ET").into_bytes(),
                ));
                document.add_object(dictionary! {
                    "Type" => "Page",
                    "Parent" => pages_id,
                    "MediaBox" => vec![0.into(), 0.into(), 200.into(), 200.into()],
                    "Resources" => dictionary! {},
                    "Contents" => content_id,
                })
            })
            .collect::<Vec<_>>();

        let field_id = document.add_object(dictionary! {
            "Type" => "Annot",
            "Subtype" => "Widget",
            "FT" => "Tx",
            "T" => Object::string_literal(format!("field-{tag}")),
            "V" => Object::string_literal(format!("value-{tag}")),
            "Rect" => vec![0.into(), 0.into(), 10.into(), 10.into()],
            "P" => page_ids[0],
        });
        if let Ok(page) = document
            .get_object_mut(page_ids[0])
            .and_then(Object::as_dict_mut)
        {
            page.set("Annots", vec![Object::Reference(field_id)]);
        }
        let acroform_id = document.add_object(dictionary! {
            "Fields" => vec![Object::Reference(field_id)],
            "DA" => Object::string_literal("/Helv 0 Tf 0 g"),
        });

        let second_item_id = document.add_object(dictionary! {
            "Title" => Object::string_literal(format!("bookmark-{tag}-two")),
            "Parent" => outlines_id,
            "Dest" => vec![Object::Reference(page_ids[1]), Object::Name(b"Fit".to_vec())],
        });
        let first_item_id = document.add_object(dictionary! {
            "Title" => Object::string_literal(format!("bookmark-{tag}-one")),
            "Parent" => outlines_id,
            "Next" => second_item_id,
            "Dest" => vec![Object::Reference(page_ids[0]), Object::Name(b"Fit".to_vec())],
        });
        if let Ok(item) = document
            .get_object_mut(second_item_id)
            .and_then(Object::as_dict_mut)
        {
            item.set("Prev", first_item_id);
        }
        document.objects.insert(
            outlines_id,
            Object::Dictionary(dictionary! {
                "Type" => "Outlines",
                "First" => first_item_id,
                "Last" => second_item_id,
                "Count" => 2,
            }),
        );

        let names_id = document.add_object(dictionary! {
            "Dests" => dictionary! {
                "Names" => vec![
                    Object::string_literal(format!("dest-{tag}")),
                    Object::Array(vec![
                        Object::Reference(page_ids[0]),
                        Object::Name(b"Fit".to_vec()),
                    ]),
                ],
            },
        });
        let structure_id = document.add_object(dictionary! {
            "Type" => "StructTreeRoot",
        });

        document.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => page_ids.iter().copied().map(Object::Reference).collect::<Vec<_>>(),
                "Count" => 2,
            }),
        );
        let catalog_id = document.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
            "AcroForm" => acroform_id,
            "Outlines" => outlines_id,
            "Names" => names_id,
            "StructTreeRoot" => structure_id,
            "Lang" => Object::string_literal("fi-FI"),
            "PageLabels" => dictionary! {
                "Nums" => vec![
                    Object::Integer(0),
                    Object::Dictionary(dictionary! { "S" => "r" }),
                    Object::Integer(1),
                    Object::Dictionary(dictionary! { "S" => "D", "St" => 5 }),
                ],
            },
        });
        document.trailer.set("Root", catalog_id);

        let mut bytes = Vec::new();
        document
            .save_to(&mut bytes)
            .expect("rich fixture should serialize");
        bytes
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack
            .windows(needle.len())
            .any(|window| window == needle)
    }

    fn catalog_of(document: &Document) -> lopdf::Dictionary {
        let root = document
            .trailer
            .get(b"Root")
            .expect("output must have a Root");
        resolved_dictionary(document, root)
    }

    fn resolved_dictionary(document: &Document, object: &Object) -> lopdf::Dictionary {
        match super::resolve(document, object).expect("reference should resolve") {
            Object::Dictionary(dictionary) => dictionary.clone(),
            Object::Stream(stream) => stream.dict.clone(),
            other => panic!("expected a dictionary, found {other:?}"),
        }
    }

    fn resolved_array(document: &Document, object: &Object) -> Vec<Object> {
        match super::resolve(document, object).expect("reference should resolve") {
            Object::Array(array) => array.clone(),
            other => panic!("expected an array, found {other:?}"),
        }
    }

    fn flattened_number_tree(document: &Document, object: &Object) -> Vec<(i64, Object)> {
        let mut entries = Vec::new();
        assert!(
            super::flatten_number_tree(document, object, &mut entries, 0),
            "number tree should be readable"
        );
        entries.sort_by_key(|(key, _)| *key);
        entries
    }

    fn flattened_name_tree(document: &Document, object: &Object) -> Vec<(Vec<u8>, Object)> {
        let mut entries = Vec::new();
        assert!(
            super::flatten_name_tree(document, object, &mut entries, 0),
            "name tree should be readable"
        );
        entries
    }

    /// Walk the merged outline's top-level chain and return the titles in order.
    fn top_level_outline_titles(document: &Document) -> Vec<String> {
        let catalog = catalog_of(document);
        let outlines = resolved_dictionary(document, catalog.get(b"Outlines").expect("outlines"));
        let mut titles = Vec::new();
        let mut current = outlines.get(b"First").and_then(Object::as_reference).ok();
        while let Some(id) = current {
            let item = document
                .get_dictionary(id)
                .expect("outline item should exist");
            titles.push(
                String::from_utf8_lossy(
                    item.get(b"Title")
                        .and_then(Object::as_str)
                        .expect("outline item should have a title"),
                )
                .into_owned(),
            );
            if titles.len() > 16 {
                break;
            }
            current = item.get(b"Next").and_then(Object::as_reference).ok();
        }
        titles
    }

    fn jpeg_image_pdf() -> (Vec<u8>, usize) {
        wide_jpeg_image_pdf(640, 480)
    }

    fn wide_jpeg_image_pdf(width: u16, height: u16) -> (Vec<u8>, usize) {
        let mut pixels = Vec::with_capacity(usize::from(width) * usize::from(height) * 3);
        for y in 0..height {
            for x in 0..width {
                // Fine deterministic detail makes the high-quality fixture a
                // meaningful compression target without checking in a binary.
                let noise = ((u32::from(x) * 73 + u32::from(y) * 151) % 251) as u8;
                pixels.extend_from_slice(&[
                    (x % 256) as u8 ^ noise,
                    (y % 256) as u8 ^ noise.rotate_left(2),
                    ((u32::from(x) + u32::from(y)) % 256) as u8 ^ noise.rotate_left(4),
                ]);
            }
        }

        let mut jpeg = Vec::new();
        Encoder::new(&mut jpeg, 96)
            .encode(&pixels, width, height, ColorType::Rgb)
            .expect("fixture JPEG should encode");
        let original_jpeg_size = jpeg.len();

        let mut document = Document::with_version("1.5");
        let pages_id = document.new_object_id();
        let image_id = document.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject",
                "Subtype" => "Image",
                "Width" => i64::from(width),
                "Height" => i64::from(height),
                "ColorSpace" => "DeviceRGB",
                "BitsPerComponent" => 8,
                "Filter" => "DCTDecode",
            },
            jpeg,
        ));
        let content_id = document.add_object(Stream::new(
            dictionary! {},
            format!("q\n{} 0 0 {} 0 0 cm\n/Im0 Do\nQ\n", width, height).into_bytes(),
        ));
        let metadata_id = document.add_object(Stream::new(
            dictionary! {
                "Type" => "Metadata",
                "Subtype" => "XML",
            },
            vec![b'm'; 4_096],
        ));
        let page_id = document.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "MediaBox" => vec![0.into(), 0.into(), i64::from(width).into(), i64::from(height).into()],
            "Resources" => dictionary! {
                "XObject" => dictionary! {
                    "Im0" => image_id,
                },
            },
            "Contents" => content_id,
            "Metadata" => metadata_id,
        });
        document.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![Object::Reference(page_id)],
                "Count" => 1,
            }),
        );
        let catalog_id = document.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
            "Metadata" => metadata_id,
        });
        let info_id = document.add_object(dictionary! {
            "Creator" => Object::string_literal("localbench compression fixture"),
            "Producer" => Object::string_literal("localbench tests"),
        });
        document.trailer.set("Root", catalog_id);
        document.trailer.set("Info", info_id);

        let mut bytes = Vec::new();
        document
            .save_to(&mut bytes)
            .expect("JPEG fixture PDF should serialize");
        (bytes, original_jpeg_size)
    }

    #[test]
    fn counts_pages_in_a_valid_pdf() {
        let pdf = one_page_pdf();
        assert_eq!(page_count(&pdf).expect("PDF should parse"), 1);
    }

    // Malformed input must return Err, never panic: a panic aborts the whole
    // wasm instance and would hang the worker. Guards against garbage PDFs.
    #[test]
    fn rejects_garbage_without_panicking() {
        for bytes in [
            b"not a pdf at all".as_slice(),
            b"%PDF-1.7\ngarbage".as_slice(),
            b"".as_slice(),
        ] {
            assert!(
                page_count(bytes).is_err(),
                "garbage must be Err, not panic/Ok"
            );
        }
    }

    #[test]
    fn merges_two_one_page_pdfs_in_order() {
        let merged = merge_bytes(vec![one_page_pdf(), one_page_pdf()]).expect("PDFs should merge");

        assert_eq!(page_count(&merged).expect("merged PDF should parse"), 2);
        let merged_document = Document::load_mem(&merged).expect("merged PDF should load");
        let page_ids = merged_document
            .get_pages()
            .into_values()
            .collect::<Vec<_>>();
        assert_ne!(page_ids[0], page_ids[1], "source page IDs must not collide");
    }

    #[test]
    fn rejects_an_empty_merge() {
        assert!(merge_bytes(Vec::new()).is_err());
    }

    #[test]
    fn rejects_an_encrypted_merge_input() {
        let mut encrypted = Document::load_mem(&one_page_pdf()).expect("fixture should parse");
        encrypted.trailer.set("Encrypt", dictionary! {});
        let mut encrypted_bytes = Vec::new();
        encrypted
            .save_to(&mut encrypted_bytes)
            .expect("encrypted marker fixture should serialize");

        let error = merge_bytes(vec![one_page_pdf(), encrypted_bytes])
            .expect_err("encrypted input must be rejected");
        assert!(error.contains(ENCRYPTED_PDF_ERROR));
    }

    #[test]
    fn extracts_pages_in_requested_order() {
        let organized = organize_bytes(&multi_page_pdf(3), vec![3, 1], vec![0, 0])
            .expect("selected pages should organize");

        assert_eq!(page_count(&organized).expect("output PDF should parse"), 2);
        let document = Document::load_mem(&organized).expect("output PDF should load");
        let source_numbers = document
            .get_pages()
            .into_values()
            .map(|page_id| {
                document
                    .get_dictionary(page_id)
                    .and_then(|page| page.get(b"LocalbenchPageNumber"))
                    .and_then(Object::as_i64)
                    .expect("fixture page number should remain")
            })
            .collect::<Vec<_>>();
        assert_eq!(source_numbers, vec![3, 1]);
    }

    #[test]
    fn adds_rotation_to_an_inherited_page_rotation() {
        let organized =
            organize_bytes(&multi_page_pdf(3), vec![2], vec![90]).expect("page should rotate");
        let document = Document::load_mem(&organized).expect("output PDF should load");
        let page_id = document.get_pages()[&1];
        let rotation = document
            .get_dictionary(page_id)
            .and_then(|page| page.get(b"Rotate"))
            .and_then(Object::as_i64)
            .expect("output page should have a rotation");
        assert_eq!(rotation, 180);
    }

    #[test]
    fn duplicates_a_selected_page_with_independent_rotations() {
        let organized = organize_bytes(&multi_page_pdf(1), vec![1, 1], vec![0, 90])
            .expect("a source page should be reusable");
        let document = Document::load_mem(&organized).expect("output PDF should load");
        let page_ids = document.get_pages().into_values().collect::<Vec<_>>();
        let rotations = page_ids
            .iter()
            .map(|page_id| {
                document
                    .get_dictionary(*page_id)
                    .and_then(|page| page.get(b"Rotate"))
                    .and_then(Object::as_i64)
                    .expect("output page should have a rotation")
            })
            .collect::<Vec<_>>();
        assert_eq!(page_ids.len(), 2);
        assert_ne!(page_ids[0], page_ids[1]);
        assert_eq!(rotations, vec![90, 180]);
    }

    #[test]
    fn rejects_an_out_of_range_page() {
        assert!(organize_bytes(&multi_page_pdf(3), vec![4], vec![0]).is_err());
    }

    #[test]
    fn rejects_mismatched_organize_arrays() {
        assert!(organize_bytes(&multi_page_pdf(3), vec![1, 2], vec![0]).is_err());
    }

    #[test]
    fn rejects_an_unsupported_rotation() {
        assert!(organize_bytes(&multi_page_pdf(3), vec![1], vec![45]).is_err());
    }

    #[test]
    fn rejects_an_empty_organize_selection() {
        assert!(organize_bytes(&multi_page_pdf(3), Vec::new(), Vec::new()).is_err());
    }

    #[test]
    fn compresses_a_baseline_jpeg_without_changing_page_count() {
        let (source, original_jpeg_size) = jpeg_image_pdf();
        let compressed = compress_bytes(&source, 25).expect("fixture should compress");

        eprintln!(
            "baseline-JPEG fixture: {} bytes -> {} bytes",
            source.len(),
            compressed.len()
        );
        assert!(compressed.len() < source.len());
        assert_eq!(page_count(&compressed).expect("output should parse"), 1);

        let output = Document::load_mem(&compressed).expect("output PDF should load");
        assert!(output.trailer.get(b"Info").is_err());
        let output_image = output
            .objects
            .values()
            .filter_map(|object| object.as_stream().ok())
            .find(|stream| {
                stream.dict.get(b"Subtype").and_then(Object::as_name).ok() == Some(b"Image")
            })
            .expect("output image should remain");
        assert!(output_image.content.len() < original_jpeg_size);
        assert_eq!(
            output_image
                .dict
                .get(b"Filter")
                .and_then(Object::as_name)
                .expect("image filter should remain"),
            b"DCTDecode"
        );
        assert_eq!(
            output_image
                .dict
                .get(b"Width")
                .and_then(Object::as_i64)
                .expect("image width should remain"),
            640
        );
        assert_eq!(
            output_image
                .dict
                .get(b"Height")
                .and_then(Object::as_i64)
                .expect("image height should remain"),
            480
        );
        assert_eq!(
            output_image
                .dict
                .get(b"ColorSpace")
                .and_then(Object::as_name)
                .expect("image color space should remain"),
            b"DeviceRGB"
        );
        assert_eq!(
            output_image
                .dict
                .get(b"BitsPerComponent")
                .and_then(Object::as_i64)
                .expect("image bit depth should remain"),
            8
        );
        assert!(output.objects.values().all(|object| match object {
            Object::Dictionary(dictionary) => !dictionary.has(b"Metadata"),
            Object::Stream(stream) => !stream.dict.has(b"Metadata"),
            _ => true,
        }));
    }

    #[test]
    fn clamps_compression_quality_to_the_public_range() {
        let (source, _) = jpeg_image_pdf();
        assert_eq!(
            compress_bytes(&source, 0).expect("quality zero should clamp"),
            compress_bytes(&source, 1).expect("quality one should work")
        );
        assert_eq!(
            compress_bytes(&source, 101).expect("quality 101 should clamp"),
            compress_bytes(&source, 100).expect("quality 100 should work")
        );
    }

    // Never growing is only half the promise. The pages have to come out with
    // their content intact and in order: a compression that quietly emptied
    // every content stream would also satisfy "no bigger, still two pages".
    #[test]
    fn a_no_image_pdf_keeps_every_page_intact_and_never_grows() {
        let source = multi_page_pdf_with_contents(2);
        let compressed = compress_bytes(&source, 30).expect("no-image PDF should still compress");

        assert!(compressed.len() <= source.len());
        let document = Document::load_mem(&compressed).expect("output should remain valid");
        let pages = document.get_pages();
        assert_eq!(pages.len(), 2);

        for page_number in 1..=2_u32 {
            let page_id = pages[&page_number];
            let page = document
                .get_dictionary(page_id)
                .expect("page dictionary should exist");
            assert_eq!(
                page.get(b"LocalbenchPageNumber")
                    .and_then(Object::as_i64)
                    .expect("page identity should survive"),
                i64::from(page_number),
                "pages must stay in their original order"
            );

            let contents_id = page
                .get(b"Contents")
                .and_then(Object::as_reference)
                .expect("page contents should survive");
            let stream = document
                .get_object(contents_id)
                .and_then(Object::as_stream)
                .expect("content stream should survive");
            let content = if stream.dict.has(b"Filter") {
                stream
                    .decompressed_content()
                    .expect("content should decompress")
            } else {
                stream.content.clone()
            };
            assert_eq!(
                String::from_utf8_lossy(&content),
                format!("BT (page-body-{page_number}) Tj ET"),
                "every page's drawing instructions must survive compression"
            );
        }
    }

    #[test]
    fn leaves_jpeg_2000_image_streams_untouched() {
        let (source, _) = jpeg_image_pdf();
        let mut input = Document::load_mem(&source).expect("fixture should load");
        let input_image = input
            .objects
            .values_mut()
            .filter_map(|object| object.as_stream_mut().ok())
            .find(|stream| {
                stream.dict.get(b"Subtype").and_then(Object::as_name).ok() == Some(b"Image")
            })
            .expect("fixture image should exist");
        input_image.dict.set("Filter", "JPXDecode");
        let original_content = input_image.content.clone();
        let mut input_bytes = Vec::new();
        input
            .save_to(&mut input_bytes)
            .expect("modified fixture should serialize");

        let compressed =
            compress_bytes(&input_bytes, 20).expect("unsupported image should be preserved");
        let output = Document::load_mem(&compressed).expect("output should load");
        let output_image = output
            .objects
            .values()
            .filter_map(|object| object.as_stream().ok())
            .find(|stream| {
                stream.dict.get(b"Subtype").and_then(Object::as_name).ok() == Some(b"Image")
            })
            .expect("unsupported image should remain");
        assert_eq!(
            output_image
                .dict
                .get(b"Filter")
                .and_then(Object::as_name)
                .expect("filter should remain"),
            b"JPXDecode"
        );
        assert_eq!(output_image.content, original_content);
    }

    // A scan is wider than 4096 pixels. Compressing it must recompress the
    // image, never resample it: a downscale here silently destroys the
    // resolution of every page in a scanned document.
    #[test]
    fn compressing_never_shrinks_an_images_pixel_dimensions() {
        const WIDTH: u16 = 5_000;
        const HEIGHT: u16 = 8;
        let (source, _) = wide_jpeg_image_pdf(WIDTH, HEIGHT);
        let compressed = compress_bytes(&source, 20).expect("wide image should compress");
        let output = Document::load_mem(&compressed).expect("output should load");
        let image = output
            .objects
            .values()
            .filter_map(|object| object.as_stream().ok())
            .find(|stream| {
                stream.dict.get(b"Subtype").and_then(Object::as_name).ok() == Some(b"Image")
            })
            .expect("output image should remain");

        assert_eq!(
            image
                .dict
                .get(b"Width")
                .and_then(Object::as_i64)
                .expect("width should remain"),
            i64::from(WIDTH),
            "a compressed page image must keep every pixel column it had"
        );
        assert_eq!(
            image
                .dict
                .get(b"Height")
                .and_then(Object::as_i64)
                .expect("height should remain"),
            i64::from(HEIGHT)
        );
    }

    // The clamp has to live in this core. Asserting that compress(0) equals
    // compress(1) proves nothing, because the encoder clamps internally too.
    #[test]
    fn clamped_quality_pins_the_public_range() {
        assert_eq!(clamped_quality(0), 1);
        assert_eq!(clamped_quality(1), 1);
        assert_eq!(clamped_quality(50), 50);
        assert_eq!(clamped_quality(100), 100);
        assert_eq!(clamped_quality(101), 100);
        assert_eq!(clamped_quality(255), 100);
    }

    // Merging must not silently gut the document: a filled form, bookmarks,
    // page numbering, named links and the language tag all have to arrive.
    #[test]
    fn merging_keeps_the_form_bookmarks_labels_names_and_language() {
        let merged = merge_bytes(vec![rich_pdf("a"), rich_pdf("b")]).expect("PDFs should merge");
        let document = Document::load_mem(&merged).expect("merged PDF should load");
        let catalog = catalog_of(&document);

        for key in [
            b"AcroForm".as_slice(),
            b"Outlines",
            b"PageLabels",
            b"Names",
            b"Lang",
        ] {
            assert!(
                catalog.has(key),
                "merged catalog lost /{}",
                String::from_utf8_lossy(key)
            );
        }

        // The bookmark titles and the filled-in field values are user data; if
        // they are not in the output bytes they are gone.
        for needle in [
            b"bookmark-a-two".as_slice(),
            b"bookmark-b-two",
            b"value-a",
            b"value-b",
        ] {
            assert!(
                contains(&merged, needle),
                "merged PDF lost {}",
                String::from_utf8_lossy(needle)
            );
        }

        let form = resolved_dictionary(&document, catalog.get(b"AcroForm").expect("form"));
        let fields = resolved_array(&document, form.get(b"Fields").expect("fields"));
        assert_eq!(fields.len(), 2, "both documents' form fields must survive");

        let outline_titles = top_level_outline_titles(&document);
        assert_eq!(
            outline_titles.len(),
            4,
            "every source bookmark must be in the merged outline"
        );
        assert!(outline_titles.contains(&"bookmark-b-one".to_owned()));

        let labels = flattened_number_tree(&document, catalog.get(b"PageLabels").expect("labels"));
        assert_eq!(
            labels.iter().map(|(key, _)| *key).collect::<Vec<_>>(),
            vec![0, 1, 2, 3],
            "page numbering must be shifted onto the merged page range"
        );

        let destinations = flattened_name_tree(
            &document,
            resolved_dictionary(&document, catalog.get(b"Names").expect("names"))
                .get(b"Dests")
                .expect("dests"),
        );
        assert_eq!(
            destinations.len(),
            2,
            "both documents' named destinations must survive"
        );
    }

    // Every page of every input must arrive with its drawing instructions.
    #[test]
    fn merging_keeps_every_page_and_its_content() {
        let merged = merge_bytes(vec![rich_pdf("a"), rich_pdf("b")]).expect("PDFs should merge");
        assert_eq!(page_count(&merged).expect("merged PDF should parse"), 4);
        for needle in [
            b"marker-a-page-1".as_slice(),
            b"marker-a-page-2",
            b"marker-b-page-1",
            b"marker-b-page-2",
        ] {
            assert!(
                contains(&merged, needle),
                "merged PDF lost the content of {}",
                String::from_utf8_lossy(needle)
            );
        }
    }

    // Only one input has bookmarks and a form: they still have to arrive.
    #[test]
    fn merging_a_plain_pdf_with_a_rich_one_keeps_the_rich_ones_extras() {
        let merged = merge_bytes(vec![one_page_pdf(), rich_pdf("b")]).expect("PDFs should merge");
        let document = Document::load_mem(&merged).expect("merged PDF should load");
        let catalog = catalog_of(&document);

        assert_eq!(document.get_pages().len(), 3);
        assert!(catalog.has(b"Outlines"), "the only outline was dropped");
        assert!(catalog.has(b"AcroForm"), "the only form was dropped");
        assert_eq!(
            top_level_outline_titles(&document),
            vec!["bookmark-b-one".to_owned(), "bookmark-b-two".to_owned()]
        );

        // The plain document's pages must not inherit the other one's labels.
        let labels = flattened_number_tree(&document, catalog.get(b"PageLabels").expect("labels"));
        assert_eq!(
            labels.iter().map(|(key, _)| *key).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
    }

    #[test]
    fn merging_two_forms_says_so_and_reports_the_tags_it_cannot_combine() {
        let codes = notice_codes(&merge_notices(vec![rich_pdf("a"), rich_pdf("b")]));
        assert!(codes.contains(&"pdf-forms-combined"));
        assert!(codes.contains(&"pdf-tags-not-merged"));
    }

    // Rearranging must keep the catalog too, and page numbering has to follow
    // the pages it belongs to rather than being dropped or left misaligned.
    #[test]
    fn organizing_keeps_the_catalog_and_moves_page_labels_with_their_pages() {
        let organized =
            organize_bytes(&rich_pdf("a"), vec![2, 1], vec![0, 0]).expect("pages should reorder");
        let document = Document::load_mem(&organized).expect("output should load");
        let catalog = catalog_of(&document);

        assert!(catalog.has(b"Outlines"), "reordering lost the bookmarks");
        assert!(catalog.has(b"AcroForm"), "reordering lost the form");
        assert!(catalog.has(b"Lang"), "reordering lost the language tag");
        assert!(
            contains(&organized, b"bookmark-a-one"),
            "the bookmark title is not in the output bytes at all"
        );

        let labels = flattened_number_tree(&document, catalog.get(b"PageLabels").expect("labels"));
        let styles = labels
            .iter()
            .map(|(key, label)| {
                let label = resolved_dictionary(&document, label);
                (
                    *key,
                    String::from_utf8_lossy(
                        label.get(b"S").and_then(Object::as_name).unwrap_or(b"?"),
                    )
                    .into_owned(),
                )
            })
            .collect::<Vec<_>>();
        // Source page 2 was decimal, source page 1 was roman; swapping the
        // pages must swap their labels with them.
        assert_eq!(
            styles,
            vec![(0, "D".to_owned()), (1, "r".to_owned())],
            "page labels must follow their pages"
        );
    }

    // A removed page's content has to leave the file, and the bookmark that
    // pointed at it must stop pointing at it rather than resurrect it.
    #[test]
    fn a_removed_page_leaves_no_content_and_no_live_link_behind() {
        let organized =
            organize_bytes(&rich_pdf("a"), vec![1], vec![0]).expect("page should be kept");

        assert!(
            !contains(&organized, b"marker-a-page-2"),
            "the removed page's content is still inside the output file"
        );
        let document = Document::load_mem(&organized).expect("output should load");
        assert_eq!(document.get_pages().len(), 1);
        assert!(
            contains(&organized, b"bookmark-a-one"),
            "the surviving bookmark was destroyed along with the removed page"
        );

        let dangling = document.objects.values().any(|object| {
            object
                .as_dict()
                .ok()
                .and_then(|dictionary| dictionary.get(b"Dest").ok())
                .and_then(|destination| destination.as_array().ok())
                .is_some_and(|destination| matches!(destination.first(), Some(Object::Null)))
        });
        assert!(
            dangling,
            "the bookmark for the removed page must point at nothing"
        );
    }

    #[test]
    fn organizing_reports_the_links_it_could_not_keep() {
        let codes = notice_codes(&organize_notices(&rich_pdf("a"), vec![1], vec![0]));
        assert!(codes.contains(&"pdf-links-to-removed-pages"));
        assert!(codes.contains(&"pdf-tags-dropped"));
    }

    // Returning the input unchanged is a legitimate outcome, but the caller
    // has to learn that nothing the interface promised actually happened.
    #[test]
    fn compress_states_when_it_hands_back_the_original_untouched() {
        let source = tiny_metadata_pdf();
        let (bytes, notices) = compress(&source, 40).expect("compress should succeed");

        assert_eq!(bytes, source, "the fixture cannot shrink");
        assert!(
            notice_codes(&notices).contains(&"pdf-returned-unchanged"),
            "a returned-as-is result must say so, got {:?}",
            notice_codes(&notices)
        );
        assert!(
            notices
                .iter()
                .any(|notice| notice.message.contains("NOT removed")),
            "the notice must contradict the interface's metadata promise"
        );
    }

    // The wasm-facing surface the interfaces consume: bytes plus two parallel
    // arrays. If they ever drift apart, a notice ends up attached to the wrong
    // code and the interface shows the wrong thing.
    #[test]
    fn a_file_result_exposes_bytes_and_matching_notice_codes() {
        let result = super::FileResult::new(
            b"pdf-bytes".to_vec(),
            vec![
                Notice::new("first-code", "First message."),
                Notice::new("second-code", "Second message."),
            ],
        );

        assert_eq!(result.bytes(), b"pdf-bytes".to_vec());
        assert_eq!(result.bytes_ref(), b"pdf-bytes");
        assert_eq!(result.notice_codes(), vec!["first-code", "second-code"]);
        assert_eq!(result.codes(), vec!["first-code", "second-code"]);
        assert_eq!(
            result.notices(),
            vec!["First message.".to_owned(), "Second message.".to_owned()]
        );
        assert_eq!(result.messages(), vec!["First message.", "Second message."]);
        assert_eq!(
            result.notices().len(),
            result.notice_codes().len(),
            "messages and codes must stay parallel"
        );
    }

    #[test]
    fn compress_states_when_it_really_did_remove_metadata() {
        let (source, _) = jpeg_image_pdf();
        let (bytes, notices) = compress(&source, 25).expect("fixture should compress");

        assert!(bytes.len() < source.len());
        assert!(notice_codes(&notices).contains(&"pdf-metadata-removed"));
    }
}
