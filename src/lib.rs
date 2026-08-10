//! ABAP (SAP) parser plugin — full-parse mode.
//!
//! Handles `.abap` files. The primary parser is tree-sitter-backed and maps the
//! concrete syntax tree into a compact semantic tree. It falls back to the
//! conservative scanner for unsupported ABAP blocks such as `FORM`.
//!
//! Semantic nodes produced:
//!   program          — root node
//!   report           — REPORT / PROGRAM declaration (label = program name)
//!   class_definition — CLASS … DEFINITION block (label = class name)
//!   class_impl       — CLASS … IMPLEMENTATION block (label = class name)
//!   interface        — INTERFACE … ENDINTERFACE block (label = name)
//!   method           — METHOD … ENDMETHOD block inside a class (label = name)
//!   form             — FORM … ENDFORM subroutine (label = name)
//!   function_module  — FUNCTION … ENDFUNCTION module (label = name)
//!   module           — MODULE … ENDMODULE screen module (label = name)
//!
//! Block nodes include conservative child nodes for signatures and body
//! statements. This keeps changed ABAP routines from collapsing to identical
//! shallow nodes when no full grammar is available.

use intentumdiff_plugin_sdk::tree::{SemanticNode, SemanticNodeBuilder};

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
    let metadata = intentumdiff_plugin_sdk::metadata::parse_plugin_metadata(PLUGIN_METADATA);
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
struct AbapParser;

// ---------------------------------------------------------------------------
// Node helpers
// ---------------------------------------------------------------------------

fn leaf(id: &str, node_type: &str, label: &str, line: u32) -> SemanticNode {
    SemanticNodeBuilder::new(id, node_type, label, line, 0, line, 0, String::new()).build()
}

fn leaf_span(id: &str, node_type: &str, label: &str, line: u32, end_col: u32) -> SemanticNode {
    SemanticNodeBuilder::new(id, node_type, label, line, 0, line, end_col, String::new()).build()
}

fn block_node(
    id: &str,
    node_type: &str,
    label: &str,
    start: u32,
    end: u32,
    children: Vec<SemanticNode>,
) -> SemanticNode {
    SemanticNodeBuilder::new(id, node_type, label, start, 0, end, 0, String::new())
        .children(children)
        .build()
}

// ---------------------------------------------------------------------------
// Parse stack frame
// ---------------------------------------------------------------------------

struct Frame {
    id: String,
    node_type: &'static str,
    label: String,
    start_line: u32,
    children: Vec<SemanticNode>,
}

/// Add `node` to the innermost open frame's children, or to root if the stack
/// is empty.
fn push_to(stack: &mut Vec<Frame>, root: &mut Vec<SemanticNode>, node: SemanticNode) {
    if let Some(parent) = stack.last_mut() {
        parent.children.push(node);
    } else {
        root.push(node);
    }
}

/// Strip trailing dots and commas from an ABAP token.
fn extract_name(words: &[&str], pos: usize) -> String {
    words
        .get(pos)
        .copied()
        .unwrap_or("(anonymous)")
        .trim_end_matches(|c: char| c == '.' || c == ',')
        .to_string()
}

fn keyword(word: &str) -> &str {
    word.split('.')
        .next()
        .unwrap_or(word)
        .trim_end_matches(|c: char| c == '.' || c == ':')
}

fn signature_children(
    parent_id: &str,
    words: &[&str],
    from: usize,
    line: u32,
    end_col: u32,
) -> Vec<SemanticNode> {
    let signature = words
        .iter()
        .skip(from)
        .map(|word| word.trim_end_matches(|c: char| c == '.' || c == ','))
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if signature.is_empty() {
        Vec::new()
    } else {
        vec![leaf_span(
            &format!("{}.0", parent_id),
            "signature",
            &signature,
            line,
            end_col,
        )]
    }
}

fn data_label(trimmed: &str) -> Option<String> {
    let open = trimmed.find('(')?;
    let close = trimmed[open + 1..].find(')')? + open + 1;
    let name = trimmed[open + 1..close].trim();
    if name.is_empty() {
        None
    } else {
        Some(name.to_uppercase())
    }
}

