use std::{
    borrow::Cow,
    io::{Cursor, Read},
};

use crate::error::AppError;
use rbx_dom_weak::{InstanceBuilder, WeakDom};
use rbx_reflection::{DataType, PropertyKind, PropertySerialization, Scriptability};
use rbx_types::{Attributes, ContentId, Ref, Tags, Variant};
use tree_sitter::{Node, Parser};

pub const PREFIX: &str = "require(game:WaitForChild(\"native_legacy\"))(getfenv());";
const GAME_PREFIX: &str = PREFIX;
const STRING_DECODER: &str = "__STRDEC";
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
    let mut parser = luau_parser()?;
    for referent in refs {
        let Some(instance) = dom.get_by_ref_mut(referent) else {
            continue;
        };
        if matches!(
            instance.class.as_str(),
            "Script" | "LocalScript" | "ModuleScript"
        ) {
            transform_script(instance, &mut parser, PREFIX)?;
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
    let root_children = dom.root().children().to_vec();
    let has_native_services = root_children.iter().any(|referent| {
        dom.get_by_ref(*referent)
            .is_some_and(|instance| TARGET_SERVICES.contains(&instance.class.as_str()))
    });
    let legacy_container = (!has_native_services)
        .then(|| {
            root_children.iter().copied().find(|referent| {
                dom.get_by_ref(*referent).is_some_and(|instance| {
                    instance.class == "Model"
                        && instance.children().iter().any(|child| {
                            dom.get_by_ref(*child).is_some_and(|child| {
                                child.class == "Model"
                                    && TARGET_SERVICES.contains(&child.name.as_str())
                            })
                        })
                })
            })
        })
        .flatten();
    let source_children = legacy_container
        .and_then(|referent| dom.get_by_ref(referent))
        .map(|instance| instance.children().to_vec())
        .unwrap_or(root_children);
    let allow_legacy_services = legacy_container.is_some() || !has_native_services;
    let services: Vec<(Ref, String)> = source_children
        .iter()
        .filter_map(|referent| {
            let instance = dom.get_by_ref(*referent)?;
            service_class(instance, allow_legacy_services)
                .map(|class_name| (*referent, class_name.to_owned()))
        })
        .collect();
    let service_refs: std::collections::HashSet<Ref> =
        services.iter().map(|(referent, _)| *referent).collect();
    let loose_children: Vec<Ref> = if has_native_services {
        Vec::new()
    } else {
        source_children
            .into_iter()
            .filter(|referent| !service_refs.contains(referent))
            .collect()
    };

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

    let mut remapped_services = Vec::new();
    let mut workspace_folder = None;

    for (service_ref, service_class) in services {
        remove_ignored_descendants(&mut dom, service_ref);
        let service = dom.get_by_ref(service_ref).expect("service exists");
        let service_name = service.name.to_string();
        let children = service.children().to_vec();
        let attributes = service_attributes(service);
        let folder = dom.insert(
            data_model,
            InstanceBuilder::new("Folder")
                .with_name(service_name)
                .with_property(
                    "Attributes",
                    attributes.with("ClassName", service_class.clone()),
                ),
        );
        for child in children {
            dom.transfer_within(child, folder);
        }
        if service_class == "Workspace" {
            workspace_folder = Some(folder);
        }
        remapped_services.push((service_ref, folder));
    }

    if !loose_children.is_empty() {
        let workspace = workspace_folder.unwrap_or_else(|| {
            dom.insert(
                data_model,
                InstanceBuilder::new("Folder")
                    .with_name("Workspace")
                    .with_property(
                        "Attributes",
                        Attributes::new().with("ClassName", "Workspace"),
                    ),
            )
        });
        for child in loose_children {
            let Some(instance) = dom.get_by_ref(child) else {
                continue;
            };
            if instance.class == "Terrain"
                || !is_archivable(instance)
                || has_tag(instance, "nl_package")
                || has_tag(instance, "nl_ignore")
            {
                dom.destroy(child);
                continue;
            }
            remove_ignored_descendants(&mut dom, child);
            dom.transfer_within(child, workspace);
        }
    }

    if dom
        .get_by_ref(data_model)
        .is_none_or(|instance| instance.children().is_empty())
    {
        return Err(AppError::InvalidModel(
            "game package contained no DataModel children".into(),
        ));
    }

    let package_refs: Vec<Ref> = dom
        .descendants_of(package)
        .map(|instance| instance.referent())
        .collect();
    let included_refs: std::collections::HashSet<Ref> = package_refs.iter().copied().collect();
    let mut parser = luau_parser()?;
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
            transform_script(instance, &mut parser, GAME_PREFIX)?;
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

fn service_class(instance: &rbx_dom_weak::Instance, allow_legacy: bool) -> Option<&str> {
    if TARGET_SERVICES.contains(&instance.class.as_str()) {
        Some(instance.class.as_str())
    } else if allow_legacy
        && instance.class == "Model"
        && TARGET_SERVICES.contains(&instance.name.as_str())
    {
        Some(instance.name.as_str())
    } else {
        None
    }
}

fn luau_parser() -> Result<Parser, AppError> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_luau::LANGUAGE.into())
        .map_err(|error| AppError::InvalidModel(format!("failed to load Luau parser: {error}")))?;
    Ok(parser)
}

