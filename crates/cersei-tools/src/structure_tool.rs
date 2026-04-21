//! Structure tool: file structure extraction.

use super::*;
use crate::xfile_storage::resolve_xfile_path;
use quote::ToTokens;
use rustpython_parser::{ast, Parse};
use serde::{Deserialize, Serialize};
use std::path::Path;
use syn::{
    Fields, File, ImplItem, Item, ItemConst, ItemEnum, ItemFn, ItemImpl, ItemMod, ItemStatic,
    ItemStruct, ItemTrait, ItemType, ItemUse, ReturnType, Signature, TraitItem, Type, Visibility,
};
use tree_sitter::{Node, Parser};
use tree_sitter_javascript::LANGUAGE as JAVASCRIPT_LANGUAGE;
use tree_sitter_typescript::{LANGUAGE_TSX, LANGUAGE_TYPESCRIPT};

pub struct StructureTool;

#[derive(Debug, Clone, Deserialize)]
struct StructureRequest {
    file_path: String,
    #[serde(default)]
    lang: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct StructureNode {
    kind: String,
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    visibility: Option<String>,
    signature: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "trait")]
    trait_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    children: Vec<StructureNode>,
}

#[async_trait]
impl Tool for StructureTool {
    fn name(&self) -> &str {
        "Structure"
    }

    fn description(&self) -> &str {
        "Provide a language-specific structural outline of a file (classes, functions, etc.). Use this to quickly map out the architecture and symbols of a file before deciding which parts to read or edit. Supports Rust, Python, JavaScript, TypeScript, Vue, Svelte."
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::ReadOnly
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::FileSystem
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "Path to the file to analyze. Absolute paths and workspace-relative paths are accepted."
                },
                "lang": {
                    "type": "string",
                    "description": "Optional language override. If omitted, the language is guessed from the file extension."
                }
            },
            "required": ["file_path"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let req: StructureRequest = match serde_json::from_value(input) {
            Ok(req) => req,
            Err(err) => return ToolResult::error(format!("Invalid input: {}", err)),
        };

        let path = resolve_xfile_path(ctx, &req.file_path);
        if !path.exists() {
            return ToolResult::error(format!("Path not found: {}", path.display()));
        }
        if !path.is_file() {
            return ToolResult::error(format!("Path is not a file: {}", path.display()));
        }

        let lang = req
            .lang
            .as_deref()
            .map(normalize_lang)
            .unwrap_or_else(|| guess_lang_from_path(&path));

        let content = match tokio::fs::read_to_string(&path).await {
            Ok(content) => content,
            Err(err) => {
                return ToolResult::error(format!(
                    "Failed to read file {}: {}",
                    path.display(),
                    err
                ));
            }
        };

        let response = match lang.as_str() {
            "rust" => match extract_rust_structure(&content, &path) {
                Ok(nodes) => serde_json::json!({
                    "file_path": path.display().to_string(),
                    "lang": lang,
                    "supported": true,
                    "skeleton": false,
                    "nodes": nodes,
                    "bytes": content.len()
                }),
                Err(err) => {
                    return ToolResult::error(format!(
                        "Failed to parse Rust file {}: {}",
                        path.display(),
                        err
                    ));
                }
            },
            "vue" => serde_json::json!({
                "file_path": path.display().to_string(),
                "lang": lang,
                "supported": true,
                "skeleton": false,
                "nodes": extract_vue_structure(&content),
                "bytes": content.len()
            }),
            "svelte" => serde_json::json!({
                "file_path": path.display().to_string(),
                "lang": lang,
                "supported": true,
                "skeleton": false,
                "nodes": extract_svelte_structure(&content),
                "bytes": content.len()
            }),
            "python" | "py" => match extract_python_structure(&content, &path) {
                Ok(nodes) => serde_json::json!({
                    "file_path": path.display().to_string(),
                    "lang": lang,
                    "supported": true,
                    "skeleton": false,
                    "nodes": nodes,
                    "bytes": content.len()
                }),
                Err(_) => serde_json::json!({
                    "file_path": path.display().to_string(),
                    "lang": lang,
                    "supported": false,
                    "skeleton": true,
                    "nodes": [],
                    "message": "Language is not supported yet.",
                    "bytes": content.len()
                }),
            },
            "javascript" | "js" | "jsx" => match extract_ts_structure(&content, false, &path) {
                Ok(nodes) => serde_json::json!({
                    "file_path": path.display().to_string(),
                    "lang": lang,
                    "supported": true,
                    "skeleton": false,
                    "nodes": nodes,
                    "bytes": content.len()
                }),
                Err(err) => {
                    return ToolResult::error(format!(
                        "Failed to parse JavaScript file {}: {}",
                        path.display(),
                        err
                    ));
                }
            },
            "typescript" | "ts" | "tsx" => match extract_ts_structure(&content, true, &path) {
                Ok(nodes) => serde_json::json!({
                    "file_path": path.display().to_string(),
                    "lang": lang,
                    "supported": true,
                    "skeleton": false,
                    "nodes": nodes,
                    "bytes": content.len()
                }),
                Err(err) => {
                    return ToolResult::error(format!(
                        "Failed to parse TypeScript file {}: {}",
                        path.display(),
                        err
                    ));
                }
            },
            _ => serde_json::json!({
                "file_path": path.display().to_string(),
                "lang": lang,
                "supported": false,
                "skeleton": true,
                "nodes": [],
                "message": "Language is not supported yet.",
                "bytes": content.len()
            }),
        };

        ToolResult::success(
            serde_json::to_string_pretty(&response).unwrap_or_else(|_| response.to_string()),
        )
        .with_metadata(response)
    }
}

fn extract_rust_structure(
    content: &str,
    path: &Path,
) -> std::result::Result<Vec<StructureNode>, String> {
    let file: File =
        syn::parse_file(content).map_err(|err| format!("{}: {}", path.display(), err))?;
    Ok(file
        .items
        .iter()
        .filter_map(extract_item)
        .collect::<Vec<StructureNode>>())
}

fn extract_item(item: &Item) -> Option<StructureNode> {
    match item {
        Item::Fn(item) => Some(extract_fn_node(item)),
        Item::Struct(item) => Some(extract_struct_node(item)),
        Item::Enum(item) => Some(extract_enum_node(item)),
        Item::Trait(item) => Some(extract_trait_node(item)),
        Item::Impl(item) => Some(extract_impl_node(item)),
        Item::Mod(item) => Some(extract_mod_node(item)),
        Item::Type(item) => Some(extract_type_node(item)),
        Item::Const(item) => Some(extract_const_node(item)),
        Item::Static(item) => Some(extract_static_node(item)),
        Item::Use(item) => Some(extract_use_node(item)),
        _ => None,
    }
}

fn extract_fn_node(item: &ItemFn) -> StructureNode {
    StructureNode {
        kind: "function".to_string(),
        name: item.sig.ident.to_string(),
        visibility: non_empty(visibility_to_string(&item.vis)),
        signature: signature_to_string(&item.sig, Some(&item.vis)),
        target: None,
        trait_name: None,
        detail: function_detail(&item.sig, None, false),
        children: Vec::new(),
    }
}

fn extract_struct_node(item: &ItemStruct) -> StructureNode {
    StructureNode {
        kind: "struct".to_string(),
        name: item.ident.to_string(),
        visibility: non_empty(visibility_to_string(&item.vis)),
        signature: normalize_token_string(format!(
            "{}struct {}{}",
            visibility_prefix(&item.vis),
            item.ident,
            generics_suffix(&item.generics)
        )),
        target: None,
        trait_name: None,
        detail: struct_detail(&item.fields),
        children: Vec::new(),
    }
}

fn extract_enum_node(item: &ItemEnum) -> StructureNode {
    StructureNode {
        kind: "enum".to_string(),
        name: item.ident.to_string(),
        visibility: non_empty(visibility_to_string(&item.vis)),
        signature: normalize_token_string(format!(
            "{}enum {}{}",
            visibility_prefix(&item.vis),
            item.ident,
            generics_suffix(&item.generics)
        )),
        target: None,
        trait_name: None,
        detail: enum_detail(item),
        children: Vec::new(),
    }
}

fn extract_trait_node(item: &ItemTrait) -> StructureNode {
    let children = item
        .items
        .iter()
        .filter_map(|item| match item {
            TraitItem::Fn(method) => Some(StructureNode {
                kind: "function".to_string(),
                name: method.sig.ident.to_string(),
                visibility: None,
                signature: signature_to_string(&method.sig, None),
                target: None,
                trait_name: None,
                detail: function_detail(&method.sig, Some(method.default.is_some()), false),
                children: Vec::new(),
            }),
            _ => None,
        })
        .collect::<Vec<_>>();

    StructureNode {
        kind: "trait".to_string(),
        name: item.ident.to_string(),
        visibility: non_empty(visibility_to_string(&item.vis)),
        signature: normalize_token_string(format!(
            "{}trait {}{}",
            visibility_prefix(&item.vis),
            item.ident,
            generics_suffix(&item.generics)
        )),
        target: None,
        trait_name: None,
        detail: trait_detail(item),
        children,
    }
}

