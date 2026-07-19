use wasm_bindgen::prelude::*;

/// Return the exact version of the compiled core.
#[wasm_bindgen]
pub fn core_version() -> String {
    env!("CARGO_PKG_VERSION").to_owned()
}

/// Pure core: parse a PDF from memory and return its page count. Returns a
/// String error (never panics on malformed input) so it is testable natively —
/// JsValue only exists inside wasm, so the error type must not cross into tests.
fn page_count(bytes: &[u8]) -> Result<u32, String> {
    let document =
        lopdf::Document::load_mem(bytes).map_err(|error| format!("Could not read this PDF: {error}"))?;

    u32::try_from(document.get_pages().len())
        .map_err(|_| "This PDF has too many pages to count.".to_owned())
}

/// Parse a PDF from memory and return its number of pages.
#[wasm_bindgen]
pub fn pdf_page_count(bytes: &[u8]) -> Result<u32, JsValue> {
    page_count(bytes).map_err(|error| JsValue::from_str(&error))
}

#[cfg(test)]
mod tests {
    use super::page_count;
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
        for bytes in [b"not a pdf at all".as_slice(), b"%PDF-1.7\ngarbage".as_slice(), b"".as_slice()] {
            assert!(page_count(bytes).is_err(), "garbage must be Err, not panic/Ok");
        }
    }
}