fn transform_script(
    instance: &mut rbx_dom_weak::Instance,
    parser: &mut Parser,
    prefix: &str,
) -> Result<(), AppError> {
    let source_key = "Source".into();
    let source = match instance.properties.get(&source_key) {
        Some(Variant::String(source)) => source,
        Some(_) => {
            return Err(AppError::InvalidModel(format!(
                "{} has an invalid Source property",
                instance.class
            )))
        }
        None => {
            return Err(AppError::InvalidModel(format!(
                "{} has no Source property",
                instance.class
            )))
        }
    };
    let source = transform_script_source(parser, source, prefix)?;
    instance
        .properties
        .insert(source_key, Variant::String(source));

    let linked_source_key = "LinkedSource".into();
    let linked_source = match instance.properties.get(&linked_source_key) {
        Some(Variant::ContentId(linked_source)) if !linked_source.as_str().is_empty() => {
            Some(linked_source.as_str().to_owned())
        }
        Some(Variant::ContentId(_)) | None => None,
        Some(_) => {
            return Err(AppError::InvalidModel(format!(
                "{} has an invalid LinkedSource property",
                instance.class
            )))
        }
    };
    let Some(linked_source) = linked_source else {
        return Ok(());
    };

    instance
        .properties
        .insert(linked_source_key, Variant::ContentId(ContentId::new()));
    let attributes_key = "Attributes".into();
    let mut attributes = match instance.properties.get(&attributes_key) {
        Some(Variant::Attributes(attributes)) => attributes.clone(),
        Some(_) => {
            return Err(AppError::InvalidModel(format!(
                "{} has invalid Attributes",
                instance.class
            )))
        }
        None => Attributes::new(),
    };
    attributes.insert("LinkedSource".into(), Variant::String(linked_source));
    instance
        .properties
        .insert(attributes_key, Variant::Attributes(attributes));
    Ok(())
}

fn transform_script_source(
    parser: &mut Parser,
    source: &str,
    prefix: &str,
) -> Result<String, AppError> {
    let source = source.strip_prefix(prefix).unwrap_or(source);
    let source = wrap_string_literals(parser, source)?;
    Ok(format!("{prefix}{source}"))
}