fn extract_impl_node(item: &ItemImpl) -> StructureNode {
    let target = type_to_string(&item.self_ty);
    let trait_name = item
        .trait_
        .as_ref()
        .map(|(_, path, _)| path.to_token_stream().to_string());

    let children = item
        .items
        .iter()
        .filter_map(|item| match item {
            ImplItem::Fn(method) => Some(StructureNode {
                kind: "function".to_string(),
                name: method.sig.ident.to_string(),
                visibility: non_empty(visibility_to_string(&method.vis)),
                signature: signature_to_string(&method.sig, Some(&method.vis)),
                target: None,
                trait_name: None,
                detail: function_detail(&method.sig, None, true),
                children: Vec::new(),
            }),
            _ => None,
        })
        .collect::<Vec<_>>();

    StructureNode {
        kind: "impl".to_string(),
        name: if let Some(trait_name) = &trait_name {
            format!("impl {} for {}", trait_name, target)
        } else {
            format!("impl {}", target)
        },
        visibility: None,
        signature: if let Some(trait_name) = &trait_name {
            format!("impl {} for {}", trait_name, target)
        } else {
            format!("impl {}", target)
        },
        target: Some(target),
        trait_name,
        detail: impl_detail(item),
        children,
    }
}

fn extract_mod_node(item: &ItemMod) -> StructureNode {
    let children = item
        .content
        .as_ref()
        .map(|(_, items)| items.iter().filter_map(extract_item).collect::<Vec<_>>())
        .unwrap_or_default();

    StructureNode {
        kind: "mod".to_string(),
        name: item.ident.to_string(),
        visibility: non_empty(visibility_to_string(&item.vis)),
        signature: normalize_token_string(format!(
            "{}mod {}",
            visibility_prefix(&item.vis),
            item.ident
        )),
        target: None,
        trait_name: None,
        detail: mod_detail(item),
        children,
    }
}

fn extract_type_node(item: &ItemType) -> StructureNode {
    StructureNode {
        kind: "type".to_string(),
        name: item.ident.to_string(),
        visibility: non_empty(visibility_to_string(&item.vis)),
        signature: normalize_token_string(format!(
            "{}type {} = {}",
            visibility_prefix(&item.vis),
            item.ident,
            type_to_string(&item.ty)
        )),
        target: None,
        trait_name: None,
        detail: None,
        children: Vec::new(),
    }
}

fn extract_const_node(item: &ItemConst) -> StructureNode {
    StructureNode {
        kind: "const".to_string(),
        name: item.ident.to_string(),
        visibility: non_empty(visibility_to_string(&item.vis)),
        signature: normalize_token_string(format!(
            "{}const {}: {}",
            visibility_prefix(&item.vis),
            item.ident,
            type_to_string(&item.ty)
        )),
        target: None,
        trait_name: None,
        detail: None,
        children: Vec::new(),
    }
}

fn extract_static_node(item: &ItemStatic) -> StructureNode {
    StructureNode {
        kind: "static".to_string(),
        name: item.ident.to_string(),
        visibility: non_empty(visibility_to_string(&item.vis)),
        signature: normalize_token_string(format!(
            "{}{}static {}: {}",
            visibility_prefix(&item.vis),
            if matches!(item.mutability, syn::StaticMutability::Mut(_)) {
                "mut "
            } else {
                ""
            },
            item.ident,
            type_to_string(&item.ty)
        )),
        target: None,
        trait_name: None,
        detail: non_empty_bool_detail(
            "mutable",
            matches!(item.mutability, syn::StaticMutability::Mut(_)),
        ),
        children: Vec::new(),
    }
}

fn extract_use_node(item: &ItemUse) -> StructureNode {
    StructureNode {
        kind: "use".to_string(),
        name: item.tree.to_token_stream().to_string(),
        visibility: non_empty(visibility_to_string(&item.vis)),
        signature: normalize_token_string(format!(
            "{}use {}",
            visibility_prefix(&item.vis),
            item.tree.to_token_stream()
        )),
        target: None,
        trait_name: None,
        detail: None,
        children: Vec::new(),
    }
}

fn fields_kind(fields: &Fields) -> &'static str {
    match fields {
        Fields::Named(_) => "named",
        Fields::Unnamed(_) => "unnamed",
        Fields::Unit => "unit",
    }
}

fn fields_summary(fields: &Fields) -> Vec<Value> {
    match fields {
        Fields::Named(named) => named
            .named
            .iter()
            .map(|field| {
                serde_json::json!({
                    "name": field.ident.as_ref().map(ToString::to_string),
                    "type": type_to_string(&field.ty),
                    "visibility": visibility_to_string(&field.vis)
                })
            })
            .collect(),
        Fields::Unnamed(unnamed) => unnamed
            .unnamed
            .iter()
            .map(|field| {
                serde_json::json!({
                    "name": Value::Null,
                    "type": type_to_string(&field.ty),
                    "visibility": visibility_to_string(&field.vis)
                })
            })
            .collect(),
        Fields::Unit => Vec::new(),
    }
}

fn struct_detail(fields: &Fields) -> Option<Value> {
    let summarized_fields = fields_summary(fields);
    let mut map = serde_json::Map::new();
    map.insert(
        "fields_kind".to_string(),
        Value::String(fields_kind(fields).to_string()),
    );
    if !summarized_fields.is_empty() {
        map.insert("fields".to_string(), Value::Array(summarized_fields));
    }
    if map.is_empty() {
        None
    } else {
        Some(Value::Object(map))
    }
}

fn enum_detail(item: &ItemEnum) -> Option<Value> {
    let variants = item
        .variants
        .iter()
        .map(|variant| {
            let summarized_fields = fields_summary(&variant.fields);
            let mut map = serde_json::Map::new();
            map.insert("name".to_string(), Value::String(variant.ident.to_string()));
            map.insert(
                "fields_kind".to_string(),
                Value::String(fields_kind(&variant.fields).to_string()),
            );
            if !summarized_fields.is_empty() {
                map.insert("fields".to_string(), Value::Array(summarized_fields));
            }
            Value::Object(map)
        })
        .collect::<Vec<_>>();

    if variants.is_empty() {
        None
    } else {
        Some(serde_json::json!({ "variants": variants }))
    }
}

fn function_detail(
    sig: &Signature,
    has_default_body: Option<bool>,
    in_impl: bool,
) -> Option<Value> {
    let mut map = serde_json::Map::new();
    if sig.asyncness.is_some() {
        map.insert("async".to_string(), Value::Bool(true));
    }
    if sig.constness.is_some() {
        map.insert("const".to_string(), Value::Bool(true));
    }
    if sig.unsafety.is_some() {
        map.insert("unsafe".to_string(), Value::Bool(true));
    }
    if in_impl && sig.receiver().is_some() {
        map.insert("method".to_string(), Value::Bool(true));
    }
    if let Some(has_default_body) = has_default_body {
        if has_default_body {
            map.insert("has_default_body".to_string(), Value::Bool(true));
        }
    }
    if let Some(return_type) = return_type_to_string(&sig.output) {
        map.insert("return_type".to_string(), Value::String(return_type));
    }
    if map.is_empty() {
        None
    } else {
        Some(Value::Object(map))
    }
}

fn trait_detail(item: &ItemTrait) -> Option<Value> {
    let mut map = serde_json::Map::new();
    if item.unsafety.is_some() {
        map.insert("unsafe".to_string(), Value::Bool(true));
    }
    if item.auto_token.is_some() {
        map.insert("auto".to_string(), Value::Bool(true));
    }
    let supertraits = item
        .supertraits
        .iter()
        .map(type_param_bound_to_string)
        .collect::<Vec<_>>();
    if !supertraits.is_empty() {
        map.insert(
            "supertraits".to_string(),
            Value::Array(supertraits.into_iter().map(Value::String).collect()),
        );
    }
    if map.is_empty() {
        None
    } else {
        Some(Value::Object(map))
    }
}

fn impl_detail(item: &ItemImpl) -> Option<Value> {
    let mut map = serde_json::Map::new();
    if item.unsafety.is_some() {
        map.insert("unsafe".to_string(), Value::Bool(true));
    }
    if map.is_empty() {
        None
    } else {
        Some(Value::Object(map))
    }
}

fn mod_detail(item: &ItemMod) -> Option<Value> {
    if item.content.is_some() {
        None
    } else {
        Some(serde_json::json!({ "inline": false }))
    }
}

fn non_empty(input: String) -> Option<String> {
    if input.is_empty() {
        None
    } else {
        Some(input)
    }
}

fn non_empty_bool_detail(key: &str, value: bool) -> Option<Value> {
    if value {
        Some(serde_json::json!({ key: true }))
    } else {
        None
    }
}

fn generics_suffix(generics: &syn::Generics) -> String {
    let rendered = generics.to_token_stream().to_string();
    if rendered.is_empty() {
        String::new()
    } else {
        format!(" {}", rendered)
    }
}