fn write_label(trimmed: &str) -> String {
    trimmed
        .trim_end_matches('.')
        .trim()
        .trim_start_matches(|c: char| c.is_ascii_alphabetic())
        .trim_start_matches(':')
        .trim()
        .to_string()
}

fn statement_node(id: &str, trimmed: &str, upper: &str, line: u32, end_col: u32) -> SemanticNode {
    let stripped = trimmed.trim_end_matches('.').trim();
    let upper_stripped = upper.trim_end_matches('.').trim();
    let first_word = upper_stripped.split_whitespace().next().unwrap_or("");
    let first_kw = keyword(first_word);

    let (node_type, label) = if first_kw.starts_with("DATA") {
        (
            "data_declaration",
            data_label(trimmed).unwrap_or_else(|| upper_stripped.to_string()),
        )
    } else if first_kw == "WRITE" {
        ("write_statement", write_label(trimmed))
    } else if first_kw == "METHODS" {
        let words = upper_stripped.split_whitespace().collect::<Vec<_>>();
        ("method_signature", extract_name(&words, 1))
    } else if stripped.contains('=') {
        ("assignment_statement", stripped.to_string())
    } else {
        ("statement", stripped.to_string())
    };

    leaf_span(id, node_type, &label, line, end_col)
}

fn push_statement(stack: &mut [Frame], trimmed: &str, upper: &str, line: u32, end_col: u32) {
    if let Some(parent) = stack.last_mut() {
        let child_idx = parent.children.len();
        let id = format!("{}.{}", parent.id, child_idx);
        parent
            .children
            .push(statement_node(&id, trimmed, upper, line, end_col));
    }
}

// ---------------------------------------------------------------------------
// Language detection
// ---------------------------------------------------------------------------

pub(crate) fn detect_language_impl(filename: &str, _content: &str) -> String {
    if filename.to_lowercase().ends_with(".abap") {
        "abap".to_string()
    } else {
        String::new()
    }
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

const TS_ENTITY_TYPES: &[&str] = &[
    "class_declaration",
    "class_implementation",
    "function_implementation",
    "interface_declaration",
    "method_implementation",
    "method_declaration",
    "class_method_declaration",
];

const TS_STATEMENT_TYPES: &[&str] = &[
    "assignment",
    "call_function",
    "call_method",
    "call_method_instance",
    "call_method_static",
    "chained_variable_declaration",
    "chained_write_statement",
    "variable_declaration",
    "write_statement",
];

const TS_LEAF_TYPES: &[&str] = &[
    "character_literal",
    "field_symbol_name",
    "name",
    "numeric_literal",
    "string",
    "structured_data_object",
];

fn ts_text<'a>(node: tree_sitter::Node<'_>, source: &'a [u8]) -> &'a str {
    node.utf8_text(source).unwrap_or("")
}

fn ts_trimmed(node: tree_sitter::Node<'_>, source: &[u8]) -> String {
    ts_text(node, source)
        .trim()
        .trim_end_matches('.')
        .trim()
        .to_string()
}

fn ts_name(node: tree_sitter::Node<'_>, source: &[u8]) -> String {
    if let Some(name) = node.child_by_field_name("name") {
        let label = ts_trimmed(name, source);
        if !label.is_empty() {
            return label.to_uppercase();
        }
    }
    for i in 0..node.named_child_count() {
        let Some(child) = node.named_child(i) else {
            continue;
        };
        if matches!(
            child.kind(),
            "name" | "structured_data_object" | "field_symbol_name"
        ) {
            let label = ts_trimmed(child, source);
            if !label.is_empty() {
                return label.to_uppercase();
            }
        }
    }
    let label = ts_trimmed(node, source);
    if label.is_empty() {
        node.kind().to_string()
    } else {
        label.to_uppercase()
    }
}

fn ts_signature_child(
    id_prefix: &str,
    node: tree_sitter::Node<'_>,
    source: &[u8],
) -> Option<SemanticNode> {
    let header = ts_text(node, source)
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .trim_end_matches('.')
        .trim()
        .to_uppercase();
    if header.is_empty() {
        return None;
    }
    Some(leaf_span(
        &format!("{}.0", id_prefix),
        "signature",
        &header,
        node.start_position().row as u32,
        node.end_position().column as u32,
    ))
}

fn ts_semantic_type(kind: &str) -> Option<&'static str> {
    Some(match kind {
        "program" => "program",
        "report_statement" => "report",
        "class_declaration" => "class_definition",
        "class_implementation" => "class_impl",
        "function_implementation" => "function_module",
        "interface_declaration" => "interface",
        "method_implementation" | "method_declaration" | "class_method_declaration" => "method",
        "method_body" => "statement_list",
        "assignment" => "assignment_statement",
        "chained_variable_declaration" | "variable_declaration" => "data_declaration",
        "chained_write_statement" | "write_statement" => "write_statement",
        "call_function" => "call_expression",
        "call_method" | "call_method_instance" | "call_method_static" => "call_expression",
        "character_literal" | "string" => "string_literal",
        "numeric_literal" => "numeric_literal",
        "name" | "structured_data_object" | "field_symbol_name" => "identifier",
        _ => return None,
    })
}

