use std::io::{Cursor, Read};

use crate::error::AppError;
use rbx_types::Variant;

pub const PREFIX: &str = "require(game:WaitForChild(\"native_legacy\"))(getfenv());\n";

pub fn sandbox(input: &[u8]) -> Result<Vec<u8>, AppError> {
    let decompressed;
    let input = if input.starts_with(&[0x1f, 0x8b]) {
        let mut decoder = flate2::read::GzDecoder::new(input);
        let mut bytes = Vec::new();
        decoder
            .by_ref()
            .take((20 * 1024 * 1024 + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|e| AppError::InvalidModel(format!("invalid gzip asset: {e}")))?;
        if bytes.len() > 20 * 1024 * 1024 {
            return Err(AppError::TooLarge);
        }
        decompressed = bytes;
        decompressed.as_slice()
    } else {
        input
    };
    let trimmed = input.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(input);
    let trimmed = trimmed
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .map(|start| &trimmed[start..])
        .unwrap_or(trimmed);
    let is_xml = trimmed.starts_with(b"<?xml")
        || (trimmed.starts_with(b"<roblox") && !trimmed.starts_with(b"<roblox!"));
    let mut dom = if is_xml {
        rbx_xml::from_reader_default(Cursor::new(trimmed))
            .map_err(|e| AppError::InvalidModel(e.to_string()))?
    } else {
        rbx_binary::from_reader(Cursor::new(input))
            .map_err(|e| AppError::InvalidModel(e.to_string()))?
    };
    let refs: Vec<_> = dom
        .descendants()
        .map(|instance| instance.referent())
        .collect();
    for referent in refs {
        let Some(instance) = dom.get_by_ref_mut(referent) else {
            continue;
        };
        if matches!(
            instance.class.as_str(),
            "Script" | "LocalScript" | "ModuleScript"
        ) {
            let source_key = "Source".into();
            let source = instance.properties.get(&source_key).ok_or_else(|| {
                AppError::InvalidModel(format!("{} has no Source property", instance.class))
            })?;
            let text = match source {
                Variant::String(value) => value.clone(),
                _ => {
                    return Err(AppError::InvalidModel(format!(
                        "{} has an invalid Source property",
                        instance.class
                    )))
                }
            };
            instance
                .properties
                .insert(source_key, Variant::String(format!("{PREFIX}{text}")));
        }
    }
    let roots: Vec<_> = dom.root().children().to_vec();
    let mut output = Vec::new();
    rbx_binary::to_writer(&mut output, &dom, &roots)
        .map_err(|e| AppError::InvalidModel(e.to_string()))?;
    if output.len() > 20 * 1024 * 1024 {
        return Err(AppError::TooLarge);
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rbx_dom_weak::{InstanceBuilder, WeakDom};

    #[test]
    fn prepends_all_script_types_and_preserves_other_instances() {
        let dom = WeakDom::new(InstanceBuilder::new("DataModel").with_children([
            InstanceBuilder::new("Script").with_property("Source", "print('server')"),
            InstanceBuilder::new("LocalScript").with_property("Source", "λ = 1"),
            InstanceBuilder::new("ModuleScript").with_property("Source", ""),
            InstanceBuilder::new("Part").with_name("untouched"),
        ]));
        let mut bytes = Vec::new();
        rbx_binary::to_writer(&mut bytes, &dom, dom.root().children()).unwrap();
        let transformed = sandbox(&bytes).unwrap();
        let parsed = rbx_binary::from_reader(Cursor::new(transformed)).unwrap();
        let scripts: Vec<_> = parsed
            .descendants()
            .filter(|i| matches!(i.class.as_str(), "Script" | "LocalScript" | "ModuleScript"))
            .collect();
        assert_eq!(scripts.len(), 3);
        for script in scripts {
            let key = "Source".into();
            let Variant::String(source) = &script.properties[&key] else {
                panic!("wrong source type")
            };
            assert!(source.starts_with(PREFIX));
        }
        assert!(parsed.descendants().any(|i| i.class == "Part"));
    }

    #[test]
    fn accepts_xml_and_outputs_binary() {
        let input = br#"<roblox version="4"><Item class="Script" referent="RBX1"><Properties><string name="Name">Script</string><ProtectedString name="Source">print('xml')</ProtectedString></Properties></Item></roblox>"#;
        let transformed = sandbox(input).unwrap();
        let parsed = rbx_binary::from_reader(Cursor::new(transformed)).unwrap();
        let script = parsed.descendants().find(|i| i.class == "Script").unwrap();
        let Variant::String(source) = &script.properties[&"Source".into()] else {
            panic!("wrong source type")
        };
        assert_eq!(source, &format!("{PREFIX}print('xml')"));
    }

    #[test]
    fn accepts_gzipped_xml() {
        use flate2::{write::GzEncoder, Compression};
        use std::io::Write;

        let input = br#"<roblox version="4"><Item class="Part" referent="RBX1"><Properties><string name="Name">Part</string></Properties></Item></roblox>"#;
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(input).unwrap();

        let transformed = sandbox(&encoder.finish().unwrap()).unwrap();
        let parsed = rbx_binary::from_reader(Cursor::new(transformed)).unwrap();
        assert!(parsed
            .descendants()
            .any(|instance| instance.class == "Part"));
    }
}