fn normalize_token_string(input: impl Into<String>) -> String {
    input
        .into()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn visibility_prefix(vis: &Visibility) -> String {
    let value = visibility_to_string(vis);
    if value.is_empty() {
        String::new()
    } else {
        format!("{} ", value)
    }
}

fn visibility_to_string(vis: &Visibility) -> String {
    match vis {
        Visibility::Public(_) => "pub".to_string(),
        Visibility::Restricted(v) => format!("pub({})", v.path.to_token_stream()),
        Visibility::Inherited => String::new(),
    }
}

fn signature_to_string(sig: &Signature, vis: Option<&Visibility>) -> String {
    let mut prefix = String::new();
    if let Some(vis) = vis {
        prefix.push_str(&visibility_prefix(vis));
    }
    prefix.push_str(&sig.to_token_stream().to_string());
    normalize_token_string(prefix)
}

fn return_type_to_string(output: &ReturnType) -> Option<String> {
    match output {
        ReturnType::Default => None,
        ReturnType::Type(_, ty) => Some(normalize_token_string(type_to_string(ty))),
    }
}

fn type_to_string(ty: &Type) -> String {
    normalize_token_string(ty.to_token_stream().to_string())
}

fn type_param_bound_to_string(bound: &syn::TypeParamBound) -> String {
    normalize_token_string(bound.to_token_stream().to_string())
}
#[derive(Debug)]
struct VueBlock {
    tag_name: String,
    attributes: Vec<(String, Option<String>)>,
    inner_content: String,
}

fn extract_ts_structure(
    content: &str,
    is_typescript: bool,
    path: &Path,
) -> std::result::Result<Vec<StructureNode>, String> {
    let mut parser = Parser::new();
    let language = if is_typescript {
        if path.extension().and_then(|ext| ext.to_str()) == Some("tsx") {
            LANGUAGE_TSX
        } else {
            LANGUAGE_TYPESCRIPT
        }
    } else {
        JAVASCRIPT_LANGUAGE
    };

    parser
        .set_language(&language.into())
        .map_err(|err| format!("{}: {}", path.display(), err))?;
    let syntax_err = "parser produced no syntax tree";
    let tree = parser
        .parse(content, None)
        .ok_or_else(|| format!("{}: {}", path.display(), syntax_err))?;

    let root = tree.root_node();
    let mut cursor = root.walk();
    let mut nodes = Vec::new();

    for child in root.named_children(&mut cursor) {
        if let Some(node) = extract_ts_node(child, content) {
            nodes.push(node);
        }
    }

    Ok(nodes)
}

fn extract_ts_node(node: Node<'_>, source: &str) -> Option<StructureNode> {
    match node.kind() {
        "function_declaration" => Some(ts_function_node(node, source, false)),
        "generator_function_declaration" => Some(ts_function_node(node, source, true)),
        "class_declaration" | "abstract_class_declaration" => Some(ts_class_node(node, source)),
        "lexical_declaration" | "variable_declaration" => Some(ts_variable_node(node, source)),
        "interface_declaration" => Some(ts_interface_node(node, source)),
        "type_alias_declaration" => Some(ts_type_alias_node(node, source)),
        "enum_declaration" => Some(ts_enum_node(node, source)),
        "import_statement" => Some(ts_import_node(node, source)),
        "export_statement" => ts_export_node(node, source),
        _ => None,
    }
}

fn ts_export_node(node: Node<'_>, source: &str) -> Option<StructureNode> {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if let Some(mut inner) = extract_ts_node(child, source) {
            inner.detail = merge_detail(inner.detail, serde_json::json!({ "export": true }));
            return Some(inner);
        }
    }

    Some(StructureNode {
        kind: "export".to_string(),
        name: snippet(node, source),
        visibility: None,
        signature: snippet(node, source),
        target: None,
        trait_name: None,
        detail: Some(serde_json::json!({ "export": true })),
        children: Vec::new(),
    })
}

fn ts_function_node(node: Node<'_>, source: &str, generator: bool) -> StructureNode {
    let name = node
        .child_by_field_name("name")
        .map(|n| snippet(n, source))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "anonymous".to_string());

    let mut detail = react_function_detail(node, source);
    if generator {
        detail.insert("generator".to_string(), Value::Bool(true));
    }

    StructureNode {
        kind: "function".to_string(),
        name,
        visibility: None,
        signature: snippet(node, source),
        target: None,
        trait_name: None,
        detail: if detail.is_empty() {
            None
        } else {
            Some(Value::Object(detail))
        },
        children: Vec::new(),
    }
}

fn ts_class_node(node: Node<'_>, source: &str) -> StructureNode {
    let name = node
        .child_by_field_name("name")
        .map(|n| snippet(n, source))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "anonymous".to_string());

    let detail = react_class_detail(node, source);
    let mut children = Vec::new();
    if let Some(body) = node.child_by_field_name("body") {
        let mut cursor = body.walk();
        for child in body.named_children(&mut cursor) {
            match child.kind() {
                "method_definition" | "abstract_method_signature" | "method_signature" => {
                    children.push(ts_method_node(child, source));
                }
                "public_field_definition" | "field_definition" | "property_signature" => {
                    children.push(ts_field_node(child, source));
                }
                _ => {}
            }
        }
    }

    StructureNode {
        kind: "class".to_string(),
        name,
        visibility: None,
        signature: snippet(node, source),
        target: None,
        trait_name: None,
        detail: if detail.is_empty() {
            None
        } else {
            Some(Value::Object(detail))
        },
        children,
    }
}

fn ts_method_node(node: Node<'_>, source: &str) -> StructureNode {
    let name = node
        .child_by_field_name("name")
        .map(|n| snippet(n, source))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| snippet(node, source));

    let mut detail = serde_json::Map::new();
    detail.insert("method".to_string(), Value::Bool(true));
    if node.kind() == "abstract_method_signature" || node.kind() == "method_signature" {
        detail.insert("declaration".to_string(), Value::Bool(true));
    }

    StructureNode {
        kind: "function".to_string(),
        name,
        visibility: None,
        signature: snippet(node, source),
        target: None,
        trait_name: None,
        detail: Some(Value::Object(detail)),
        children: Vec::new(),
    }
}

fn ts_field_node(node: Node<'_>, source: &str) -> StructureNode {
    let name = node
        .child_by_field_name("name")
        .map(|n| snippet(n, source))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| snippet(node, source));

    StructureNode {
        kind: "field".to_string(),
        name,
        visibility: None,
        signature: snippet(node, source),
        target: None,
        trait_name: None,
        detail: None,
        children: Vec::new(),
    }
}

fn ts_variable_node(node: Node<'_>, source: &str) -> StructureNode {
    let name = first_named_descendant_of_kind(node, "identifier", source)
        .unwrap_or_else(|| snippet(node, source));

    let mut detail = serde_json::Map::new();
    if let Some(kind) = node.child_by_field_name("kind") {
        detail.insert("binding".to_string(), Value::String(snippet(kind, source)));
    }
    if let Some(react_detail) = react_variable_detail(node, source) {
        for (key, value) in react_detail {
            detail.insert(key, value);
        }
    }

    StructureNode {
        kind: "variable".to_string(),
        name,
        visibility: None,
        signature: snippet(node, source),
        target: None,
        trait_name: None,
        detail: if detail.is_empty() {
            None
        } else {
            Some(Value::Object(detail))
        },
        children: Vec::new(),
    }
}

fn ts_interface_node(node: Node<'_>, source: &str) -> StructureNode {
    let name = node
        .child_by_field_name("name")
        .map(|n| snippet(n, source))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "anonymous".to_string());

    let mut children = Vec::new();
    if let Some(body) = node.child_by_field_name("body") {
        let mut cursor = body.walk();
        for child in body.named_children(&mut cursor) {
            if child.kind() == "property_signature" || child.kind() == "method_signature" {
                children.push(ts_field_or_method_signature_node(child, source));
            }
        }
    }

    StructureNode {
        kind: "interface".to_string(),
        name,
        visibility: None,
        signature: snippet(node, source),
        target: None,
        trait_name: None,
        detail: None,
        children,
    }
}

fn ts_field_or_method_signature_node(node: Node<'_>, source: &str) -> StructureNode {
    match node.kind() {
        "method_signature" => ts_method_node(node, source),
        _ => ts_field_node(node, source),
    }
}

fn ts_type_alias_node(node: Node<'_>, source: &str) -> StructureNode {
    let name = node
        .child_by_field_name("name")
        .map(|n| snippet(n, source))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "anonymous".to_string());

    StructureNode {
        kind: "type".to_string(),
        name,
        visibility: None,
        signature: snippet(node, source),
        target: None,
        trait_name: None,
        detail: None,
        children: Vec::new(),
    }
}

fn ts_enum_node(node: Node<'_>, source: &str) -> StructureNode {
    let name = node
        .child_by_field_name("name")
        .map(|n| snippet(n, source))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "anonymous".to_string());

    StructureNode {
        kind: "enum".to_string(),
        name,
        visibility: None,
        signature: snippet(node, source),
        target: None,
        trait_name: None,
        detail: None,
        children: Vec::new(),
    }
}

