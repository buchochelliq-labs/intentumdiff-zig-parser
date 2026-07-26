//! Zig parser plugin — full-parse mode.
//!
//! Handles `.zig` files.
//! The plugin parses source with Tree-sitter inside Rust/Wasm.
//!
//! Semantic model:
//! - `container_declaration` (struct/enum/union/opaque)   → class-like
//! - `function_declaration`                               → method-like
//! - `test_declaration`                                   → method-like (test blocks)
//! - Top-level `var_declaration` / `const_declaration`    → tracked but not method-like
//! - Labels: function → identifier child; container → parent const name if available;
//!   test → string literal content.

use intentdiff_plugin_sdk::{
    cst::CstNode,
    hash::structural_hash_with_memo,
    tree::{SemanticNode, SemanticNodeBuilder},
};

wit_bindgen::generate!({
    path: "wit/plugin.wit",
    world: "parser-plugin",
});

use crate::exports::intentdiff::plugin::parser::ExamplePair;
use crate::exports::intentdiff::plugin::parser::Guest;
use crate::exports::intentdiff::plugin::parser::LanguageInfoRecord;
use crate::exports::intentdiff::plugin::parser::ParserMode;

const PLUGIN_METADATA: &str = include_str!("../plugin_metadata.info");

fn language_info_for(ids: Vec<String>) -> Vec<LanguageInfoRecord> {
    let metadata = intentdiff_plugin_sdk::metadata::parse_plugin_metadata(PLUGIN_METADATA);
    ids.into_iter()
        .map(|language_id| {
            let info = metadata.language_or_default(&language_id);
            LanguageInfoRecord {
                language_id: info.language_id,
                language_name: info.language_name,
                language_short_name: info.language_short_name,
                monaco_language: info.monaco_language,
                default_filename: info.default_filename,
                language_file_extensions: info.language_file_extensions,
                author: metadata.author().to_string(),
                plugin_version: metadata.plugin_version().to_string(),
                last_updated: metadata.last_updated().to_string(),
            }
        })
        .collect()
}
struct ZigParser;

const TRIVIA: &[&str] = &["line_comment", "doc_comment", "container_doc_comment"];

const SEMANTIC_TYPES: &[&str] = &[
    // Root
    "source_file",
    // Top-level declarations
    "function_declaration",
    "function_proto",
    "var_declaration",
    "const_declaration",
    "test_declaration",
    "comptime_declaration",
    "usingnamespace_declaration",
    // Container types (struct / enum / union / opaque)
    "container_declaration",
    "container_field",
    // Statements
    "if_statement",
    "while_statement",
    "for_statement",
    "switch_expression",
    "block",
    "return_expression",
    "defer_statement",
    "errdefer_statement",
    "break_expression",
    "continue_expression",
    "assign_expression",
    // Expressions
    "call_expression",
    "builtin_call_expression",
    "field_access",
    "pointer_deref",
    "array_access",
    "struct_init",
    "array_init",
    "catch_expression",
    "try_expression",
    "await_expression",
    "async_expression",
    "error_union",
    "error_set",
    // Literals
    "string",
    "multiline_string",
    // Grammar-TRUE literal kinds (the java #72 lesson): tree-sitter-zig emits
    // "integer"/"float", not "integer_literal"/"float_literal".
    "integer",
    "float",
    "char_literal",
    "true",
    "false",
    "null",
    "undefined",
    // Names
    "identifier",
    "builtin_identifier",
    // Types
    "type_expr",
    "optional_type",
    "error_union_type",
    "pointer_type",
    "array_type",
    "slice_type",
];

fn is_semantic(node_type: &str) -> bool {
    SEMANTIC_TYPES.contains(&node_type)
}

fn is_class_like(node_type: &str) -> bool {
    node_type == "container_declaration"
}

fn is_method_like(node_type: &str) -> bool {
    matches!(node_type, "function_declaration" | "test_declaration")
}

fn label_for(node: &CstNode) -> String {
    if node.is_leaf() {
        return node.text_or_empty().to_string();
    }
    // Literal containers label with their captured source text (SDK-shared, issue #47).
    if let Some(label) = intentdiff_plugin_sdk::ts_convert::literal_label(node) {
        return label;
    }
    match node.node_type.as_str() {
        "function_declaration" | "function_proto" => {
            for child in &node.children {
                if child.node_type == "identifier" {
                    return child.text_or_empty().to_string();
                }
            }
        }
        "var_declaration" | "const_declaration" => {
            for child in &node.children {
                if child.node_type == "identifier" {
                    return child.text_or_empty().to_string();
                }
            }
        }
        "test_declaration" => {
            // `test "name" { ... }` — first string child is the test name
            for child in &node.children {
                if child.node_type == "string" {
                    let text = child.text_or_empty();
                    // Strip surrounding quotes
                    return text.trim_matches('"').to_string();
                }
                if child.node_type == "identifier" {
                    return child.text_or_empty().to_string();
                }
            }
            return "(anonymous test)".to_string();
        }
        "container_declaration" => {
            // Containers are often anonymous (`struct { ... }`); label as the keyword
            for child in &node.children {
                if matches!(
                    child.node_type.as_str(),
                    "struct" | "enum" | "union" | "opaque"
                ) {
                    return child.text_or_empty().to_string();
                }
            }
            // If assigned to a const, the parent const_declaration will label it
            return "(container)".to_string();
        }
        "call_expression" | "builtin_call_expression" => {
            if let Some(first) = node.children.first() {
                return first.text_or_empty().to_string();
            }
        }
        _ => {}
    }
    for child in &node.children {
        if child.node_type == "identifier" {
            return child.text_or_empty().to_string();
        }
    }
    node.node_type.clone()
}

