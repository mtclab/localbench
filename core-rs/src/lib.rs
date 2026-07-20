use lopdf::dictionary;
use wasm_bindgen::prelude::*;

const ENCRYPTED_PDF_ERROR: &str = "This PDF is password-protected, so its pages can't be read.";

/// Return the exact version of the compiled core.
#[wasm_bindgen]
pub fn core_version() -> String {
    env!("CARGO_PKG_VERSION").to_owned()
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

/// Pure core: combine PDFs in the supplied order and return the serialized PDF.
fn merge(mut docs: Vec<Vec<u8>>) -> Result<Vec<u8>, String> {
    if docs.is_empty() {
        return Err("Choose at least one PDF to merge.".to_owned());
    }

    if docs.len() == 1 {
        let bytes = docs
            .pop()
            .ok_or_else(|| "Choose at least one PDF to merge.".to_owned())?;
        load_pdf(&bytes)?;
        return Ok(bytes);
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

    for document in &mut source_documents {
        let object_count = u32::try_from(document.objects.len())
            .map_err(|_| "These PDFs contain too many objects to merge.".to_owned())?;
        let following_object_id = next_object_id
            .checked_add(object_count)
            .ok_or_else(|| "These PDFs contain too many objects to merge.".to_owned())?;

        document.renumber_objects_with(next_object_id);
        next_object_id = following_object_id;

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
    output.objects.insert(
        catalog_id,
        lopdf::Object::Dictionary(lopdf::dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        }),
    );
    output.max_id = next_object_id - 1;
    output.trailer.set("Root", catalog_id);

    let mut bytes = Vec::new();
    output
        .save_to(&mut bytes)
        .map_err(|error| format!("Could not create the merged PDF: {error}"))?;
    Ok(bytes)
}

/// Parse a PDF from memory and return its number of pages.
#[wasm_bindgen]
pub fn pdf_page_count(bytes: &[u8]) -> Result<u32, JsValue> {
    page_count(bytes).map_err(|error| JsValue::from_str(&error))
}

/// Combine PDFs in array order and return the serialized PDF bytes.
#[wasm_bindgen]
pub fn merge_pdfs(docs: js_sys::Array) -> Result<Vec<u8>, JsValue> {
    let docs = docs
        .iter()
        .map(|bytes| js_sys::Uint8Array::new(&bytes).to_vec())
        .collect();
    merge(docs).map_err(|error| JsValue::from_str(&error))
}

#[cfg(test)]
mod tests {
    use super::{merge, page_count, ENCRYPTED_PDF_ERROR};
    use lopdf::{dictionary, Document, Object};

    fn one_page_pdf() -> Vec<u8> {
        let mut document = Document::with_version("1.5");
        let pages_id = document.new_object_id();
        let page_id = document.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "MediaBox" => vec![0.into(), 0.into(), 200.into(), 200.into()],
            "Resources" => dictionary! {},
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
        });
        document.trailer.set("Root", catalog_id);

        let mut bytes = Vec::new();
        document
            .save_to(&mut bytes)
            .expect("in-memory PDF should serialize");
        bytes
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
        let merged = merge(vec![one_page_pdf(), one_page_pdf()]).expect("PDFs should merge");

        assert_eq!(page_count(&merged).expect("merged PDF should parse"), 2);
        let merged_document = Document::load_mem(&merged).expect("merged PDF should load");
        let page_ids = merged_document.get_pages().into_values().collect::<Vec<_>>();
        assert_ne!(page_ids[0], page_ids[1], "source page IDs must not collide");
    }

    #[test]
    fn rejects_an_empty_merge() {
        assert!(merge(Vec::new()).is_err());
    }

    #[test]
    fn rejects_an_encrypted_merge_input() {
        let mut encrypted = Document::load_mem(&one_page_pdf()).expect("fixture should parse");
        encrypted.trailer.set("Encrypt", dictionary! {});
        let mut encrypted_bytes = Vec::new();
        encrypted
            .save_to(&mut encrypted_bytes)
            .expect("encrypted marker fixture should serialize");

        let error = merge(vec![one_page_pdf(), encrypted_bytes])
            .expect_err("encrypted input must be rejected");
        assert!(error.contains(ENCRYPTED_PDF_ERROR));
    }
}