fn ts_import_node(node: Node<'_>, source: &str) -> StructureNode {
    let name = first_string_like_descendant(node, source).unwrap_or_else(|| snippet(node, source));

    StructureNode {
        kind: "import".to_string(),
        name,
        visibility: None,
        signature: snippet(node, source),
        target: None,
        trait_name: None,
        detail: None,
        children: Vec::new(),
    }
}

fn react_function_detail(node: Node<'_>, source: &str) -> serde_json::Map<String, Value> {
    let mut detail = serde_json::Map::new();
    let name = node
        .child_by_field_name("name")
        .map(|n| snippet(n, source))
        .unwrap_or_default();

    if looks_like_react_component_name(&name) && contains_jsx_descendant(node) {
        detail.insert("component".to_string(), Value::Bool(true));
        detail.insert(
            "react_kind".to_string(),
            Value::String("function".to_string()),
        );
        detail.insert("classic_react".to_string(), Value::Bool(true));
    }

    detail
}

fn react_class_detail(node: Node<'_>, source: &str) -> serde_json::Map<String, Value> {
    let mut detail = serde_json::Map::new();

    if let Some(class_name) = node.child_by_field_name("name").map(|n| snippet(n, source)) {
        if looks_like_react_component_name(&class_name) && class_has_render_method(node, source) {
            detail.insert("component".to_string(), Value::Bool(true));
            detail.insert("react_kind".to_string(), Value::String("class".to_string()));
            detail.insert("classic_react".to_string(), Value::Bool(true));
        }
    }
    let react_component = "React.Component";
    if let Some(heritage) = find_named_descendant_by_kind(node, "class_heritage") {
        let extends = snippet(heritage, source);
        if extends.contains(react_component)
            || extends.contains("Component")
            || extends.contains("PureComponent")
        {
            detail.insert("extends".to_string(), Value::String(extends.clone()));
            detail
                .entry("component".to_string())
                .or_insert(Value::Bool(true));
            detail
                .entry("react_kind".to_string())
                .or_insert(Value::String("class".to_string()));
            detail
                .entry("classic_react".to_string())
                .or_insert(Value::Bool(true));
        }
    }

    detail
}

fn react_variable_detail(node: Node<'_>, source: &str) -> Option<serde_json::Map<String, Value>> {
    if let Some(create_class) = react_create_class_detail(node, source) {
        return Some(create_class);
    }

    let name = first_named_descendant_of_kind(node, "identifier", source)?;
    if !looks_like_react_component_name(&name) {
        return None;
    }

    if contains_jsx_descendant(node) {
        let mut detail = serde_json::Map::new();
        detail.insert("component".to_string(), Value::Bool(true));
        detail.insert(
            "react_kind".to_string(),
            Value::String("function".to_string()),
        );
        detail.insert("classic_react".to_string(), Value::Bool(true));
        return Some(detail);
    }

    None
}

fn react_create_class_detail(
    node: Node<'_>,
    source: &str,
) -> Option<serde_json::Map<String, Value>> {
    let create_class = "React.createClass";
    let text = snippet(node, source);
    if !text.contains(create_class) {
        return None;
    }

    let mut detail = serde_json::Map::new();
    detail.insert("component".to_string(), Value::Bool(true));
    detail.insert(
        "react_kind".to_string(),
        Value::String("createClass".to_string()),
    );
    detail.insert("classic_react".to_string(), Value::Bool(true));
    Some(detail)
}

fn looks_like_react_component_name(name: &str) -> bool {
    name.chars()
        .next()
        .is_some_and(|ch| ch.is_ascii_uppercase())
}

fn class_has_render_method(node: Node<'_>, source: &str) -> bool {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "class_body" {
            let mut body_cursor = child.walk();
            for member in child.named_children(&mut body_cursor) {
                if member.kind() == "method_definition" {
                    if let Some(name) = member.child_by_field_name("name") {
                        if snippet(name, source) == "render" {
                            return true;
                        }
                    }
                }
            }
        }
    }
    false
}

fn contains_jsx_descendant(node: Node<'_>) -> bool {
    if matches!(
        node.kind(),
        "jsx_element" | "jsx_self_closing_element" | "jsx_fragment"
    ) {
        return true;
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if contains_jsx_descendant(child) {
            return true;
        }
    }

    false
}

fn find_named_descendant_by_kind<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    if node.kind() == kind {
        return Some(node);
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if let Some(found) = find_named_descendant_by_kind(child, kind) {
            return Some(found);
        }
    }

    None
}

fn first_named_descendant_of_kind(node: Node<'_>, kind: &str, source: &str) -> Option<String> {
    if node.kind() == kind {
        return Some(snippet(node, source));
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if let Some(found) = first_named_descendant_of_kind(child, kind, source) {
            return Some(found);
        }
    }

    None
}

fn first_string_like_descendant(node: Node<'_>, source: &str) -> Option<String> {
    if matches!(node.kind(), "string" | "string_fragment") {
        return Some(
            snippet(node, source)
                .trim_matches('"')
                .trim_matches('\'')
                .to_string(),
        );
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if let Some(found) = first_string_like_descendant(child, source) {
            return Some(found);
        }
    }

    None
}

fn snippet(node: Node<'_>, source: &str) -> String {
    node.utf8_text(source.as_bytes())
        .map(normalize_token_string)
        .unwrap_or_else(|_| node.kind().to_string())
}

fn extract_python_structure(
    content: &str,
    path: &Path,
) -> std::result::Result<Vec<StructureNode>, String> {
    let suite =
        ast::Suite::parse(content, &path.display().to_string()).map_err(|err| err.to_string())?;
    Ok(extract_python_suite(&suite))
}

fn extract_python_suite(suite: &[ast::Stmt]) -> Vec<StructureNode> {
    suite.iter().filter_map(extract_python_stmt).collect()
}

fn extract_python_stmt(stmt: &ast::Stmt) -> Option<StructureNode> {
    match stmt {
        ast::Stmt::Import(import) => Some(python_import_node(import)),
        ast::Stmt::ImportFrom(import) => Some(python_import_from_node(import)),
        ast::Stmt::FunctionDef(func) => Some(python_function_node(func, false)),
        ast::Stmt::AsyncFunctionDef(func) => Some(python_function_node(func, true)),
        ast::Stmt::ClassDef(class_def) => Some(python_class_node(class_def)),
        ast::Stmt::Assign(assign) => python_assign_node(assign),
        ast::Stmt::AnnAssign(assign) => python_ann_assign_node(assign),
        ast::Stmt::TypeAlias(alias) => Some(python_type_alias_node(alias)),
        _ => None,
    }
}

fn python_import_node(import: &ast::StmtImport) -> StructureNode {
    let names = import
        .names
        .iter()
        .map(python_alias_name)
        .collect::<Vec<_>>();

    StructureNode {
        kind: "import".to_string(),
        name: names.join(", "),
        visibility: None,
        signature: format!("import {}", names.join(", ")),
        target: None,
        trait_name: None,
        detail: None,
        children: Vec::new(),
    }
}

fn python_import_from_node(import: &ast::StmtImportFrom) -> StructureNode {
    let module = import
        .module
        .as_ref()
        .map(|m| m.to_string())
        .unwrap_or_else(|| ".".to_string());
    let names = import
        .names
        .iter()
        .map(python_alias_name)
        .collect::<Vec<_>>();

    let mut detail = serde_json::Map::new();
    if import.level.map(|l| l.to_usize()).unwrap_or(0) > 0 {
        detail.insert(
            "level".to_string(),
            Value::Number(import.level.unwrap().to_usize().into()),
        );
    }

    StructureNode {
        kind: "import".to_string(),
        name: module.clone(),
        visibility: None,
        signature: format!("from {} import {}", module, names.join(", ")),
        target: None,
        trait_name: None,
        detail: if detail.is_empty() {
            None
        } else {
            Some(Value::Object(detail))
        },
        children: Vec::new(),
    }
}

fn python_function_node<T>(func: &T, is_async: bool) -> StructureNode
where
    T: PythonFunctionLike,
{
    let mut detail = serde_json::Map::new();
    if is_async {
        detail.insert("async".to_string(), Value::Bool(true));
    }
    let decorators = func.decorator_names();
    if !decorators.is_empty() {
        detail.insert(
            "decorators".to_string(),
            Value::Array(decorators.into_iter().map(Value::String).collect()),
        );
    }

    StructureNode {
        kind: if is_async {
            "async_function".to_string()
        } else {
            "function".to_string()
        },
        name: func.name().to_string(),
        visibility: None,
        signature: python_function_signature(func, is_async),
        target: None,
        trait_name: None,
        detail: if detail.is_empty() {
            None
        } else {
            Some(Value::Object(detail))
        },
        children: Vec::new(),
    }
}