fn ts_label(node: tree_sitter::Node<'_>, source: &[u8], semantic_type: &str) -> String {
    match semantic_type {
        "program" => "program".to_string(),
        "report" | "class_definition" | "class_impl" | "function_module" | "interface"
        | "method" => ts_name(node, source),
        "identifier" => ts_trimmed(node, source).to_uppercase(),
        "statement_list" => "statement_list".to_string(),
        "assignment_statement" => ts_trimmed(node, source),
        "call_expression" => ts_name(node, source),
        "data_declaration" => ts_name(node, source),
        "write_statement" => {
            let label = ts_trimmed(node, source);
            label
                .strip_prefix("WRITE:")
                .or_else(|| label.strip_prefix("write:"))
                .unwrap_or(&label)
                .trim()
                .to_string()
        }
        _ => ts_trimmed(node, source),
    }
}

fn ts_convert(node: tree_sitter::Node<'_>, source: &[u8], id_prefix: &str) -> Option<SemanticNode> {
    if node.is_error() || node.is_missing() {
        return None;
    }
    let kind = node.kind();
    let semantic_type = ts_semantic_type(kind);

    let mut children: Vec<SemanticNode> = Vec::new();
    if TS_ENTITY_TYPES.contains(&kind) {
        if let Some(signature) = ts_signature_child(id_prefix, node, source) {
            children.push(signature);
        }
    }
    for i in 0..node.named_child_count() {
        let Some(child) = node.named_child(i) else {
            continue;
        };
        if child.kind() == "name" && TS_ENTITY_TYPES.contains(&kind) {
            continue;
        }
        if let Some(converted) = ts_convert(child, source, &format!("{}.{}", id_prefix, i)) {
            children.push(converted);
        }
    }

    let Some(semantic_type) = semantic_type else {
        if children.len() == 1 {
            return children.into_iter().next();
        }
        if children.is_empty() {
            return None;
        }
        return Some(
            SemanticNodeBuilder::new(
                id_prefix,
                kind,
                kind,
                node.start_position().row as u32,
                node.start_position().column as u32,
                node.end_position().row as u32,
                node.end_position().column as u32,
                String::new(),
            )
            .children(children)
            .build(),
        );
    };

    if children.is_empty()
        && !TS_ENTITY_TYPES.contains(&kind)
        && !TS_STATEMENT_TYPES.contains(&kind)
        && !TS_LEAF_TYPES.contains(&kind)
        && kind != "report_statement"
        && kind != "program"
    {
        return None;
    }

    Some(
        SemanticNodeBuilder::new(
            id_prefix,
            semantic_type,
            ts_label(node, source, semantic_type),
            node.start_position().row as u32,
            node.start_position().column as u32,
            node.end_position().row as u32,
            node.end_position().column as u32,
            String::new(),
        )
        .children(children)
        .build(),
    )
}

fn ts_counts(node: tree_sitter::Node<'_>) -> (usize, usize) {
    let mut total = 1;
    let mut errors = usize::from(node.is_error() || node.is_missing());
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i) {
            let (child_total, child_errors) = ts_counts(child);
            total += child_total;
            errors += child_errors;
        }
    }
    (total, errors)
}

