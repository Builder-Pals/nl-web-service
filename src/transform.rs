use std::{
    borrow::Cow,
    io::{Cursor, Read},
};

use crate::error::AppError;
use rbx_dom_weak::{InstanceBuilder, WeakDom};
use rbx_reflection::{DataType, PropertyKind, PropertySerialization, Scriptability};
use rbx_types::{Attributes, Ref, Tags, Variant};

pub const PREFIX: &str = "require(game:WaitForChild(\"native_legacy\"))(getfenv());\n";
const GAME_PREFIX: &str = "require(game:WaitForChild(\"native_legacy\"))(getfenv());";
const LIMIT: usize = 20 * 1024 * 1024;
const TARGET_SERVICES: &[&str] = &[
    "Workspace",
    "Players",
    "Lighting",
    "ReplicatedStorage",
    "ReplicatedFirst",
    "ServerScriptService",
    "ServerStorage",
    "StarterGui",
    "StarterPack",
    "StarterPlayer",
    "Chat",
    "Teams",
    "SoundService",
];

pub fn sandbox(input: &[u8]) -> Result<Vec<u8>, AppError> {
    let decompressed;
    let input = if input.starts_with(&[0x1f, 0x8b]) {
        let mut decoder = flate2::read::GzDecoder::new(input);
        let mut bytes = Vec::new();
        decoder
            .by_ref()
            .take((LIMIT + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|e| AppError::InvalidModel(format!("invalid gzip asset: {e}")))?;
        if bytes.len() > LIMIT {
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
    if output.len() > LIMIT {
        return Err(AppError::TooLarge);
    }
    Ok(output)
}

pub fn package_game(input: &[u8], game_name: &str) -> Result<Vec<u8>, AppError> {
    let input = decompress(input)?;
    let mut dom = decode(&input)?;
    let root = dom.root_ref();

    let mut package_tags = Tags::new();
    package_tags.push("nl_package");
    let package = dom.insert(
        root,
        InstanceBuilder::new("Configuration")
            .with_name(format!("Game Package ({game_name})"))
            .with_property("Tags", package_tags),
    );
    let data_model = dom.insert(
        package,
        InstanceBuilder::new("Folder")
            .with_name("DataModel")
            .with_property("Attributes", Attributes::new().with("name", game_name)),
    );

    let services: Vec<Ref> = dom
        .root()
        .children()
        .iter()
        .copied()
        .filter(|referent| {
            dom.get_by_ref(*referent)
                .is_some_and(|instance| TARGET_SERVICES.contains(&instance.class.as_str()))
        })
        .collect();
    let mut remapped_services = Vec::new();

    for service_ref in services {
        remove_ignored_descendants(&mut dom, service_ref);
        let service = dom.get_by_ref(service_ref).expect("service exists");
        let service_name = service.name.to_string();
        let service_class = service.class.to_string();
        let children = service.children().to_vec();
        let attributes = service_attributes(service);
        let folder = dom.insert(
            data_model,
            InstanceBuilder::new("Folder")
                .with_name(service_name)
                .with_property("Attributes", attributes.with("ClassName", service_class)),
        );
        for child in children {
            dom.transfer_within(child, folder);
        }
        remapped_services.push((service_ref, folder));
    }

    let package_refs: Vec<Ref> = dom
        .descendants_of(package)
        .map(|instance| instance.referent())
        .collect();
    let included_refs: std::collections::HashSet<Ref> = package_refs.iter().copied().collect();
    for referent in package_refs {
        let Some(instance) = dom.get_by_ref_mut(referent) else {
            continue;
        };
        for value in instance.properties.values_mut() {
            if let Variant::Ref(target) = value {
                if let Some((_, replacement)) = remapped_services
                    .iter()
                    .find(|(source, _)| source == target)
                {
                    *target = *replacement;
                } else if !included_refs.contains(target) {
                    *target = Ref::none();
                }
            }
        }
        if matches!(
            instance.class.as_str(),
            "Script" | "LocalScript" | "ModuleScript"
        ) {
            let source = instance
                .properties
                .get_mut(&"Source".into())
                .ok_or_else(|| {
                    AppError::InvalidModel(format!("{} has no Source property", instance.class))
                })?;
            let Variant::String(source) = source else {
                return Err(AppError::InvalidModel(format!(
                    "{} has an invalid Source property",
                    instance.class
                )));
            };
            if !source.starts_with(GAME_PREFIX) {
                source.insert_str(0, GAME_PREFIX);
            }
        }
    }

    let mut output = Vec::new();
    rbx_binary::to_writer(&mut output, &dom, &[package])
        .map_err(|e| AppError::InvalidModel(e.to_string()))?;
    if output.len() > LIMIT {
        return Err(AppError::TooLarge);
    }
    Ok(output)
}

fn decompress(input: &[u8]) -> Result<Vec<u8>, AppError> {
    if !input.starts_with(&[0x1f, 0x8b]) {
        return Ok(input.to_vec());
    }
    let mut bytes = Vec::new();
    flate2::read::GzDecoder::new(input)
        .take((LIMIT + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|e| AppError::InvalidModel(format!("invalid gzip asset: {e}")))?;
    if bytes.len() > LIMIT {
        return Err(AppError::TooLarge);
    }
    Ok(bytes)
}

fn decode(input: &[u8]) -> Result<WeakDom, AppError> {
    let trimmed = input.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(input);
    let trimmed = trimmed
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .map(|start| &trimmed[start..])
        .unwrap_or(trimmed);
    if trimmed.starts_with(b"<?xml")
        || (trimmed.starts_with(b"<roblox") && !trimmed.starts_with(b"<roblox!"))
    {
        let normalized = strip_legacy_external_elements(trimmed)?;
        rbx_xml::from_reader_default(Cursor::new(normalized.as_ref()))
            .map_err(|e| AppError::InvalidModel(e.to_string()))
    } else {
        rbx_binary::from_reader(Cursor::new(input))
            .map_err(|e| AppError::InvalidModel(e.to_string()))
    }
}

fn strip_legacy_external_elements(input: &[u8]) -> Result<Cow<'_, [u8]>, AppError> {
    const OPEN: &[u8] = b"<External>";
    const CLOSE: &[u8] = b"</External>";
    let Some(mut start) = input.windows(OPEN.len()).position(|window| window == OPEN) else {
        return Ok(Cow::Borrowed(input));
    };

    let mut output = Vec::with_capacity(input.len());
    let mut copied_until = 0;
    loop {
        output.extend_from_slice(&input[copied_until..start]);
        let content_start = start + OPEN.len();
        let Some(close_offset) = input[content_start..]
            .windows(CLOSE.len())
            .position(|window| window == CLOSE)
        else {
            return Err(AppError::InvalidModel(
                "legacy External element was not closed".into(),
            ));
        };
        copied_until = content_start + close_offset + CLOSE.len();
        let Some(next_offset) = input[copied_until..]
            .windows(OPEN.len())
            .position(|window| window == OPEN)
        else {
            output.extend_from_slice(&input[copied_until..]);
            return Ok(Cow::Owned(output));
        };
        start = copied_until + next_offset;
    }
}

fn remove_ignored_descendants(dom: &mut WeakDom, service_ref: Ref) {
    let mut ignored_roots = Vec::new();
    let mut ignored = std::collections::HashSet::new();
    for instance in dom.descendants_of(service_ref).skip(1) {
        if ignored.contains(&instance.parent()) {
            ignored.insert(instance.referent());
            continue;
        }
        // Terrain is bound to Workspace and makes an uploaded Model unloadable.
        if instance.class == "Terrain"
            || !is_archivable(instance)
            || has_tag(instance, "nl_package")
            || has_tag(instance, "nl_ignore")
        {
            ignored.insert(instance.referent());
            ignored_roots.push(instance.referent());
        }
    }
    for referent in ignored_roots {
        dom.destroy(referent);
    }
}

fn is_archivable(instance: &rbx_dom_weak::Instance) -> bool {
    !matches!(
        instance.properties.get(&"Archivable".into()),
        Some(Variant::Bool(false))
    )
}

fn has_tag(instance: &rbx_dom_weak::Instance, expected: &str) -> bool {
    matches!(
        instance.properties.get(&"Tags".into()),
        Some(Variant::Tags(tags)) if tags.iter().any(|tag| tag == expected)
    )
}

fn service_attributes(instance: &rbx_dom_weak::Instance) -> Attributes {
    let database = rbx_reflection_database::get_bundled();
    let Some(class) = database.classes.get(instance.class.as_str()) else {
        return Attributes::new();
    };
    let mut attributes = Attributes::new();
    for (name, value) in &instance.properties {
        if !name.as_str().starts_with(char::is_uppercase) {
            continue;
        }
        let descriptor = database
            .superclasses_iter(class)
            .find_map(|class| class.properties.get(name.as_str()));
        let Some(descriptor) = descriptor else {
            continue;
        };
        if !matches!(descriptor.scriptability, Scriptability::ReadWrite)
            || !matches!(
                descriptor.kind,
                PropertyKind::Canonical {
                    serialization: PropertySerialization::Serializes
                }
            )
        {
            continue;
        }
        if database
            .superclasses_iter(class)
            .find_map(|class| class.default_properties.get(name.as_str()))
            == Some(value)
        {
            continue;
        }
        if let Some(value) = attribute_value(database, descriptor, value) {
            attributes.insert(name.to_string(), value);
        }
    }
    attributes
}

fn attribute_value(
    database: &rbx_reflection::ReflectionDatabase<'_>,
    descriptor: &rbx_reflection::PropertyDescriptor<'_>,
    value: &Variant,
) -> Option<Variant> {
    match value {
        Variant::Bool(_)
        | Variant::Float32(_)
        | Variant::Float64(_)
        | Variant::Int32(_)
        | Variant::Int64(_)
        | Variant::String(_)
        | Variant::Vector2(_)
        | Variant::Vector3(_)
        | Variant::Color3(_)
        | Variant::CFrame(_)
        | Variant::UDim2(_)
        | Variant::Rect(_) => Some(value.clone()),
        Variant::Enum(value) => {
            let DataType::Enum(enum_name) = &descriptor.data_type else {
                return None;
            };
            let item = database
                .enums
                .get(enum_name.as_ref())?
                .items
                .iter()
                .find(|(_, candidate)| **candidate == value.to_u32())?
                .0;
            Some(Variant::String(format!("nl_enum_{item}")))
        }
        Variant::EnumItem(value) => {
            let item = database
                .enums
                .get(value.ty.as_str())?
                .items
                .iter()
                .find(|(_, candidate)| **candidate == value.value)?
                .0;
            Some(Variant::String(format!("nl_enum_{item}")))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rbx_types::Enum;

    fn attribute_text(value: Option<&Variant>) -> Option<String> {
        match value? {
            Variant::String(value) => Some(value.clone()),
            Variant::BinaryString(value) => {
                String::from_utf8(<rbx_types::BinaryString as AsRef<[u8]>>::as_ref(value).to_vec())
                    .ok()
            }
            _ => None,
        }
    }

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

    #[test]
    fn packages_xml_with_legacy_external_elements() {
        let input = br#"<roblox version="4"><External>RBX0</External><Item class="Workspace" referent="RBX1"><Properties><string name="Name">Workspace</string></Properties><External>RBX2</External></Item></roblox>"#;

        let decoded = decode(input).unwrap();
        assert!(decoded
            .descendants()
            .any(|instance| instance.class == "Workspace"));
        let output = package_game(input, "Legacy").unwrap();
        let parsed = rbx_binary::from_reader(Cursor::new(output)).unwrap();
        assert!(parsed
            .descendants()
            .any(|instance| instance.name == "Game Package (Legacy)"));
    }

    #[test]
    fn packages_game_services_scripts_attributes_and_references() {
        let lighting = InstanceBuilder::new("Lighting")
            .with_name("Lighting")
            .with_property("Technology", Enum::from_u32(3));
        let lighting_ref = lighting.referent();
        let mut ignored_tags = Tags::new();
        ignored_tags.push("nl_ignore");
        let workspace = InstanceBuilder::new("Workspace")
            .with_name("Workspace")
            .with_property("Gravity", 100.0_f32)
            .with_children([
                InstanceBuilder::new("ObjectValue")
                    .with_name("LightingReference")
                    .with_property("Value", lighting_ref),
                InstanceBuilder::new("Folder")
                    .with_name("Ignored")
                    .with_property("Tags", ignored_tags)
                    .with_child(InstanceBuilder::new("Part")),
                InstanceBuilder::new("Part")
                    .with_name("NonArchivable")
                    .with_property("Archivable", false),
                InstanceBuilder::new("Terrain").with_name("Terrain"),
            ]);
        let scripts = InstanceBuilder::new("ServerScriptService")
            .with_name("ServerScriptService")
            .with_children([
                InstanceBuilder::new("Script").with_property("Source", "print('wrapped')"),
                InstanceBuilder::new("ModuleScript")
                    .with_property("Source", format!("{GAME_PREFIX}return true")),
            ]);
        let source = WeakDom::new(
            InstanceBuilder::new("DataModel").with_children([
                workspace,
                lighting,
                scripts,
                InstanceBuilder::new("HttpService")
                    .with_child(InstanceBuilder::new("Folder").with_name("Not Packaged")),
            ]),
        );
        let mut input = Vec::new();
        rbx_binary::to_writer(&mut input, &source, source.root().children()).unwrap();

        let output = package_game(&input, "Fixture").unwrap();
        let parsed = rbx_binary::from_reader(Cursor::new(output)).unwrap();
        let package = parsed
            .descendants()
            .find(|instance| instance.class == "Configuration")
            .unwrap();
        assert_eq!(package.name, "Game Package (Fixture)");
        assert!(has_tag(package, "nl_package"));
        let data_model = package
            .children()
            .iter()
            .filter_map(|child| parsed.get_by_ref(*child))
            .find(|instance| instance.name == "DataModel")
            .unwrap();
        let Variant::Attributes(data_attributes) = &data_model.properties[&"Attributes".into()]
        else {
            panic!("DataModel attributes missing")
        };
        assert_eq!(
            attribute_text(data_attributes.get("name")),
            Some("Fixture".into())
        );

        let workspace = data_model
            .children()
            .iter()
            .filter_map(|child| parsed.get_by_ref(*child))
            .find(|instance| instance.name == "Workspace")
            .unwrap();
        let lighting = data_model
            .children()
            .iter()
            .filter_map(|child| parsed.get_by_ref(*child))
            .find(|instance| instance.name == "Lighting")
            .unwrap();
        let Variant::Attributes(attributes) = &workspace.properties[&"Attributes".into()] else {
            panic!("service attributes missing")
        };
        assert_eq!(
            attribute_text(attributes.get("ClassName")),
            Some("Workspace".into())
        );
        assert_eq!(attributes.get("Gravity"), Some(&Variant::Float32(100.0)));

        let reference = parsed
            .descendants_of(workspace.referent())
            .find(|instance| instance.name == "LightingReference")
            .unwrap();
        assert_eq!(
            reference.properties.get(&"Value".into()),
            Some(&Variant::Ref(lighting.referent()))
        );
        assert!(!parsed.descendants().any(|instance| {
            matches!(
                instance.name.as_str(),
                "Ignored" | "NonArchivable" | "Not Packaged"
            )
        }));
        assert!(!parsed
            .descendants()
            .any(|instance| instance.class == "Terrain"));

        let sources: Vec<_> = parsed
            .descendants()
            .filter(|instance| matches!(instance.class.as_str(), "Script" | "ModuleScript"))
            .map(|instance| match &instance.properties[&"Source".into()] {
                Variant::String(source) => source,
                _ => panic!("invalid source"),
            })
            .collect();
        assert_eq!(sources.len(), 2);
        assert!(sources.iter().all(|source| source.starts_with(GAME_PREFIX)));
        assert!(sources
            .iter()
            .all(|source| source.matches(GAME_PREFIX).count() == 1));
    }

    #[tokio::test]
    #[ignore = "live Roblox acceptance test"]
    async fn parses_crossroads_live() {
        let client = reqwest::Client::builder().build().unwrap();
        let input = client
            .get("https://assetdelivery.roblox.com/v1/asset/?id=1818")
            .send()
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        let output = package_game(&input, "Classic: Crossroads").unwrap();
        let parsed = rbx_binary::from_reader(Cursor::new(output)).unwrap();
        assert!(parsed.descendants().any(|instance| {
            instance.class == "Configuration"
                && instance.name == "Game Package (Classic: Crossroads)"
        }));
        assert!(parsed
            .descendants()
            .any(|instance| instance.class == "Folder" && instance.name == "Workspace"));
    }
}