fn convert(
    node: &CstNode,
    id_prefix: &str,
    parent_class: Option<&str>,
    memo: &mut std::collections::HashMap<usize, String>,
) -> Option<SemanticNode> {
    convert_semantic_classed(
        node,
        id_prefix,
        parent_class,
        memo,
        &|t| TRIVIA.contains(&t),
        &is_semantic,
        &is_class_like,
        &is_method_like,
        &label_for,
    )
}



use intentdiff_plugin_sdk::ts_convert::{convert_semantic_classed, node_to_cst};

fn parse_source(source: &str) -> Result<CstNode, String> {
    let mut parser = tree_sitter::Parser::new();
    let lang = tree_sitter_zig::LANGUAGE.into();
    parser
        .set_language(&lang)
        .map_err(|_| "Failed to load zig grammar".to_string())?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| "Parse failed".to_string())?;
    Ok(node_to_cst(tree.root_node(), source.as_bytes()))
}

fn process_impl(source: &str) -> String {
    let root: CstNode = match parse_source(source) {
        Ok(n) => n,
        Err(e) => return format!(r#"{{\"error\":\"{}\"}}"#, e),
    };
    let mut memo = std::collections::HashMap::new();
    let sem = match convert(&root, "0", None, &mut memo) {
        Some(n) => n,
        None => return r#"{"error":"Empty semantic tree"}"#.to_string(),
    };
    match serde_json::to_string(&sem) {
        Ok(s) => s,
        Err(e) => format!(r#"{{"error":"Serialisation error: {}"}}"#, e),
    }
}

impl Guest for ZigParser {
    fn get_parser_mode() -> ParserMode {
        ParserMode::FullParse
    }
    fn grammar_id() -> String {
        "zig".to_string()
    }
    fn detect_language(filename: String, _content: String) -> String {
        if filename.to_lowercase().ends_with(".zig") {
            return "zig".to_string();
        }
        String::new()
    }
    fn preprocess_source(source: String) -> String {
        source
    }
    fn process(input: String, _language: String, _filename: String) -> String {
        process_impl(&input)
    }
    fn trivia_node_types() -> Vec<String> {
        TRIVIA.iter().map(|s| s.to_string()).collect()
    }
    fn language_ids() -> Vec<String> {
        vec!["zig".to_string()]
    }
    fn language_info() -> Vec<LanguageInfoRecord> {
        language_info_for(Self::language_ids())
    }
    fn priority() -> i32 {
        0
    }

    fn example(_language: String) -> ExamplePair {
        ExamplePair {
            old: "const std = @import(\"std\");\n\nfn add(a: i32, b: i32) i32 {\n    return a + b;\n}\n\npub fn main() void {\n    std.debug.print(\"{d}\\n\", .{add(3, 4)});\n}\n".to_string(),
            new: "const std = @import(\"std\");\n\nfn add(a: i32, b: i32) i32 {\n    return a + b;\n}\n\nfn multiply(a: i32, b: i32) i32 {\n    return a * b;\n}\n\npub fn main() void {\n    std.debug.print(\"add: {d}\\n\", .{add(3, 4)});\n    std.debug.print(\"mul: {d}\\n\", .{multiply(3, 4)});\n}\n".to_string(),
        }
    }
}
export!(ZigParser);

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exports::intentdiff::plugin::parser::Guest;
    use intentdiff_plugin_sdk::testing as t;

    #[test]
    fn grammar_id_nonempty() {
        assert!(!ZigParser::grammar_id().is_empty());
    }

    #[test]
    fn language_ids_contain_grammar_id() {
        let gid = ZigParser::grammar_id();
        let ids = ZigParser::language_ids();
        assert!(
            ids.contains(&gid),
            "language_ids {:?} must contain {:?}",
            ids,
            gid
        );
    }

    #[test]
    fn detect_language_known_ext() {
        let r = ZigParser::detect_language("test.zig".to_string(), "".to_string());
        assert_eq!(r.as_str(), "zig");
    }

    #[test]
    fn detect_language_unknown_ext() {
        let r =
            ZigParser::detect_language("test.xyz_notareal_ext_9z8y".to_string(), "".to_string());
        assert_eq!(r.as_str(), "");
    }

    #[test]
    fn parser_mode_is_full_parse() {
        assert!(matches!(
            ZigParser::get_parser_mode(),
            ParserMode::FullParse
        ));
    }

    #[test]
    fn process_impl_accepts_raw_example_source() {
        let example = ZigParser::example(ZigParser::grammar_id());
        let out = process_impl(&example.old);
        t::assert_valid_json(&out, "process(raw example)");
        assert!(!out.contains("\"error\""), "{out}");
    }
    #[test]
    fn process_impl_empty_returns_valid_json() {
        let out = process_impl("");
        t::assert_valid_json(&out, "process(empty)");
    }

    #[test]
    fn process_impl_whitespace_returns_valid_json() {
        let out = process_impl("   \n  ");
        t::assert_valid_json(&out, "process(whitespace)");
    }
}