fn python_class_node(class_def: &ast::StmtClassDef) -> StructureNode {
    let mut detail = serde_json::Map::new();
    let bases = class_def
        .bases
        .iter()
        .map(python_expr_name)
        .collect::<Vec<_>>();
    if !bases.is_empty() {
        detail.insert(
            "bases".to_string(),
            Value::Array(bases.into_iter().map(Value::String).collect()),
        );
    }
    let decorators = class_def
        .decorator_list
        .iter()
        .map(python_expr_name)
        .collect::<Vec<_>>();
    if !decorators.is_empty() {
        detail.insert(
            "decorators".to_string(),
            Value::Array(decorators.into_iter().map(Value::String).collect()),
        );
    }

    let children = class_def
        .body
        .iter()
        .filter_map(|stmt| match stmt {
            ast::Stmt::FunctionDef(func) => Some(python_method_node(func, false)),
            ast::Stmt::AsyncFunctionDef(func) => Some(python_method_node(func, true)),
            ast::Stmt::Assign(assign) => python_assign_node(assign),
            ast::Stmt::AnnAssign(assign) => python_ann_assign_node(assign),
            _ => None,
        })
        .collect();

    StructureNode {
        kind: "class".to_string(),
        name: class_def.name.to_string(),
        visibility: None,
        signature: python_class_signature(class_def),
        target: None,
        trait_name: None,
        detail: if detail.is_empty() {
            None
        } else {
            Some(Value::Object(detail))
        },
        children,
    }
}

fn python_method_node<T>(func: &T, is_async: bool) -> StructureNode
where
    T: PythonFunctionLike,
{
    let mut node = python_function_node(func, is_async);
    node.detail = merge_detail(node.detail, serde_json::json!({ "method": true }));
    node
}

fn python_assign_node(assign: &ast::StmtAssign) -> Option<StructureNode> {
    let target = assign.targets.first()?;
    Some(StructureNode {
        kind: "variable".to_string(),
        name: python_expr_name(target),
        visibility: None,
        signature: format!("{} = ...", python_expr_name(target)),
        target: None,
        trait_name: None,
        detail: Some(serde_json::json!({ "assignment": true })),
        children: Vec::new(),
    })
}

fn python_ann_assign_node(assign: &ast::StmtAnnAssign) -> Option<StructureNode> {
    let target = assign.target.as_ref();
    Some(StructureNode {
        kind: "variable".to_string(),
        name: python_expr_name(target),
        visibility: None,
        signature: format!(
            "{}: {}",
            python_expr_name(target),
            python_expr_name(assign.annotation.as_ref())
        ),
        target: None,
        trait_name: None,
        detail: Some(serde_json::json!({ "annotation": true })),
        children: Vec::new(),
    })
}

fn python_type_alias_node(alias: &ast::StmtTypeAlias) -> StructureNode {
    StructureNode {
        kind: "type".to_string(),
        name: python_expr_name(alias.name.as_ref()),
        visibility: None,
        signature: format!("type {} = ...", python_expr_name(alias.name.as_ref())),
        target: None,
        trait_name: None,
        detail: None,
        children: Vec::new(),
    }
}

trait PythonFunctionLike {
    fn name(&self) -> &str;
    fn args(&self) -> &ast::Arguments;
    fn decorator_names(&self) -> Vec<String>;
}

impl PythonFunctionLike for ast::StmtFunctionDef {
    fn name(&self) -> &str {
        self.name.as_str()
    }

    fn args(&self) -> &ast::Arguments {
        &self.args
    }

    fn decorator_names(&self) -> Vec<String> {
        self.decorator_list.iter().map(python_expr_name).collect()
    }
}

impl PythonFunctionLike for ast::StmtAsyncFunctionDef {
    fn name(&self) -> &str {
        self.name.as_str()
    }

    fn args(&self) -> &ast::Arguments {
        &self.args
    }

    fn decorator_names(&self) -> Vec<String> {
        self.decorator_list.iter().map(python_expr_name).collect()
    }
}

fn python_function_signature<T>(func: &T, is_async: bool) -> String
where
    T: PythonFunctionLike,
{
    let args = python_arguments_signature(func.args());
    if is_async {
        format!("async def {}({})", func.name(), args)
    } else {
        format!("def {}({})", func.name(), args)
    }
}

fn python_class_signature(class_def: &ast::StmtClassDef) -> String {
    let bases = class_def
        .bases
        .iter()
        .map(python_expr_name)
        .collect::<Vec<_>>();
    if bases.is_empty() {
        format!("class {}", class_def.name)
    } else {
        format!("class {}({})", class_def.name, bases.join(", "))
    }
}

fn python_arguments_signature(args: &ast::Arguments) -> String {
    let mut parts = Vec::new();
    parts.extend(args.posonlyargs.iter().map(|arg| python_arg_name(&arg.def)));
    if !args.posonlyargs.is_empty() {
        parts.push("/".to_string());
    }
    parts.extend(args.args.iter().map(|arg| python_arg_name(&arg.def)));
    if let Some(vararg) = &args.vararg {
        parts.push(format!("*{}", python_arg_name(vararg)));
    }
    parts.extend(args.kwonlyargs.iter().map(|arg| python_arg_name(&arg.def)));
    if args.vararg.is_none() && !args.kwonlyargs.is_empty() {
        parts.push("*".to_string());
    }
    if let Some(kwarg) = &args.kwarg {
        parts.push(format!("**{}", python_arg_name(kwarg)));
    }
    parts.join(", ")
}

fn python_arg_name(arg: &ast::Arg) -> String {
    arg.arg.to_string()
}

fn python_alias_name(alias: &ast::Alias) -> String {
    match &alias.asname {
        Some(asname) => format!("{} as {}", alias.name, asname),
        None => alias.name.to_string(),
    }
}

fn python_expr_name(expr: &ast::Expr) -> String {
    match expr {
        ast::Expr::Name(name) => name.id.to_string(),
        ast::Expr::Attribute(attr) => {
            format!("{}.{}", python_expr_name(attr.value.as_ref()), attr.attr)
        }
        ast::Expr::Subscript(sub) => format!("{}[...]", python_expr_name(sub.value.as_ref())),
        ast::Expr::Call(call) => python_expr_name(call.func.as_ref()),
        ast::Expr::Tuple(tuple) => tuple
            .elts
            .iter()
            .map(python_expr_name)
            .collect::<Vec<_>>()
            .join(", "),
        ast::Expr::List(list) => format!(
            "[{}]",
            list.elts
                .iter()
                .map(python_expr_name)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        ast::Expr::Constant(value) => format!("{:?}", value.value),
        _ => "expr".to_string(),
    }
}

fn merge_detail(existing: Option<Value>, extra: Value) -> Option<Value> {
    match (existing, extra) {
        (Some(Value::Object(mut left)), Value::Object(right)) => {
            left.extend(right);
            Some(Value::Object(left))
        }
        (None, value) => Some(value),
        (Some(value), _) => Some(value),
    }
}

fn extract_svelte_structure(content: &str) -> Vec<StructureNode> {
    let blocks = extract_vue_blocks(content);
    let mut nodes = Vec::new();
    let mut covered = Vec::new();

    for block in blocks {
        let kind = match block.tag_name.as_str() {
            "script" => "script",
            "style" => "style",
            _ => continue,
        };

        let mut detail = serde_json::Map::new();
        for key in ["lang", "src", "context"] {
            if let Some(value) = vue_attr_value(&block, key) {
                detail.insert(key.to_string(), Value::String(value.to_string()));
            }
        }
        if has_vue_attr(&block, "module") {
            detail.insert("module".to_string(), Value::Bool(true));
        }
        let content_preview = block.inner_content.trim();
        if !content_preview.is_empty() {
            detail.insert("has_content".to_string(), Value::Bool(true));
            detail.insert(
                "preview".to_string(),
                Value::String(content_preview.chars().take(120).collect()),
            );
        }

        nodes.push(StructureNode {
            kind: kind.to_string(),
            name: kind.to_string(),
            visibility: None,
            signature: build_vue_signature(&block),
            target: None,
            trait_name: None,
            detail: if detail.is_empty() {
                None
            } else {
                Some(Value::Object(detail))
            },
            children: Vec::new(),
        });

        if let Some(range) = find_block_range(content, &block) {
            covered.push(range);
        }
    }

    let template_content = remove_ranges(content, &covered).trim().to_string();
    if !template_content.is_empty() {
        let mut detail = serde_json::Map::new();
        detail.insert("has_content".to_string(), Value::Bool(true));
        detail.insert(
            "preview".to_string(),
            Value::String(template_content.chars().take(120).collect()),
        );
        nodes.push(StructureNode {
            kind: "template".to_string(),
            name: "template".to_string(),
            visibility: None,
            signature: "<template>".to_string(),
            target: None,
            trait_name: None,
            detail: Some(Value::Object(detail)),
            children: Vec::new(),
        });
    }

    nodes
}

fn find_block_range(content: &str, block: &VueBlock) -> Option<(usize, usize)> {
    let start_pattern = build_vue_signature(block);
    let close_pattern = format!("</{}>", block.tag_name);
    let start = content.find(&start_pattern)?;
    let after_start = start + start_pattern.len();
    let relative_end = content[after_start..].find(&close_pattern)?;
    let end = after_start + relative_end + close_pattern.len();
    Some((start, end))
}

fn remove_ranges(content: &str, ranges: &[(usize, usize)]) -> String {
    if ranges.is_empty() {
        return content.to_string();
    }

    let mut sorted = ranges.to_vec();
    sorted.sort_by_key(|(start, _)| *start);

    let mut result = String::new();
    let mut cursor = 0usize;
    for (start, end) in sorted {
        if start > cursor {
            result.push_str(&content[cursor..start]);
        }
        cursor = cursor.max(end);
    }
    if cursor < content.len() {
        result.push_str(&content[cursor..]);
    }
    result
}

fn extract_vue_structure(content: &str) -> Vec<StructureNode> {
    extract_vue_blocks(content)
        .into_iter()
        .map(|block| vue_block_to_node(&block))
        .collect()
}

fn extract_vue_blocks(content: &str) -> Vec<VueBlock> {
    let bytes = content.as_bytes();
    let mut cursor = 0usize;
    let mut blocks = Vec::new();

    while let Some(start) = find_next_top_level_block(bytes, cursor) {
        let tag_name_start = start + 1;
        let mut name_end = tag_name_start;
        while name_end < bytes.len() && is_tag_name_char(bytes[name_end]) {
            name_end += 1;
        }

        if name_end == tag_name_start {
            cursor = start + 1;
            continue;
        }

        let tag_name = content[tag_name_start..name_end].to_string();
        let Some(open_end) = find_tag_end(bytes, name_end) else {
            break;
        };
        let raw_open_tag = &content[start + 1..open_end];
        let attributes = parse_tag_attributes(raw_open_tag, &tag_name);
        let close_tag = format!("</{}>", tag_name);
        let inner_start = open_end + 1;
        let Some(relative_close_start) = content[inner_start..].find(&close_tag) else {
            cursor = open_end + 1;
            continue;
        };
        let close_start = inner_start + relative_close_start;
        let inner_content = content[inner_start..close_start].to_string();

        blocks.push(VueBlock {
            tag_name,
            attributes,
            inner_content,
        });

        cursor = close_start + close_tag.len();
    }

    blocks
}

fn find_next_top_level_block(bytes: &[u8], mut cursor: usize) -> Option<usize> {
    while cursor < bytes.len() {
        if bytes[cursor] == b'<' {
            let next = bytes.get(cursor + 1).copied();
            if matches!(next, Some(b'!') | Some(b'/') | Some(b'?')) {
                cursor += 1;
                continue;
            }
            if matches!(next, Some(ch) if is_tag_name_start(ch)) {
                return Some(cursor);
            }
        }
        cursor += 1;
    }
    None
}

fn find_tag_end(bytes: &[u8], mut cursor: usize) -> Option<usize> {
    let mut quote: Option<u8> = None;
    let single_quote = 39u8;
    let double_quote = b'"';

    while cursor < bytes.len() {
        let ch = bytes[cursor];
        if let Some(active_quote) = quote {
            if ch == active_quote {
                quote = None;
            }
        } else if ch == single_quote || ch == double_quote {
            quote = Some(ch);
        } else if ch == b'>' {
            return Some(cursor);
        }
        cursor += 1;
    }

    None
}

fn is_tag_name_start(ch: u8) -> bool {
    ch.is_ascii_alphabetic()
}

fn is_tag_name_char(ch: u8) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, b'-' | b'_')
}