fn ts_has_top_level_construct(root: tree_sitter::Node<'_>) -> bool {
    (0..root.named_child_count()).any(|i| {
        root.named_child(i)
            .map(|child| {
                matches!(child.kind(), "report_statement")
                    || TS_ENTITY_TYPES.contains(&child.kind())
                    || TS_STATEMENT_TYPES.contains(&child.kind())
            })
            .unwrap_or(false)
    })
}

fn source_has_known_unsupported_blocks(source: &str) -> bool {
    source.lines().any(|line| {
        let upper = line.trim_start().to_uppercase();
        upper.starts_with("FORM ")
            || upper.starts_with("ENDFORM")
            || upper.starts_with("MODULE ")
            || upper.starts_with("ENDMODULE")
    })
}

fn parse_abap_tree_sitter(source: &str) -> Option<SemanticNode> {
    if source_has_known_unsupported_blocks(source) {
        return None;
    }

    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_abap_sqry::LANGUAGE.into())
        .ok()?;
    let tree = parser.parse(source, None)?;
    let root = tree.root_node();
    let (total_nodes, error_nodes) = ts_counts(root);
    if !ts_has_top_level_construct(root) {
        return None;
    }
    if error_nodes * 3 >= total_nodes {
        return None;
    }
    if error_nodes > 0 && source_has_known_unsupported_blocks(source) {
        return None;
    }
    let mut semantic_root = ts_convert(root, source.as_bytes(), "0")?;
    if semantic_root.node_type != "program" {
        semantic_root = SemanticNodeBuilder::new(
            "0",
            "program",
            "program",
            0,
            0,
            source.lines().count().saturating_sub(1) as u32,
            0,
            String::new(),
        )
        .children(vec![semantic_root])
        .build();
    }
    if semantic_root.children.is_empty() {
        None
    } else {
        Some(semantic_root)
    }
}