fn wrap_string_literals(parser: &mut Parser, source: &str) -> Result<String, AppError> {
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| AppError::InvalidModel("failed to parse Luau source".into()))?;
    let mut insertions = Vec::new();
    collect_string_insertions(tree.root_node(), source.as_bytes(), &mut insertions);
    if insertions.is_empty() {
        return Ok(source.to_owned());
    }

    // Closing parentheses must precede prefixes when two insertions share a byte boundary.
    insertions.sort_unstable_by_key(|insertion| (insertion.offset, insertion.order));
    let extra_len: usize = insertions
        .iter()
        .map(|insertion| insertion.text.len())
        .sum();
    let mut output = String::with_capacity(source.len() + extra_len);
    let mut copied_until = 0;
    for insertion in insertions {
        output.push_str(&source[copied_until..insertion.offset]);
        output.push_str(insertion.text);
        copied_until = insertion.offset;
    }
    output.push_str(&source[copied_until..]);
    Ok(output)
}

#[derive(Clone, Copy)]
struct Insertion {
    offset: usize,
    order: u8,
    text: &'static str,
}

fn collect_string_insertions(node: Node<'_>, source: &[u8], insertions: &mut Vec<Insertion>) {
    if node.kind() == "string"
        && !is_literal_type(node)
        && !is_direct_decoder_argument(node, source)
    {
        if is_shorthand_call_argument(node, source) {
            insertions.push(Insertion {
                offset: node.start_byte(),
                order: 1,
                text: "(__STRDEC ",
            });
            insertions.push(Insertion {
                offset: node.end_byte(),
                order: 0,
                text: ")",
            });
        } else {
            insertions.push(Insertion {
                offset: node.start_byte(),
                order: 1,
                text: "__STRDEC ",
            });
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_string_insertions(child, source, insertions);
    }
}

fn is_literal_type(node: Node<'_>) -> bool {
    node.parent()
        .is_some_and(|parent| parent.kind() == "literal_type")
}

fn is_direct_decoder_argument(node: Node<'_>, source: &[u8]) -> bool {
    let Some(arguments) = node.parent().filter(|parent| parent.kind() == "arguments") else {
        return false;
    };
    let Some(call) = arguments
        .parent()
        .filter(|parent| parent.kind() == "function_call")
    else {
        return false;
    };
    call.child_by_field_name("name")
        .and_then(|name| name.utf8_text(source).ok())
        == Some(STRING_DECODER)
}

fn is_shorthand_call_argument(node: Node<'_>, source: &[u8]) -> bool {
    let Some(arguments) = node.parent().filter(|parent| parent.kind() == "arguments") else {
        return false;
    };
    arguments
        .parent()
        .is_some_and(|parent| parent.kind() == "function_call")
        && source.get(arguments.start_byte()) != Some(&b'(')
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
        let without_externals = strip_legacy_external_elements(trimmed)?;
        let normalized = strip_legacy_binary_content(without_externals.as_ref())?;
        let sanitized = sanitize_legacy_xml_controls(normalized.as_ref());
        rbx_xml::from_reader_default(Cursor::new(sanitized.as_ref()))
            .map_err(|e| AppError::InvalidModel(e.to_string()))
    } else {
        rbx_binary::from_reader(Cursor::new(input))
            .map_err(|e| AppError::InvalidModel(e.to_string()))
    }
}

fn sanitize_legacy_xml_controls(input: &[u8]) -> Cow<'_, [u8]> {
    let mut output: Option<Vec<u8>> = None;
    let mut copied_until = 0;
    let mut index = 0;
    while index < input.len() {
        let replace_len = if input[index] < 0x20 && !matches!(input[index], b'\t' | b'\n' | b'\r') {
            Some(1)
        } else {
            invalid_xml_numeric_reference(&input[index..])
        };
        let Some(replace_len) = replace_len else {
            index += 1;
            continue;
        };
        let output = output.get_or_insert_with(|| Vec::with_capacity(input.len()));
        output.extend_from_slice(&input[copied_until..index]);
        output.push(b' ');
        index += replace_len;
        copied_until = index;
    }
    let Some(mut output) = output else {
        return Cow::Borrowed(input);
    };
    output.extend_from_slice(&input[copied_until..]);
    Cow::Owned(output)
}