fn parse_tag_attributes(raw_open_tag: &str, tag_name: &str) -> Vec<(String, Option<String>)> {
    let remainder = raw_open_tag[tag_name.len()..].trim();
    let mut attrs = Vec::new();
    let mut chars = remainder.char_indices().peekable();

    while let Some((start_idx, ch)) = chars.next() {
        if ch.is_whitespace() {
            continue;
        }

        let mut end_idx = start_idx + ch.len_utf8();
        while let Some(&(idx, next_ch)) = chars.peek() {
            if next_ch.is_whitespace() || next_ch == '=' {
                break;
            }
            end_idx = idx + next_ch.len_utf8();
            chars.next();
        }

        let name = remainder[start_idx..end_idx]
            .trim_end_matches('/')
            .to_string();
        while let Some(&(_, next_ch)) = chars.peek() {
            if next_ch.is_whitespace() {
                chars.next();
            } else {
                break;
            }
        }

        let value = if let Some(&(_, '=')) = chars.peek() {
            chars.next();
            while let Some(&(_, next_ch)) = chars.peek() {
                if next_ch.is_whitespace() {
                    chars.next();
                } else {
                    break;
                }
            }

            if let Some(&(value_start, quote)) = chars.peek() {
                let single_quote = '\'';
                let double_quote = '"';
                if quote == double_quote || quote == single_quote {
                    chars.next();
                    let mut value_end = remainder.len();
                    while let Some((idx, next_ch)) = chars.next() {
                        if next_ch == quote {
                            value_end = idx;
                            break;
                        }
                    }
                    Some(remainder[value_start + quote.len_utf8()..value_end].to_string())
                } else {
                    let start = value_start;
                    let mut end = start + quote.len_utf8();
                    while let Some(&(idx, next_ch)) = chars.peek() {
                        if next_ch.is_whitespace() {
                            break;
                        }
                        end = idx + next_ch.len_utf8();
                        chars.next();
                    }
                    Some(remainder[start..end].trim_end_matches('/').to_string())
                }
            } else {
                None
            }
        } else {
            None
        };

        if !name.is_empty() {
            attrs.push((name, value));
        }
    }

    attrs
}

fn vue_block_to_node(block: &VueBlock) -> StructureNode {
    let kind = match block.tag_name.as_str() {
        "template" => "template",
        "script" => "script",
        "style" => "style",
        _ => "block",
    }
    .to_string();

    let name = if kind == "block" {
        block.tag_name.clone()
    } else {
        kind.clone()
    };

    let signature = build_vue_signature(block);
    let detail = vue_block_detail(block, &kind);

    StructureNode {
        kind,
        name,
        visibility: None,
        signature,
        target: None,
        trait_name: None,
        detail,
        children: Vec::new(),
    }
}

fn build_vue_signature(block: &VueBlock) -> String {
    let mut signature = format!("<{}", block.tag_name);
    for (name, value) in &block.attributes {
        signature.push(' ');
        signature.push_str(name);
        if let Some(value) = value {
            signature.push_str("=\"");
            signature.push_str(value);
            signature.push('"');
        }
    }
    signature.push('>');
    signature
}

fn vue_block_detail(block: &VueBlock, kind: &str) -> Option<Value> {
    let mut map = serde_json::Map::new();

    if kind == "script" && has_vue_attr(block, "setup") {
        map.insert("setup".to_string(), Value::Bool(true));
    }

    if kind == "style" {
        if has_vue_attr(block, "scoped") {
            map.insert("scoped".to_string(), Value::Bool(true));
        }
        if has_vue_attr(block, "module") {
            map.insert("module".to_string(), Value::Bool(true));
        }
    }

    for key in ["lang", "src"] {
        if let Some(value) = vue_attr_value(block, key) {
            map.insert(key.to_string(), Value::String(value.to_string()));
        }
    }

    if kind == "block" {
        map.insert("tag".to_string(), Value::String(block.tag_name.clone()));
    }

    let attribute_names = block
        .attributes
        .iter()
        .map(|(name, _)| Value::String(name.clone()))
        .collect::<Vec<_>>();
    if !attribute_names.is_empty() {
        map.insert("attributes".to_string(), Value::Array(attribute_names));
    }

    let content_preview = block.inner_content.trim();
    if !content_preview.is_empty() {
        let preview = content_preview.chars().take(120).collect::<String>();
        map.insert("has_content".to_string(), Value::Bool(true));
        map.insert("preview".to_string(), Value::String(preview));
    }

    if map.is_empty() {
        None
    } else {
        Some(Value::Object(map))
    }
}

fn has_vue_attr(block: &VueBlock, name: &str) -> bool {
    block
        .attributes
        .iter()
        .any(|(attr_name, _)| attr_name == name)
}

fn vue_attr_value<'a>(block: &'a VueBlock, name: &str) -> Option<&'a str> {
    block
        .attributes
        .iter()
        .find_map(|(attr_name, value)| (attr_name == name).then(|| value.as_deref()).flatten())
}

fn guess_lang_from_path(path: &Path) -> String {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("rs") => "rust".to_string(),
        Some("vue") => "vue".to_string(),
        Some("svelte") => "svelte".to_string(),
        Some("js") => "javascript".to_string(),
        Some("jsx") => "javascript".to_string(),
        Some("ts") => "typescript".to_string(),
        Some("tsx") => "typescript".to_string(),
        Some("py") => "python".to_string(),
        Some(other) => other.to_ascii_lowercase(),
        None => "unknown".to_string(),
    }
}

