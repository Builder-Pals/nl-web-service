use std::io::Cursor;

use crate::error::AppError;
use rbx_types::Variant;

pub const PREFIX: &str = "require(game:WaitForChild(\"native_legacy\"))(getfenv());\n";

pub fn sandbox(input: &[u8]) -> Result<Vec<u8>, AppError> {
    if input.starts_with(b"<?xml")
        || (input.starts_with(b"<roblox") && !input.starts_with(b"<roblox!"))
    {
        return Err(AppError::InvalidModel("XML models are not accepted".into()));
    }
    let mut dom = rbx_binary::from_reader(Cursor::new(input))
        .map_err(|e| AppError::InvalidModel(e.to_string()))?;
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
    fn rejects_xml() {
        assert!(matches!(
            sandbox(b"<roblox />"),
            Err(AppError::InvalidModel(_))
        ));
    }
}