pub(crate) fn parse_abap(source: &str) -> String {
    if let Some(root) = parse_abap_tree_sitter(source) {
        return serde_json::to_string(&root)
            .unwrap_or_else(|e| format!(r#"{{"error":"Serialisation error: {}"}}"#, e));
    }
    parse_abap_fallback(source)
}

pub(crate) fn parse_abap_fallback(source: &str) -> String {
    let mut root_children: Vec<SemanticNode> = Vec::new();
    let mut stack: Vec<Frame> = Vec::new();
    let mut counter: usize = 0;
    let total_lines = source.lines().count().saturating_sub(1) as u32;

    for (idx, raw_line) in source.lines().enumerate() {
        let lineno = idx as u32;
        let trimmed = raw_line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // ABAP comment: * at start of line or " anywhere
        let first_ch = trimmed.chars().next().unwrap();
        if first_ch == '*' || first_ch == '"' {
            continue;
        }

        let upper = trimmed.to_uppercase();
        let words: Vec<&str> = upper.split_whitespace().collect();
        if words.is_empty() {
            continue;
        }
        let end_col = raw_line.len() as u32;
        // ABAP statements end with a period; strip statement punctuation before
        // keyword matching so lines such as `WRITE:` and accidental suffixes on
        // terminators do not hide the statement kind.
        let kw0 = keyword(words[0]);

        match kw0 {
            "REPORT" | "PROGRAM" if stack.is_empty() => {
                if words.len() >= 2 {
                    let id = format!("0.{}", counter);
                    counter += 1;
                    root_children.push(leaf(&id, "report", &extract_name(&words, 1), lineno));
                }
            }
            "CLASS" if words.len() >= 3 => {
                let name = extract_name(&words, 1);
                let kw = words.get(2).copied().unwrap_or("");
                let node_type: &'static str = if kw.starts_with("IMPL") {
                    "class_impl"
                } else {
                    "class_definition"
                };
                let id = format!("0.{}", counter);
                counter += 1;
                stack.push(Frame {
                    id,
                    node_type,
                    label: name,
                    start_line: lineno,
                    children: vec![],
                });
            }
            "INTERFACE" if stack.is_empty() && words.len() >= 2 => {
                let name = extract_name(&words, 1);
                let id = format!("0.{}", counter);
                counter += 1;
                let children = signature_children(&id, &words, 2, lineno, end_col);
                stack.push(Frame {
                    id,
                    node_type: "interface",
                    label: name,
                    start_line: lineno,
                    children,
                });
            }
            "METHOD" if !stack.is_empty() && words.len() >= 2 => {
                let name = extract_name(&words, 1);
                let parent_id = stack
                    .last()
                    .map(|f| f.id.as_str())
                    .unwrap_or("0")
                    .to_string();
                let child_idx = stack.last().map(|f| f.children.len()).unwrap_or(0);
                let id = format!("{}.{}", parent_id, child_idx);
                let children = signature_children(&id, &words, 2, lineno, end_col);
                stack.push(Frame {
                    id,
                    node_type: "method",
                    label: name,
                    start_line: lineno,
                    children,
                });
            }
            "FORM" if stack.is_empty() && words.len() >= 2 => {
                let name = extract_name(&words, 1);
                let id = format!("0.{}", counter);
                counter += 1;
                let children = signature_children(&id, &words, 2, lineno, end_col);
                stack.push(Frame {
                    id,
                    node_type: "form",
                    label: name,
                    start_line: lineno,
                    children,
                });
            }
            "FUNCTION"
                if stack.is_empty()
                    && words.len() >= 2
                    && words.get(1).copied() != Some("POOL") =>
            {
                let name = extract_name(&words, 1);
                let id = format!("0.{}", counter);
                counter += 1;
                let children = signature_children(&id, &words, 2, lineno, end_col);
                stack.push(Frame {
                    id,
                    node_type: "function_module",
                    label: name,
                    start_line: lineno,
                    children,
                });
            }
            "MODULE" if stack.is_empty() && words.len() >= 2 => {
                let name = extract_name(&words, 1);
                let id = format!("0.{}", counter);
                counter += 1;
                let children = signature_children(&id, &words, 2, lineno, end_col);
                stack.push(Frame {
                    id,
                    node_type: "module",
                    label: name,
                    start_line: lineno,
                    children,
                });
            }
            "ENDCLASS" | "ENDINTERFACE" => {
                if let Some(frame) = stack.pop() {
                    let n = block_node(
                        &frame.id,
                        frame.node_type,
                        &frame.label,
                        frame.start_line,
                        lineno,
                        frame.children,
                    );
                    root_children.push(n);
                }
            }
            "ENDMETHOD" => {
                if let Some(frame) = stack.pop() {
                    let n = block_node(
                        &frame.id,
                        frame.node_type,
                        &frame.label,
                        frame.start_line,
                        lineno,
                        frame.children,
                    );
                    push_to(&mut stack, &mut root_children, n);
                }
            }
            "ENDFORM" | "ENDFUNCTION" | "ENDMODULE" => {
                if let Some(frame) = stack.pop() {
                    let n = block_node(
                        &frame.id,
                        frame.node_type,
                        &frame.label,
                        frame.start_line,
                        lineno,
                        frame.children,
                    );
                    root_children.push(n);
                }
            }
            _ if !stack.is_empty() => {
                push_statement(&mut stack, trimmed, &upper, lineno, end_col);
            }
            _ => {}
        }
    }

    // Drain unclosed frames (malformed source)
    while let Some(frame) = stack.pop() {
        let n = block_node(
            &frame.id,
            frame.node_type,
            &frame.label,
            frame.start_line,
            total_lines,
            frame.children,
        );
        root_children.push(n);
    }

    let root = SemanticNodeBuilder::new(
        "0",
        "program",
        "program",
        0,
        0,
        total_lines,
        0,
        String::new(),
    )
    .children(root_children)
    .build();

    match serde_json::to_string(&root) {
        Ok(s) => s,
        Err(e) => format!(r#"{{"error":"Serialisation error: {}"}}"#, e),
    }
}

// ---------------------------------------------------------------------------
// WIT guest impl
// ---------------------------------------------------------------------------

impl Guest for AbapParser {
    fn get_parser_mode() -> ParserMode {
        ParserMode::FullParse
    }
    fn grammar_id() -> String {
        "abap".to_string()
    }
    fn detect_language(filename: String, content: String) -> String {
        detect_language_impl(&filename, &content)
    }
    fn preprocess_source(source: String) -> String {
        source
    }
    fn example(_language: String) -> ExamplePair {
        ExamplePair {
            old: "REPORT z_demo.\n\nFORM greet.\n  WRITE: 'Hello, World'.\nENDFORM.\n".to_string(),
            new: "REPORT z_demo.\n\nFORM greet USING lv_name TYPE string.\n  DATA(lv_msg) = |Hello, { lv_name }!|.\n  WRITE: lv_msg.\nENDFORM.\n\nFORM add_numbers USING a TYPE i b TYPE i CHANGING result TYPE i.\n  result = a + b.\nENDFORM.\n".to_string(),
        }
    }
    fn process(input: String, _language: String, _filename: String) -> String {
        parse_abap(&input)
    }
    fn trivia_node_types() -> Vec<String> {
        vec![]
    }
    fn language_ids() -> Vec<String> {
        vec!["abap".to_string()]
    }
    fn language_info() -> Vec<LanguageInfoRecord> {
        language_info_for(Self::language_ids())
    }
    fn priority() -> i32 {
        0
    }
}

export!(AbapParser);

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    intentumdiff_plugin_sdk::plugin_compliance_tests! {
        process: parse_abap,
        detect_fn: detect_language_impl,
        detect_cases: [
            ("my_program.abap", "", "abap"),
            ("MY_PROGRAM.ABAP", "", "abap"),
            ("script.sql",      "", ""),
            ("script.txt",      "", ""),
        ],
        grammar_id: "abap",
        language_ids: ["abap"],
    }

    const SAMPLE: &str = "REPORT zmyreport.\n\
        \nCLASS zcl_manager DEFINITION.\n\
        PUBLIC SECTION.\n  METHODS get_data.\n\
        ENDCLASS.\n\
        \nCLASS zcl_manager IMPLEMENTATION.\n\
        METHOD get_data.\n    WRITE: 'Hello'.\n  ENDMETHOD.\n\
        ENDCLASS.\n\
        \nFORM my_subroutine.\n  WRITE: 'sub'.\nENDFORM.\n";

    fn tree_sitter_json(src: &str) -> String {
        serde_json::to_string(&parse_abap_tree_sitter(src).expect("tree-sitter parse"))
            .expect("tree-sitter json")
    }

    #[test]
    fn test_valid_json_no_error() {
        let out = parse_abap(SAMPLE);
        intentumdiff_plugin_sdk::testing::assert_valid_json(&out, "SAMPLE");
        intentumdiff_plugin_sdk::testing::assert_no_error(&out, "SAMPLE");
    }

    #[test]
    fn test_root_is_program() {
        let out = parse_abap(SAMPLE);
        intentumdiff_plugin_sdk::testing::assert_root_node_type(&out, "program", "SAMPLE");
    }

    #[test]
    fn test_report_leaf() {
        let out = parse_abap("REPORT ztest.");
        intentumdiff_plugin_sdk::testing::assert_contains_node_type(&out, "report", "report");
    }

    #[test]
    fn test_class_definition() {
        let out = parse_abap(SAMPLE);
        intentumdiff_plugin_sdk::testing::assert_contains_node_type(
            &out,
            "class_definition",
            "class_definition",
        );
    }

    #[test]
    fn test_class_impl_with_method() {
        let out = parse_abap(SAMPLE);
        intentumdiff_plugin_sdk::testing::assert_contains_node_type(&out, "class_impl", "class_impl");
        intentumdiff_plugin_sdk::testing::assert_contains_node_type(&out, "method", "method");
    }

    #[test]
    fn test_tree_sitter_class_interface_and_method_blocks() {
        let src = "REPORT ztest.\n\
            \nINTERFACE zif_demo.\n  METHODS get_data.\nENDINTERFACE.\n\
            \nCLASS zcl_manager DEFINITION.\n  PUBLIC SECTION.\n  METHODS get_data.\nENDCLASS.\n\
            \nCLASS zcl_manager IMPLEMENTATION.\n  METHOD get_data.\n    WRITE: 'Hello'.\n  ENDMETHOD.\nENDCLASS.\n";
        let out = tree_sitter_json(src);

        intentumdiff_plugin_sdk::testing::assert_contains_node_type(&out, "report", "report");
        intentumdiff_plugin_sdk::testing::assert_contains_node_type(&out, "interface", "interface");
        intentumdiff_plugin_sdk::testing::assert_contains_node_type(
            &out,
            "class_definition",
            "class_definition",
        );
        intentumdiff_plugin_sdk::testing::assert_contains_node_type(&out, "class_impl", "class_impl");
        intentumdiff_plugin_sdk::testing::assert_contains_node_type(&out, "method", "method");
        intentumdiff_plugin_sdk::testing::assert_contains_node_type(
            &out,
            "write_statement",
            "write_statement",
        );
    }

    #[test]
    fn test_tree_sitter_function_module() {
        let src = "FUNCTION z_my_func.\n  WRITE: 'x'.\nENDFUNCTION.\n";
        let out = tree_sitter_json(src);

        intentumdiff_plugin_sdk::testing::assert_contains_node_type(
            &out,
            "function_module",
            "function_module",
        );
        intentumdiff_plugin_sdk::testing::assert_contains_node_type(
            &out,
            "write_statement",
            "write_statement",
        );
    }

    #[test]
    fn test_form_block_routes_to_fallback_scanner() {
        let src = "REPORT ztest.\n\nFORM greet.\n  WRITE: 'Hello'.\nENDFORM.\n";
        assert!(parse_abap_tree_sitter(src).is_none());
        let out = parse_abap(src);
        intentumdiff_plugin_sdk::testing::assert_contains_node_type(&out, "form", "form");
    }

    #[test]
    fn test_form_block() {
        let out = parse_abap(SAMPLE);
        intentumdiff_plugin_sdk::testing::assert_contains_node_type(&out, "form", "form");
    }

    #[test]
    fn test_form_signature_and_body_affect_hash() {
        let old = "REPORT z_demo.\n\
            \nFORM greet.\n  WRITE: 'Hello, World'.\nENDFORM.\n";
        let new = "REPORT z_demo.\n\
            \nFORM greet USING lv_name TYPE string.\n  DATA(lv_msg) = |Hello, { lv_name }!|.\n  WRITE: lv_msg.\nENDFORM.\n";

        let old_tree: SemanticNode = serde_json::from_str(&parse_abap(old)).unwrap();
        let new_tree: SemanticNode = serde_json::from_str(&parse_abap(new)).unwrap();
        let old_form = old_tree
            .children
            .iter()
            .find(|node| node.node_type == "form" && node.label == "GREET")
            .unwrap();
        let new_form = new_tree
            .children
            .iter()
            .find(|node| node.node_type == "form" && node.label == "GREET")
            .unwrap();

        assert_ne!(old_form.structural_hash, new_form.structural_hash);
        assert!(old_form
            .children
            .iter()
            .any(|node| node.node_type == "write_statement"));
        assert!(new_form
            .children
            .iter()
            .any(|node| node.node_type == "signature"));
        assert!(new_form
            .children
            .iter()
            .any(|node| node.node_type == "signature" && node.position.end_col > 0));
        assert!(new_form
            .children
            .iter()
            .any(|node| node.node_type == "data_declaration"));
    }

    #[test]
    fn test_comment_lines_ignored() {
        let src = "* This is a comment\n\" This too\nREPORT ztest.";
        let out = parse_abap(src);
        intentumdiff_plugin_sdk::testing::assert_contains_node_type(&out, "report", "with comments");
    }

    #[test]
    fn test_function_module() {
        let src = "FUNCTION z_my_func.\n  \" code\nENDFUNCTION.";
        let out = parse_abap(src);
        intentumdiff_plugin_sdk::testing::assert_contains_node_type(
            &out,
            "function_module",
            "function_module",
        );
    }

    #[test]
    fn test_labels_nonempty() {
        let out = parse_abap(SAMPLE);
        intentumdiff_plugin_sdk::testing::assert_labels_nonempty(&out, "class_definition", "labels");
        intentumdiff_plugin_sdk::testing::assert_labels_nonempty(&out, "method", "labels");
    }
}