fn normalize_lang(lang: &str) -> String {
    match lang.trim().to_ascii_lowercase().as_str() {
        "rs" | "rust" => "rust".to_string(),
        "vue" => "vue".to_string(),
        "svelte" => "svelte".to_string(),
        "js" | "jsx" | "javascript" => "javascript".to_string(),
        "ts" | "tsx" | "typescript" => "typescript".to_string(),
        "py" | "python" => "python".to_string(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permissions::AllowAll;
    use crate::Tool;
    use std::sync::Arc;

    fn make_ctx(root: &Path) -> ToolContext {
        ToolContext {
            working_dir: root.to_path_buf(),
            session_id: "structure-test".into(),
            permissions: Arc::new(AllowAll),
            cost_tracker: Arc::new(CostTracker::new()),
            mcp_manager: None,
            extensions: Extensions::default(),
            network_policy: None,
        }
    }

    #[test]
    fn filesystem_toolset_includes_structure() {
        let names: Vec<String> = crate::filesystem()
            .into_iter()
            .map(|tool| tool.name().to_string())
            .collect();

        assert!(names.iter().any(|name| name == "Structure"));
    }
    #[tokio::test]
    async fn returns_structure_for_rust_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("sample.rs");
        tokio::fs::write(
            &path,
            r#"
pub struct User {
    pub id: u64,
    name: String,
}

impl User {
    pub fn new(name: String) -> Self {
        Self { id: 0, name }
    }
}
"#,
        )
        .await
        .unwrap();

        let tool = StructureTool;
        let result = tool
            .execute(
                serde_json::json!({
                    "file_path": path.display().to_string()
                }),
                &make_ctx(tmp.path()),
            )
            .await;

        assert!(!result.is_error, "{}", result.content);
        let meta = result.metadata.expect("metadata");
        assert_eq!(meta.get("lang").and_then(Value::as_str), Some("rust"));
        assert_eq!(meta.get("supported").and_then(Value::as_bool), Some(true));
        assert_eq!(meta.get("skeleton").and_then(Value::as_bool), Some(false));

        let nodes = meta
            .get("nodes")
            .and_then(Value::as_array)
            .expect("nodes array");
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[0].get("kind").and_then(Value::as_str), Some("struct"));
        assert_eq!(nodes[0].get("name").and_then(Value::as_str), Some("User"));
        assert_eq!(nodes[1].get("kind").and_then(Value::as_str), Some("impl"));

        let children = nodes[1]
            .get("children")
            .and_then(Value::as_array)
            .expect("impl children");
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].get("name").and_then(Value::as_str), Some("new"));
    }

    #[tokio::test]
    async fn extracts_nested_module_and_trait_items() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("sample.rs");
        tokio::fs::write(
            &path,
            r#"
pub mod api {
    pub trait Service {
        fn handle(&self) -> String;
    }
}
"#,
        )
        .await
        .unwrap();

        let tool = StructureTool;
        let result = tool
            .execute(
                serde_json::json!({
                    "file_path": path.display().to_string()
                }),
                &make_ctx(tmp.path()),
            )
            .await;

        assert!(!result.is_error, "{}", result.content);
        let meta = result.metadata.expect("metadata");
        let nodes = meta
            .get("nodes")
            .and_then(Value::as_array)
            .expect("nodes array");

        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].get("kind").and_then(Value::as_str), Some("mod"));
        let mod_children = nodes[0]
            .get("children")
            .and_then(Value::as_array)
            .expect("mod children");
        assert_eq!(mod_children.len(), 1);
        assert_eq!(
            mod_children[0].get("kind").and_then(Value::as_str),
            Some("trait")
        );

        let trait_children = mod_children[0]
            .get("children")
            .and_then(Value::as_array)
            .expect("trait children");
        assert_eq!(trait_children.len(), 1);
        assert_eq!(
            trait_children[0].get("name").and_then(Value::as_str),
            Some("handle")
        );
    }

    #[tokio::test]
    async fn omits_empty_visibility_and_normalizes_signatures() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("sample.rs");
        tokio::fs::write(
            &path,
            r#"
struct Pair<T>(T, T);

impl<T> Pair<T> {
    fn first(&self) -> &T {
        &self.0
    }
}
"#,
        )
        .await
        .unwrap();

        let tool = StructureTool;
        let result = tool
            .execute(
                serde_json::json!({
                    "file_path": path.display().to_string()
                }),
                &make_ctx(tmp.path()),
            )
            .await;

        assert!(!result.is_error, "{}", result.content);
        let meta = result.metadata.expect("metadata");
        let nodes = meta
            .get("nodes")
            .and_then(Value::as_array)
            .expect("nodes array");

        assert_eq!(nodes[0].get("visibility"), None);
        assert_eq!(
            nodes[0].get("signature").and_then(Value::as_str),
            Some("struct Pair < T >")
        );

        let children = nodes[1]
            .get("children")
            .and_then(Value::as_array)
            .expect("impl children");
        assert_eq!(children[0].get("visibility"), None);
        assert_eq!(
            children[0].get("signature").and_then(Value::as_str),
            Some("fn first (& self) -> & T")
        );
        assert_eq!(
            children[0]
                .get("detail")
                .and_then(|v: &Value| v.get("method"))
                .and_then(Value::as_bool),
            Some(true)
        );
    }

    #[tokio::test]
    async fn returns_structure_for_vue_sfc() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("Component.vue");
        tokio::fs::write(
            &path,
            r#"
<template>
  <div class="greeting">{{ msg }}</div>
</template>

<script setup lang="ts">
const msg = 'hi'
</script>

<style scoped>
greeting { color: red; }
</style>

<docs lang="md">
Hello docs
</docs>
"#,
        )
        .await
        .unwrap();

        let tool = StructureTool;
        let result = tool
            .execute(
                serde_json::json!({
                    "file_path": path.display().to_string()
                }),
                &make_ctx(tmp.path()),
            )
            .await;

        assert!(!result.is_error, "{}", result.content);
        let meta = result.metadata.expect("metadata");
        assert_eq!(meta.get("lang").and_then(Value::as_str), Some("vue"));
        assert_eq!(meta.get("supported").and_then(Value::as_bool), Some(true));
        assert_eq!(meta.get("skeleton").and_then(Value::as_bool), Some(false));

        let nodes = meta
            .get("nodes")
            .and_then(Value::as_array)
            .expect("nodes array");
        assert_eq!(nodes.len(), 4);

        assert_eq!(
            nodes[0].get("kind").and_then(Value::as_str),
            Some("template")
        );
        assert_eq!(
            nodes[0].get("signature").and_then(Value::as_str),
            Some("<template>")
        );

        assert_eq!(nodes[1].get("kind").and_then(Value::as_str), Some("script"));
        assert_eq!(
            nodes[1]
                .get("detail")
                .and_then(|v: &Value| v.get("setup"))
                .and_then(Value::as_bool),
            Some(true)
        );
        assert_eq!(
            nodes[1]
                .get("detail")
                .and_then(|v: &Value| v.get("lang"))
                .and_then(Value::as_str),
            Some("ts")
        );

        assert_eq!(nodes[2].get("kind").and_then(Value::as_str), Some("style"));
        assert_eq!(
            nodes[2]
                .get("detail")
                .and_then(|v: &Value| v.get("scoped"))
                .and_then(Value::as_bool),
            Some(true)
        );

        assert_eq!(nodes[3].get("kind").and_then(Value::as_str), Some("block"));
        assert_eq!(nodes[3].get("name").and_then(Value::as_str), Some("docs"));
        assert_eq!(
            nodes[3]
                .get("detail")
                .and_then(|v: &Value| v.get("tag"))
                .and_then(Value::as_str),
            Some("docs")
        );
    }

    #[tokio::test]
    async fn returns_structure_for_javascript_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("sample.js");
        tokio::fs::write(
            &path,
            r#"
import { ref } from 'vue';

export function greet(name) {
  return `Hello ${name}`;
}

class Person {
  age = 1;
  speak() { return greet('x'); }
}

const count = ref(0);
"#,
        )
        .await
        .unwrap();

        let tool = StructureTool;
        let result = tool
            .execute(
                serde_json::json!({
                    "file_path": path.display().to_string()
                }),
                &make_ctx(tmp.path()),
            )
            .await;

        assert!(!result.is_error, "{}", result.content);
        let meta = result.metadata.expect("metadata");
        assert_eq!(meta.get("lang").and_then(Value::as_str), Some("javascript"));
        let nodes = meta
            .get("nodes")
            .and_then(Value::as_array)
            .expect("nodes array");

        assert_eq!(nodes[0].get("kind").and_then(Value::as_str), Some("import"));
        assert_eq!(nodes[0].get("name").and_then(Value::as_str), Some("vue"));
        assert_eq!(
            nodes[1].get("kind").and_then(Value::as_str),
            Some("function")
        );
        assert_eq!(nodes[1].get("name").and_then(Value::as_str), Some("greet"));
        assert_eq!(
            nodes[1]
                .get("detail")
                .and_then(|v: &Value| v.get("export"))
                .and_then(Value::as_bool),
            Some(true)
        );
        assert_eq!(nodes[2].get("kind").and_then(Value::as_str), Some("class"));
        assert_eq!(nodes[2].get("name").and_then(Value::as_str), Some("Person"));
        assert_eq!(
            nodes[3].get("kind").and_then(Value::as_str),
            Some("variable")
        );
    }

    #[tokio::test]
    async fn detects_classic_react_components_in_jsx() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("App.jsx");
        tokio::fs::write(
            &path,
            r#"
import React, { Component } from 'react';

function Header() {
  return <h1>Hello</h1>;
}

class App extends React.Component {
  render() {
    return <Header />;
  }
}

const Legacy = React.createClass({
  render() { return <div />; }
});
"#,
        )
        .await
        .unwrap();

        let tool = StructureTool;
        let result = tool
            .execute(
                serde_json::json!({
                    "file_path": path.display().to_string()
                }),
                &make_ctx(tmp.path()),
            )
            .await;

        assert!(!result.is_error, "{}", result.content);
        let meta = result.metadata.expect("metadata");
        assert_eq!(meta.get("lang").and_then(Value::as_str), Some("javascript"));
        let nodes = meta
            .get("nodes")
            .and_then(Value::as_array)
            .expect("nodes array");

        assert_eq!(nodes[1].get("name").and_then(Value::as_str), Some("Header"));
        assert_eq!(
            nodes[1]
                .get("detail")
                .and_then(|v: &Value| v.get("component"))
                .and_then(Value::as_bool),
            Some(true)
        );
        assert_eq!(
            nodes[1]
                .get("detail")
                .and_then(|v: &Value| v.get("react_kind"))
                .and_then(Value::as_str),
            Some("function")
        );

        assert_eq!(nodes[2].get("name").and_then(Value::as_str), Some("App"));
        assert_eq!(
            nodes[2]
                .get("detail")
                .and_then(|v: &Value| v.get("component"))
                .and_then(Value::as_bool),
            Some(true)
        );
        assert_eq!(
            nodes[2]
                .get("detail")
                .and_then(|v: &Value| v.get("react_kind"))
                .and_then(Value::as_str),
            Some("class")
        );

        assert_eq!(nodes[3].get("name").and_then(Value::as_str), Some("Legacy"));
        assert_eq!(
            nodes[3]
                .get("detail")
                .and_then(|v: &Value| v.get("react_kind"))
                .and_then(Value::as_str),
            Some("createClass")
        );
    }

    #[tokio::test]
    async fn returns_structure_for_svelte_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("Widget.svelte");
        tokio::fs::write(
            &path,
            r#"
<script context="module" lang="ts">
  export const prerender = true;
</script>

<script>
  let count = 0;
</script>

<style>
  .count { color: red; }
</style>

<div class="count">{count}</div>
"#,
        )
        .await
        .unwrap();

        let tool = StructureTool;
        let result = tool
            .execute(
                serde_json::json!({
                    "file_path": path.display().to_string()
                }),
                &make_ctx(tmp.path()),
            )
            .await;

        assert!(!result.is_error, "{}", result.content);
        let meta = result.metadata.expect("metadata");
        assert_eq!(meta.get("lang").and_then(Value::as_str), Some("svelte"));
        assert_eq!(meta.get("supported").and_then(Value::as_bool), Some(true));
        assert_eq!(meta.get("skeleton").and_then(Value::as_bool), Some(false));

        let nodes = meta
            .get("nodes")
            .and_then(Value::as_array)
            .expect("nodes array");
        assert_eq!(nodes.len(), 4);

        assert_eq!(nodes[0].get("kind").and_then(Value::as_str), Some("script"));
        assert_eq!(
            nodes[0]
                .get("detail")
                .and_then(|v: &Value| v.get("context"))
                .and_then(Value::as_str),
            Some("module")
        );
        assert_eq!(
            nodes[0]
                .get("detail")
                .and_then(|v: &Value| v.get("lang"))
                .and_then(Value::as_str),
            Some("ts")
        );

        assert_eq!(nodes[1].get("kind").and_then(Value::as_str), Some("script"));
        assert_eq!(nodes[2].get("kind").and_then(Value::as_str), Some("style"));
        assert_eq!(
            nodes[3].get("kind").and_then(Value::as_str),
            Some("template")
        );
        assert_eq!(
            nodes[3].get("signature").and_then(Value::as_str),
            Some("<template>")
        );
    }

    #[tokio::test]
    async fn returns_structure_for_python_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("sample.py");
        tokio::fs::write(
            &path,
            r#"
import os
from pkg.mod import Thing as Alias

VALUE: int = 1

class Service(BaseService):
    role = "api"

    @classmethod
    def build(cls, name, *, debug=False):
        return cls()

    async def fetch(self, item_id):
        return item_id

async def run(task):
    return task
"#,
        )
        .await
        .unwrap();

        let tool = StructureTool;
        let result = tool
            .execute(
                serde_json::json!({
                    "file_path": path.display().to_string()
                }),
                &make_ctx(tmp.path()),
            )
            .await;

        assert!(!result.is_error, "{}", result.content);
        let meta = result.metadata.expect("metadata");
        assert_eq!(meta.get("lang").and_then(Value::as_str), Some("python"));
        assert_eq!(meta.get("supported").and_then(Value::as_bool), Some(true));
        assert_eq!(meta.get("skeleton").and_then(Value::as_bool), Some(false));

        let nodes = meta
            .get("nodes")
            .and_then(Value::as_array)
            .expect("nodes array");

        assert_eq!(nodes[0].get("kind").and_then(Value::as_str), Some("import"));
        assert_eq!(nodes[1].get("kind").and_then(Value::as_str), Some("import"));
        assert_eq!(
            nodes[2].get("kind").and_then(Value::as_str),
            Some("variable")
        );
        assert_eq!(nodes[2].get("name").and_then(Value::as_str), Some("VALUE"));
        assert_eq!(nodes[3].get("kind").and_then(Value::as_str), Some("class"));
        assert_eq!(
            nodes[3].get("name").and_then(Value::as_str),
            Some("Service")
        );
        assert_eq!(
            nodes[3]
                .get("detail")
                .and_then(|v: &Value| v.get("bases"))
                .and_then(Value::as_array)
                .and_then(|bases| bases.first())
                .and_then(Value::as_str),
            Some("BaseService")
        );

        let class_children = nodes[3]
            .get("children")
            .and_then(Value::as_array)
            .expect("class children");
        assert_eq!(
            class_children[0].get("kind").and_then(Value::as_str),
            Some("variable")
        );
        assert_eq!(
            class_children[1].get("kind").and_then(Value::as_str),
            Some("function")
        );
        assert_eq!(
            class_children[1].get("name").and_then(Value::as_str),
            Some("build")
        );
        assert_eq!(
            class_children[2].get("kind").and_then(Value::as_str),
            Some("async_function")
        );

        assert_eq!(
            nodes[4].get("kind").and_then(Value::as_str),
            Some("async_function")
        );
        assert_eq!(nodes[4].get("name").and_then(Value::as_str), Some("run"));
    }
    #[tokio::test]
    async fn returns_structure_for_typescript_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("sample.ts");
        tokio::fs::write(
            &path,
            r#"
export interface User {
  id: number;
  greet(name: string): string;
}

export type Id = string | number;

export enum State {
  Ready,
}

export class Service {
  endpoint: string;
  fetch(id: Id): Promise<User> { throw new Error('x'); }
}
"#,
        )
        .await
        .unwrap();

        let tool = StructureTool;
        let result = tool
            .execute(
                serde_json::json!({
                    "file_path": path.display().to_string()
                }),
                &make_ctx(tmp.path()),
            )
            .await;

        assert!(!result.is_error, "{}", result.content);
        let meta = result.metadata.expect("metadata");
        assert_eq!(meta.get("lang").and_then(Value::as_str), Some("typescript"));
        let nodes = meta
            .get("nodes")
            .and_then(Value::as_array)
            .expect("nodes array");

        assert_eq!(
            nodes[0].get("kind").and_then(Value::as_str),
            Some("interface")
        );
        assert_eq!(nodes[0].get("name").and_then(Value::as_str), Some("User"));
        assert_eq!(nodes[1].get("kind").and_then(Value::as_str), Some("type"));
        assert_eq!(nodes[1].get("name").and_then(Value::as_str), Some("Id"));
        assert_eq!(nodes[2].get("kind").and_then(Value::as_str), Some("enum"));
        assert_eq!(nodes[2].get("name").and_then(Value::as_str), Some("State"));
        assert_eq!(nodes[3].get("kind").and_then(Value::as_str), Some("class"));
        assert_eq!(
            nodes[3].get("name").and_then(Value::as_str),
            Some("Service")
        );
    }
    #[tokio::test]
    async fn lang_override_is_respected_for_unsupported_lang() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("sample.txt");
        tokio::fs::write(&path, "hello\n").await.unwrap();

        let tool = StructureTool;
        let result = tool
            .execute(
                serde_json::json!({
                    "file_path": path.display().to_string(),
                    "lang": "python"
                }),
                &make_ctx(tmp.path()),
            )
            .await;

        assert!(!result.is_error, "{}", result.content);
        let meta = result.metadata.expect("metadata");
        assert_eq!(meta.get("lang").and_then(Value::as_str), Some("python"));
        assert_eq!(meta.get("supported").and_then(Value::as_bool), Some(true));
        assert_eq!(meta.get("skeleton").and_then(Value::as_bool), Some(false));
    }
}