fn invalid_xml_numeric_reference(input: &[u8]) -> Option<usize> {
    if !input.starts_with(b"&#") {
        return None;
    }
    let mut index = 2;
    let radix = if input
        .get(index)
        .is_some_and(|byte| matches!(byte, b'x' | b'X'))
    {
        index += 1;
        16_u32
    } else {
        10_u32
    };
    let digit_start = index;
    let mut value = 0_u32;
    while let Some(byte) = input.get(index) {
        let digit = match radix {
            10 => byte.is_ascii_digit().then(|| u32::from(*byte - b'0')),
            16 => byte.to_ascii_lowercase().is_ascii_hexdigit().then(|| {
                if byte.is_ascii_digit() {
                    u32::from(*byte - b'0')
                } else {
                    u32::from(byte.to_ascii_lowercase() - b'a' + 10)
                }
            }),
            _ => unreachable!(),
        };
        let Some(digit) = digit else {
            break;
        };
        value = value.checked_mul(radix)?.checked_add(digit)?;
        index += 1;
    }
    if index == digit_start || input.get(index) != Some(&b';') {
        return None;
    }
    (value < 0x20 && !matches!(value, 0x09 | 0x0a | 0x0d)).then_some(index + 1)
}

fn strip_legacy_binary_content(input: &[u8]) -> Result<Cow<'_, [u8]>, AppError> {
    const BINARY_OPEN: &[u8] = b"<binary";
    const BINARY_CLOSE: &[u8] = b"</binary>";
    const HASH_OPEN: &[u8] = b"<hash";
    const HASH_CLOSE: &[u8] = b"</hash>";
    const EMPTY_CONTENT: &[u8] = b"<null></null>";

    let mut output = Vec::with_capacity(input.len());
    let mut copied_until = 0;
    loop {
        let binary = input[copied_until..]
            .windows(BINARY_OPEN.len())
            .position(|window| window == BINARY_OPEN)
            .map(|offset| (offset, BINARY_OPEN, BINARY_CLOSE, "binary"));
        let hash = input[copied_until..]
            .windows(HASH_OPEN.len())
            .position(|window| window == HASH_OPEN)
            .map(|offset| (offset, HASH_OPEN, HASH_CLOSE, "hash"));
        let next = match (binary, hash) {
            (Some(binary), Some(hash)) => Some(if binary.0 < hash.0 { binary } else { hash }),
            (Some(binary), None) => Some(binary),
            (None, Some(hash)) => Some(hash),
            (None, None) => None,
        };
        let Some((offset, open, close, kind)) = next else {
            if copied_until == 0 {
                return Ok(Cow::Borrowed(input));
            }
            output.extend_from_slice(&input[copied_until..]);
            return Ok(Cow::Owned(output));
        };
        let start = copied_until + offset;
        output.extend_from_slice(&input[copied_until..start]);
        let Some(open_end_offset) = input[start + open.len()..]
            .iter()
            .position(|byte| *byte == b'>')
        else {
            return Err(AppError::InvalidModel(format!(
                "legacy {kind} content opening tag was not closed"
            )));
        };
        let content_start = start + open.len() + open_end_offset + 1;
        let Some(close_offset) = input[content_start..]
            .windows(close.len())
            .position(|window| window == close)
        else {
            return Err(AppError::InvalidModel(format!(
                "legacy {kind} content was not closed"
            )));
        };
        output.extend_from_slice(EMPTY_CONTENT);
        copied_until = content_start + close_offset + close.len();
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

    fn wrap(source: &str) -> String {
        wrap_string_literals(&mut luau_parser().unwrap(), source).unwrap()
    }

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
    fn wraps_luau_string_literals() {
        assert_eq!(
            wrap(r#"local asset_id = "rbxassetid://""#),
            r#"local asset_id = __STRDEC "rbxassetid://""#
        );
        assert_eq!(
            wrap("local owner_names = { [[Telemon]], 'dave' .. 'bazuka' }"),
            "local owner_names = { __STRDEC [[Telemon]], __STRDEC 'dave' .. __STRDEC 'bazuka' }"
        );
    }

    #[test]
    fn preserves_comments_string_types_and_existing_decoder_calls() {
        let source = concat!(
            "-- keep \'comment text\' untouched\n",
            "type Kind = \"literal type\"\n",
            "local value: Kind = __STRDEC \"already wrapped\"\n",
            "local interpolated = `hello {name .. \"!\"}`\n",
        );
        let expected = concat!(
            "-- keep \'comment text\' untouched\n",
            "type Kind = \"literal type\"\n",
            "local value: Kind = __STRDEC \"already wrapped\"\n",
            "local interpolated = __STRDEC `hello {name .. __STRDEC \"!\"}`\n",
        );
        assert_eq!(wrap(source), expected);
    }

    #[test]
    fn preserves_shorthand_call_syntax_and_line_numbers() {
        let source = "print \"first\"\nlocal block = [[second\nthird]]\nreturn 'fourth'\n";
        let transformed = transform_script_source(&mut luau_parser().unwrap(), source, PREFIX)
            .expect("source transforms");

        assert_eq!(
            transformed,
            concat!(
                "require(game:WaitForChild(\"native_legacy\"))(getfenv());",
                "print (__STRDEC \"first\")\n",
                "local block = __STRDEC [[second\nthird]]\n",
                "return __STRDEC 'fourth'\n",
            )
        );
        assert_eq!(
            transformed.bytes().filter(|byte| *byte == b'\n').count(),
            source.bytes().filter(|byte| *byte == b'\n').count()
        );

        let tree = luau_parser()
            .unwrap()
            .parse(&transformed, None)
            .expect("transformed source parses");
        assert!(!tree.root_node().has_error());
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
    fn preserves_and_clears_linked_sources_for_all_script_types() {
        let script_classes = ["Script", "LocalScript", "ModuleScript"];
        let dom = WeakDom::new(InstanceBuilder::new("DataModel").with_children(
            script_classes.iter().enumerate().map(|(index, class)| {
                let mut attributes = Attributes::new();
                attributes.insert("Existing".into(), Variant::Bool(true));
                InstanceBuilder::new(*class)
                    .with_name(format!("Linked{index}"))
                    .with_property("Source", format!("print('{index}')"))
                    .with_property(
                        "LinkedSource",
                        ContentId::from(format!("rbxassetid://{}", index + 100)),
                    )
                    .with_property("Attributes", attributes)
            }),
        ));
        let mut bytes = Vec::new();
        rbx_binary::to_writer(&mut bytes, &dom, dom.root().children()).unwrap();

        let transformed = sandbox(&bytes).unwrap();
        let parsed = rbx_binary::from_reader(Cursor::new(transformed)).unwrap();
        for (index, class) in script_classes.iter().enumerate() {
            let script = parsed
                .descendants()
                .find(|instance| instance.name == format!("Linked{index}"))
                .unwrap();
            assert_eq!(&script.class, class);
            assert_eq!(
                script.properties.get(&"LinkedSource".into()),
                Some(&Variant::ContentId(ContentId::new()))
            );
            let Some(Variant::Attributes(attributes)) = script.properties.get(&"Attributes".into())
            else {
                panic!("attributes missing")
            };
            assert_eq!(attributes.get("Existing"), Some(&Variant::Bool(true)));
            assert_eq!(
                attribute_text(attributes.get("LinkedSource")),
                Some(format!("rbxassetid://{}", index + 100))
            );
            let Some(Variant::String(source)) = script.properties.get(&"Source".into()) else {
                panic!("source missing")
            };
            assert_eq!(source, &format!("{PREFIX}print(__STRDEC '{index}')"));
        }
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
        assert_eq!(source, &format!("{PREFIX}print(__STRDEC 'xml')"));
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
    fn packages_xml_with_legacy_embedded_texture_content() {
        let input = br#"<roblox xmlns:xmime="http://www.w3.org/2005/05/xmlmime" version="4"><Item class="Workspace" referent="RBX1"><Properties><string name="Name">Workspace</string></Properties><Item class="Texture" referent="RBX2"><Properties><string name="Name">Texture</string><Content name="Texture" mimeType="image/jpeg"><binary xmime:contentType="image/jpeg">aGVsbG8=</binary></Content></Properties></Item><Item class="Texture" referent="RBX3"><Properties><string name="Name">Hashed Texture</string><Content name="Texture"><hash>0123456789abcdef</hash></Content></Properties></Item></Item></roblox>"#;

        let decoded = decode(input).unwrap();
        let texture = decoded
            .descendants()
            .find(|instance| instance.class == "Texture")
            .unwrap();
        assert_eq!(texture.name, "Texture");
        assert!(decoded
            .descendants()
            .any(|instance| instance.name == "Hashed Texture"));

        let output = package_game(input, "Legacy Texture").unwrap();
        let parsed = rbx_binary::from_reader(Cursor::new(output)).unwrap();
        assert!(parsed
            .descendants()
            .any(|instance| instance.name == "Game Package (Legacy Texture)"));
    }

    #[test]
    fn packages_legacy_xml_with_forbidden_control_characters() {
        let input = b"<roblox version=\"4\"><Item class=\"Workspace\" referent=\"RBX1\"><Properties><string name=\"Name\">Workspace</string></Properties><Item class=\"Script\" referent=\"RBX2\"><Properties><string name=\"Name\">Script</string><ProtectedString name=\"Source\">print('\x1b')</ProtectedString></Properties></Item></Item></roblox>";

        let output = package_game(input, "Legacy Controls").unwrap();
        let parsed = rbx_binary::from_reader(Cursor::new(output)).unwrap();
        let data_model = parsed
            .descendants()
            .find(|instance| instance.class == "Folder" && instance.name == "DataModel")
            .unwrap();
        assert!(!data_model.children().is_empty());
        assert!(parsed
            .descendants()
            .any(|instance| instance.class == "Script"));
    }

    #[test]
    fn sanitizes_raw_and_numeric_xml_control_characters() {
        let input = b"raw:\x1b decimal:&#0; hex:&#x1B; allowed:&#9; valid:&#255;";
        assert_eq!(
            sanitize_legacy_xml_controls(input).as_ref(),
            b"raw:  decimal:  hex:  allowed:&#9; valid:&#255;"
        );
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
                InstanceBuilder::new("Script")
                    .with_property("Source", "print('wrapped')")
                    .with_property("LinkedSource", ContentId::from("rbxassetid://12345")),
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
        let linked_script = parsed
            .descendants()
            .find(|instance| {
                matches!(
                    instance.properties.get(&"LinkedSource".into()),
                    Some(Variant::ContentId(linked_source)) if linked_source.as_str().is_empty()
                ) && matches!(
                    instance.properties.get(&"Attributes".into()),
                    Some(Variant::Attributes(attributes))
                        if attribute_text(attributes.get("LinkedSource"))
                            == Some("rbxassetid://12345".into())
                )
            })
            .expect("linked script was preserved as an attribute");
        assert_eq!(linked_script.class, "Script");
    }

    #[test]
    fn packages_legacy_model_wrapped_services_and_loose_children() {
        let source = WeakDom::new(
            InstanceBuilder::new("DataModel").with_child(
                InstanceBuilder::new("Model")
                    .with_name("CreatorId=2231221 ___ PlaceId=14375697")
                    .with_children([
                        InstanceBuilder::new("Model")
                            .with_name("Workspace")
                            .with_children([
                                InstanceBuilder::new("Part").with_name("Arena"),
                                InstanceBuilder::new("Script")
                                    .with_property("Source", "print('legacy')"),
                            ]),
                        InstanceBuilder::new("Model")
                            .with_name("Lighting")
                            .with_child(InstanceBuilder::new("Sky")),
                        InstanceBuilder::new("IntValue").with_name("votesp"),
                    ]),
            ),
        );
        let mut input = Vec::new();
        rbx_binary::to_writer(&mut input, &source, source.root().children()).unwrap();

        let output = package_game(&input, "Sword Fighting Tournament").unwrap();
        let parsed = rbx_binary::from_reader(Cursor::new(output)).unwrap();
        let data_model = parsed
            .descendants()
            .find(|instance| instance.class == "Folder" && instance.name == "DataModel")
            .unwrap();
        let children: Vec<_> = data_model
            .children()
            .iter()
            .filter_map(|referent| parsed.get_by_ref(*referent))
            .collect();
        assert_eq!(children.len(), 2);
        assert!(children.iter().any(|instance| instance.name == "Lighting"));
        let workspace = children
            .iter()
            .find(|instance| instance.name == "Workspace")
            .unwrap();
        let workspace_names: Vec<_> = parsed
            .descendants_of(workspace.referent())
            .map(|instance| instance.name.as_str())
            .collect();
        assert!(workspace_names.contains(&"Arena"));
        assert!(workspace_names.contains(&"votesp"));
        assert!(!parsed
            .descendants()
            .any(|instance| instance.name.starts_with("CreatorId=")));
    }

    #[test]
    fn rejects_empty_game_packages() {
        let source = WeakDom::new(InstanceBuilder::new("DataModel"));
        let mut input = Vec::new();
        rbx_binary::to_writer(&mut input, &source, source.root().children()).unwrap();

        assert!(matches!(
            package_game(&input, "Empty"),
            Err(AppError::InvalidModel(message))
                if message == "game package contained no DataModel children"
        ));
    }

    #[tokio::test]
    #[ignore = "live archive acceptance test"]
    async fn packages_sword_fighting_tournament_archive_live() {
        let input = reqwest::get("https://raw.githubusercontent.com/Builder-Pals/native-level-archive/main/levels/sha256/e2/e2a6ea8f0a8b747ed4d043b82ea53754af39e403e972d5f383b2d21758256342.rbxlx")
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .bytes()
            .await
            .unwrap();
        let output = package_game(&input, "Sword Fighting Tournament").unwrap();
        if let Ok(path) = std::env::var("NL_ACCEPTANCE_OUTPUT") {
            std::fs::write(path, &output).unwrap();
        }
        let parsed = rbx_binary::from_reader(Cursor::new(output)).unwrap();
        let data_model = parsed
            .descendants()
            .find(|instance| instance.class == "Folder" && instance.name == "DataModel")
            .unwrap();
        let children: Vec<_> = data_model
            .children()
            .iter()
            .filter_map(|referent| parsed.get_by_ref(*referent))
            .collect();
        eprintln!(
            "DataModel children: {}",
            children
                .iter()
                .map(|instance| instance.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
        assert!(!children.is_empty());
        assert!(children.iter().any(|instance| instance.name == "Workspace"));
        assert!(parsed
            .descendants()
            .any(|instance| instance.class == "Part"));
    }

    #[test]
    #[ignore = "manual uploaded package acceptance test"]
    fn inspects_uploaded_game_package() {
        let path = std::env::var("NL_ACCEPTANCE_INPUT").expect("NL_ACCEPTANCE_INPUT is required");
        let input = std::fs::read(path).unwrap();
        let parsed = rbx_binary::from_reader(Cursor::new(input)).unwrap();
        let data_model = parsed
            .descendants()
            .find(|instance| instance.class == "Folder" && instance.name == "DataModel")
            .unwrap();
        let children: Vec<_> = data_model
            .children()
            .iter()
            .filter_map(|referent| parsed.get_by_ref(*referent))
            .collect();
        eprintln!(
            "uploaded DataModel children: {}",
            children
                .iter()
                .map(|instance| instance.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
        assert!(!children.is_empty());
        assert!(children.iter().any(|instance| instance.name == "Workspace"));
        assert!(parsed
            .descendants()
            .any(|instance| instance.class == "Part"));
    }

    #[test]
    #[ignore = "manual LinkedSource game acceptance test"]
    fn packages_linked_source_game_input() {
        let path = std::env::var("NL_ACCEPTANCE_INPUT").expect("NL_ACCEPTANCE_INPUT is required");
        let input = std::fs::read(path).unwrap();
        let source = decode(&input).unwrap();
        let original_linked_sources: std::collections::HashSet<_> = source
            .descendants()
            .filter(|instance| {
                matches!(
                    instance.class.as_str(),
                    "Script" | "LocalScript" | "ModuleScript"
                )
            })
            .filter_map(
                |instance| match instance.properties.get(&"LinkedSource".into()) {
                    Some(Variant::ContentId(linked_source))
                        if !linked_source.as_str().is_empty() =>
                    {
                        Some(linked_source.as_str().to_owned())
                    }
                    _ => None,
                },
            )
            .collect();
        assert!(
            !original_linked_sources.is_empty(),
            "input contained no linked scripts"
        );

        let output = package_game(&input, "LinkedSource acceptance fixture").unwrap();
        let parsed = rbx_binary::from_reader(Cursor::new(output)).unwrap();
        let linked_scripts: Vec<_> = parsed
            .descendants()
            .filter(|instance| {
                matches!(
                    instance.class.as_str(),
                    "Script" | "LocalScript" | "ModuleScript"
                )
            })
            .filter_map(|instance| {
                let Some(Variant::ContentId(linked_source)) =
                    instance.properties.get(&"LinkedSource".into())
                else {
                    return None;
                };
                assert!(linked_source.as_str().is_empty());
                let Some(Variant::String(source)) = instance.properties.get(&"Source".into())
                else {
                    panic!("linked script source missing")
                };
                assert!(source.starts_with(GAME_PREFIX));
                let Some(Variant::Attributes(attributes)) =
                    instance.properties.get(&"Attributes".into())
                else {
                    return None;
                };
                attribute_text(attributes.get("LinkedSource"))
                    .map(|linked_source| (instance, linked_source))
            })
            .collect();
        assert!(
            !linked_scripts.is_empty(),
            "output contained no linked scripts"
        );
        assert!(linked_scripts.iter().all(|(_, linked_source)| {
            original_linked_sources.contains(linked_source.as_str())
        }));
    }

    #[test]
    #[ignore = "manual local archive sweep"]
    fn packages_all_indexed_archive_games() {
        let root = std::path::PathBuf::from(
            std::env::var("NL_ARCHIVE_ROOT").expect("NL_ARCHIVE_ROOT is required"),
        );
        let index: serde_json::Value =
            serde_json::from_slice(&std::fs::read(root.join("place-index-v1.json")).unwrap())
                .unwrap();
        let places = index["places"].as_object().unwrap();
        let mut failures = Vec::new();
        for (place_id, place) in places {
            let preferred = &place["preferred"];
            let title = preferred["title"].as_str().unwrap();
            let path = preferred["path"].as_str().unwrap();
            let input = std::fs::read(root.join(path)).unwrap();
            match package_game(&input, title) {
                Ok(output) => match rbx_binary::from_reader(Cursor::new(&output)) {
                    Ok(parsed) => {
                        let children = parsed
                            .descendants()
                            .find(|instance| {
                                instance.class == "Folder" && instance.name == "DataModel"
                            })
                            .map(|instance| instance.children().len())
                            .unwrap_or_default();
                        if children == 0 {
                            failures.push(format!("{place_id}: empty DataModel"));
                        } else {
                            eprintln!(
                                "packaged {place_id} ({title}): {children} DataModel children, {} bytes",
                                output.len()
                            );
                        }
                    }
                    Err(error) => failures.push(format!("{place_id}: invalid output: {error}")),
                },
                Err(error) => failures.push(format!("{place_id}: {error}")),
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
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
