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

// ---------------------------------------------------------------------------
// Literal-aware ABAP token stream
// ---------------------------------------------------------------------------
//
// The tree-sitter grammar does not know backtick string literals, string
// templates or `DATA ... VALUE` initializers; it splits them into identifiers
// and ERROR nodes. The scanner below is the parser's source of truth for
// literal payloads and for the token text of regions the grammar does not
// model, so no executable token is silently dropped from the semantic tree.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TokKind {
    Word,
    Literal,
    Punct,
}

#[derive(Clone, Copy, Debug)]
struct Tok {
    start: usize,
    end: usize,
    kind: TokKind,
}

/// Byte offset just past a literal that opens at `start` (`'…'`, `` `…` `` or
/// `|…|`). Doubled delimiters escape quotes; templates honour `\` escapes and
/// `{ … }` embedded expressions. ABAP literals never span lines, so an
/// unterminated literal stops at the end of its line.
fn literal_end(bytes: &[u8], start: usize) -> usize {
    let quote = bytes[start];
    let mut i = start + 1;
    let mut depth = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\n' {
            return i;
        }
        if quote == b'|' {
            match b {
                b'\\' => {
                    i += 2;
                    continue;
                }
                b'{' => depth += 1,
                b'}' => depth = depth.saturating_sub(1),
                b'|' if depth == 0 => return i + 1,
                _ => {}
            }
        } else if b == quote {
            if bytes.get(i + 1) == Some(&quote) {
                i += 2;
                continue;
            }
            return i + 1;
        }
        i += 1;
    }
    bytes.len()
}

/// Tokenise `source[start..end]`, skipping whitespace and comments (`"` to end
/// of line; `*` in the first column). Offsets are absolute into `source`.
fn abap_tokens(source: &str, start: usize, end: usize) -> Vec<Tok> {
    let bytes = source.as_bytes();
    let end = end.min(bytes.len());
    let mut toks = Vec::new();
    let mut i = start;
    while i < end {
        let b = bytes[i];
        if b.is_ascii_whitespace() {
            i += 1;
        } else if b == b'"' || (b == b'*' && (i == 0 || bytes[i - 1] == b'\n')) {
            while i < end && bytes[i] != b'\n' {
                i += 1;
            }
        } else if matches!(b, b'\'' | b'`' | b'|') {
            let e = literal_end(bytes, i).min(end);
            toks.push(Tok { start: i, end: e, kind: TokKind::Literal });
            i = e;
        } else if matches!(b, b',' | b'.' | b':') {
            toks.push(Tok { start: i, end: i + 1, kind: TokKind::Punct });
            i += 1;
        } else {
            let s = i;
            while i < end {
                let c = bytes[i];
                if c.is_ascii_whitespace() || matches!(c, b'\'' | b'`' | b'|' | b'"' | b',' | b'.' | b':') {
                    break;
                }
                i += 1;
            }
            toks.push(Tok { start: s, end: i, kind: TokKind::Word });
        }
    }
    toks
}

/// Canonical text of a token: words are case-insensitive in ABAP and are
/// upper-cased; literal payloads are kept byte-for-byte.
fn tok_text(source: &str, tok: &Tok) -> String {
    let text = &source[tok.start..tok.end];
    match tok.kind {
        TokKind::Literal => text.to_string(),
        _ => text.to_uppercase(),
    }
}

/// Layout- and comment-insensitive canonical text of a token run.
fn canonical_tokens(source: &str, toks: &[Tok]) -> String {
    let mut out = String::new();
    for tok in toks {
        let text = tok_text(source, tok);
        if tok.kind == TokKind::Punct && (text == "," || text == ":") {
            out.push_str(&text);
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&text);
    }
    out
}

fn canonical_text(source: &str, start: usize, end: usize) -> String {
    let mut toks = abap_tokens(source, start, end);
    if toks.last().is_some_and(|t| t.kind == TokKind::Punct && &source[t.start..t.end] == ".") {
        toks.pop();
    }
    canonical_tokens(source, &toks)
}

/// Byte offset → (0-based row, UTF-8 byte column).
struct LineIndex {
    starts: Vec<usize>,
}

impl LineIndex {
    fn new(source: &str) -> Self {
        let mut starts = vec![0];
        starts.extend(source.match_indices('\n').map(|(i, _)| i + 1));
        LineIndex { starts }
    }

    fn pos(&self, byte: usize) -> (u32, u32) {
        let row = self.starts.partition_point(|&s| s <= byte).saturating_sub(1);
        (row as u32, (byte - self.starts[row]) as u32)
    }
}

/// Shared projection context: the source, its literal spans and line index.
struct Ctx<'a> {
    source: &'a str,
    literals: Vec<(usize, usize)>,
    lines: LineIndex,
}

impl<'a> Ctx<'a> {
    fn new(source: &'a str) -> Self {
        let literals = abap_tokens(source, 0, source.len())
            .into_iter()
            .filter(|t| t.kind == TokKind::Literal)
            .map(|t| (t.start, t.end))
            .collect();
        Ctx { source, literals, lines: LineIndex::new(source) }
    }

    fn node(&self, id: &str, node_type: &str, label: &str, start: usize, end: usize) -> SemanticNodeBuilder {
        let (sr, sc) = self.lines.pos(start);
        let (er, ec) = self.lines.pos(end);
        SemanticNodeBuilder::new(id, node_type, label, sr, sc, er, ec, String::new())
    }

    fn leaf_toks(&self, id: &str, node_type: &str, toks: &[Tok]) -> Option<SemanticNode> {
        let first = toks.first()?;
        let last = toks.last()?;
        let label = canonical_tokens(self.source, toks);
        Some(self.node(id, node_type, &label, first.start, last.end).build())
    }
}

fn is_word(source: &str, tok: &Tok, word: &str) -> bool {
    tok.kind == TokKind::Word && source[tok.start..tok.end].eq_ignore_ascii_case(word)
}

fn is_punct(source: &str, tok: &Tok, p: &str) -> bool {
    tok.kind == TokKind::Punct && &source[tok.start..tok.end] == p
}

/// Paren-depth delta of a word token (`#(` opens, `)` closes).
fn paren_delta(source: &str, tok: &Tok) -> i32 {
    if tok.kind != TokKind::Word {
        return 0;
    }
    let text = &source[tok.start..tok.end];
    text.matches('(').count() as i32 - text.matches(')').count() as i32
}

/// One declared data object: `name TYPE … VALUE …` or inline `DATA(name) = …`.
/// `rest` holds the tokens after the name; the initializer starts at the first
/// depth-0 `init_kw` (`VALUE`, or `=` for inline declarations).
fn data_entry_node(
    ctx: &Ctx<'_>,
    id: &str,
    name: (&str, usize, usize),
    rest: &[Tok],
    init_kw: &str,
    span: (usize, usize),
) -> SemanticNode {
    let src = ctx.source;
    let (label, name_start, name_end) = name;
    let mut children = vec![ctx
        .node(&format!("{id}.0"), "identifier", label, name_start, name_end)
        .build()];
    let mut depth = 0;
    let mut split = None;
    for (i, tok) in rest.iter().enumerate() {
        if depth == 0 && is_word(src, tok, init_kw) {
            split = Some(i);
            break;
        }
        depth += paren_delta(src, tok);
    }
    let (typing, init) = match split {
        // A bare `VALUE` with nothing after it still keeps the keyword.
        Some(i) if i + 1 == rest.len() => (&rest[..i], &rest[i..]),
        Some(i) => (&rest[..i], &rest[i + 1..]),
        None => (rest, &rest[rest.len()..]),
    };
    for (kind, part) in [("type_annotation", typing), ("initializer", init)] {
        let n = children.len();
        if let Some(child) = ctx.leaf_toks(&format!("{id}.{n}"), kind, part) {
            children.push(child);
        }
    }
    ctx.node(id, "data_declaration", label, span.0, span.1)
        .children(children)
        .build()
}

/// Project a `DATA` statement (plain, chained `DATA:` or inline `DATA(x) = …`)
/// from its token stream. The grammar mis-parses initializers and literals,
/// so the declaration's name, typing and initializer all come from tokens.
fn data_statement_node(ctx: &Ctx<'_>, id: &str, start: usize, end: usize) -> Option<SemanticNode> {
    let src = ctx.source;
    let mut toks = abap_tokens(src, start, end);
    if toks.last().is_some_and(|t| is_punct(src, t, ".")) {
        toks.pop();
    }
    let head = *toks.first()?;
    let last_end = toks.last()?.end;
    let head_text = &src[head.start..head.end];
    let upper_head = head_text.to_uppercase();

    // Inline declaration: `DATA(name) = expr`.
    if let Some(inner) = upper_head.strip_prefix("DATA(") {
        let close = inner.find(')')?;
        let name = inner[..close].trim();
        if name.is_empty() {
            return None;
        }
        let name_start = head.start + 5;
        let name_end = name_start + close;
        return Some(data_entry_node(
            ctx,
            id,
            (name, name_start, name_end),
            &toks[1..],
            "=",
            (head.start, last_end),
        ));
    }
    if upper_head != "DATA" {
        return None;
    }

    let body = &toks[1..];
    let chained = body.first().is_some_and(|t| is_punct(src, t, ":"));
    if !chained {
        let name_tok = body.first()?;
        let name = tok_text(src, name_tok);
        return Some(data_entry_node(
            ctx,
            id,
            (&name, name_tok.start, name_tok.end),
            &body[1..],
            "VALUE",
            (head.start, last_end),
        ));
    }

    // Chained `DATA: a TYPE i VALUE 1, b TYPE string.` — one entry per
    // depth-0 comma-separated segment.
    let mut entries = Vec::new();
    let mut seg_start = 1;
    let mut depth = 0;
    for i in 1..=body.len() {
        if i < body.len() {
            let tok = &body[i];
            if !(depth == 0 && is_punct(src, tok, ",")) {
                depth += paren_delta(src, tok);
                continue;
            }
        }
        let seg = &body[seg_start..i];
        if let (Some(name_tok), Some(seg_last)) = (seg.first(), seg.last()) {
            let n = entries.len();
            let name = tok_text(src, name_tok);
            entries.push(data_entry_node(
                ctx,
                &format!("{id}.{n}"),
                (&name, name_tok.start, name_tok.end),
                &seg[1..],
                "VALUE",
                (name_tok.start, seg_last.end),
            ));
        }
        seg_start = i + 1;
    }
    Some(
        ctx.node(id, "data_declaration_chain", "DATA", head.start, last_end)
            .children(entries)
            .build(),
    )
}

/// Conservative node for source the grammar did not model (ERROR regions,
/// literals it split apart). Splits on depth-0 `.` so each statement keeps its
/// own node; string literals become `string_literal` with their full payload.
fn unmodeled_nodes(ctx: &Ctx<'_>, id: &str, start: usize, end: usize) -> Vec<SemanticNode> {
    let src = ctx.source;
    if ctx.literals.contains(&(start, end)) {
        let text = &src[start..end];
        return vec![ctx.node(id, "string_literal", text, start, end).build()];
    }
    let toks = abap_tokens(src, start, end);
    let mut out = Vec::new();
    for stmt in toks.split(|t| is_punct(src, t, ".")) {
        let (Some(first), Some(last)) = (stmt.first(), stmt.last()) else {
            continue;
        };
        let sid = if out.is_empty() { id.to_string() } else { format!("{id}~{}", out.len()) };
        if let Some(node) = data_statement_node(ctx, &sid, first.start, last.end) {
            out.push(node);
            continue;
        }
        if stmt.len() == 1 && first.kind == TokKind::Literal {
            out.push(ctx.node(&sid, "string_literal", &src[first.start..first.end], first.start, first.end).build());
            continue;
        }
        if is_word(src, first, "WRITE") {
            let children = stmt
                .iter()
                .filter(|t| t.kind == TokKind::Literal)
                .enumerate()
                .map(|(n, t)| {
                    ctx.node(&format!("{sid}.{n}"), "string_literal", &src[t.start..t.end], t.start, t.end)
                        .build()
                })
                .collect::<Vec<_>>();
            let label = canonical_tokens(src, stmt);
            out.push(ctx.node(&sid, "write_statement", &label, first.start, last.end).children(children).build());
            continue;
        }
        if is_word(src, first, "VALUE") && stmt.len() > 1 {
            if let Some(node) = ctx.leaf_toks(&sid, "initializer", &stmt[1..]) {
                out.push(node);
                continue;
            }
        }
        if let Some(node) = ctx.leaf_toks(&sid, "unparsed", stmt) {
            out.push(node);
        }
    }
    out
}

fn statement_node(
    ctx: &Ctx<'_>,
    id: &str,
    trimmed: &str,
    upper: &str,
    span: (usize, usize),
    line: u32,
    end_col: u32,
) -> SemanticNode {
    let stripped = trimmed.trim_end_matches('.').trim();
    let upper_stripped = upper.trim_end_matches('.').trim();
    let first_word = upper_stripped.split_whitespace().next().unwrap_or("");
    let first_kw = keyword(first_word);

    if first_kw.starts_with("DATA") {
        // Name, typing and initializer all carry meaning (`DATA(x) = 1` vs
        // `= 2`, `TYPE i VALUE 1` vs `VALUE 2`); never reduce a declaration
        // to its name alone.
        if let Some(node) = data_statement_node(ctx, id, span.0, span.1) {
            return node;
        }
        let label = data_label(trimmed)
            .unwrap_or_else(|| canonical_text(ctx.source, span.0, span.1));
        return leaf_span(id, "data_declaration", &label, line, end_col);
    }

    let (node_type, label) = if first_kw == "WRITE" {
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

fn push_statement(
    ctx: &Ctx<'_>,
    stack: &mut [Frame],
    trimmed: &str,
    upper: &str,
    span: (usize, usize),
    line: u32,
    end_col: u32,
) {
    if let Some(parent) = stack.last_mut() {
        let child_idx = parent.children.len();
        let id = format!("{}.{}", parent.id, child_idx);
        parent
            .children
            .push(statement_node(ctx, &id, trimmed, upper, span, line, end_col));
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
    ctx: &Ctx<'_>,
) -> Option<SemanticNode> {
    let start = node.start_byte();
    let line_end = ctx.source[start..node.end_byte()]
        .find('\n')
        .map_or(node.end_byte(), |i| start + i);
    // Literal-preserving: only words are case-folded.
    let header = canonical_text(ctx.source, start, line_end);
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

/// Is `child` source the grammar failed to model? ERROR nodes are, and so is
/// any node that cuts through a string literal the grammar does not lex
/// (backtick strings and templates come back as identifiers + ERRORs).
/// Returns the byte range to project conservatively, widened to whole literals.
fn ts_unmodeled_range(ctx: &Ctx<'_>, child: tree_sitter::Node<'_>) -> Option<(usize, usize)> {
    let (mut s, mut e) = (child.start_byte(), child.end_byte());
    let mut broken = child.is_error();
    for &(ls, le) in &ctx.literals {
        let intersects = ls < e && s < le;
        if intersects && !(s <= ls && le <= e) {
            broken = true;
        }
    }
    if !broken {
        return None;
    }
    // Widen to every literal the range touches (repeat: widening can reach more).
    loop {
        let (ps, pe) = (s, e);
        for &(ls, le) in &ctx.literals {
            if ls < e && s < le {
                s = s.min(ls);
                e = e.max(le);
            }
        }
        if (ps, pe) == (s, e) {
            return Some((s, e));
        }
    }
}

fn ts_convert(node: tree_sitter::Node<'_>, ctx: &Ctx<'_>, id_prefix: &str) -> Option<SemanticNode> {
    let source = ctx.source.as_bytes();
    if node.is_error() || node.is_missing() {
        return None;
    }
    let kind = node.kind();
    if matches!(kind, "variable_declaration" | "chained_variable_declaration") {
        if let Some(decl) = data_statement_node(ctx, id_prefix, node.start_byte(), node.end_byte()) {
            return Some(decl);
        }
    }
    if kind == "typing" {
        let label = canonical_text(ctx.source, node.start_byte(), node.end_byte());
        return Some(ctx.node(id_prefix, "type_annotation", &label, node.start_byte(), node.end_byte()).build());
    }
    let semantic_type = ts_semantic_type(kind);

    let mut children: Vec<SemanticNode> = Vec::new();
    if TS_ENTITY_TYPES.contains(&kind) {
        if let Some(signature) = ts_signature_child(id_prefix, node, ctx) {
            children.push(signature);
        }
    }
    // Pending unmodeled region: (start, end, child index for the id).
    let mut pending: Option<(usize, usize, usize)> = None;
    for i in 0..node.named_child_count() {
        let Some(child) = node.named_child(i) else {
            continue;
        };
        if child.is_missing() {
            continue;
        }
        if let Some((s, e)) = ts_unmodeled_range(ctx, child) {
            pending = match pending {
                Some((ps, pe, pi)) if s < pe => Some((ps, pe.max(e), pi)),
                Some((ps, pe, pi)) => {
                    children.extend(unmodeled_nodes(ctx, &format!("{}.{}", id_prefix, pi), ps, pe));
                    Some((s, e, i))
                }
                None => Some((s, e, i)),
            };
            continue;
        }
        if let Some((ps, pe, pi)) = pending.take() {
            children.extend(unmodeled_nodes(ctx, &format!("{}.{}", id_prefix, pi), ps, pe));
        }
        if child.kind() == "name" && TS_ENTITY_TYPES.contains(&kind) {
            continue;
        }
        if let Some(converted) = ts_convert(child, ctx, &format!("{}.{}", id_prefix, i)) {
            children.push(converted);
        }
    }
    if let Some((ps, pe, pi)) = pending.take() {
        children.extend(unmodeled_nodes(ctx, &format!("{}.{}", id_prefix, pi), ps, pe));
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
    let ctx = Ctx::new(source);
    let mut semantic_root = ts_convert(root, &ctx, "0")?;
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
    let ctx = Ctx::new(source);

    for (idx, raw_line) in source.lines().enumerate() {
        let lineno = idx as u32;
        let trimmed = raw_line.trim();
        let trimmed_start = ctx.lines.starts[idx] + (raw_line.len() - raw_line.trim_start().len());
        let span = (trimmed_start, trimmed_start + trimmed.len());
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
                    root_children.push(leaf_span(&id, "report", &extract_name(&words, 1), lineno, end_col));
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
                push_statement(&ctx, &mut stack, trimmed, &upper, span, lineno, end_col);
            }
            _ => {
                // Top-level statements outside any block are executable
                // report code; keep them rather than certifying them away.
                let id = format!("0.{}", counter);
                counter += 1;
                root_children.push(statement_node(&ctx, &id, trimmed, &upper, span, lineno, end_col));
            }
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
    fn test_fallback_report_and_program_have_source_byte_spans() {
        // FORM is not handled by tree-sitter and intentionally forces the
        // conservative scanner used by the 74-language RC corpus fixture.
        // A nonempty report declaration must never get (0,0)–(0,0).
        for declaration in ["REPORT z_demo.", "PROGRAM z_café."] {
            let src = format!("{declaration}\n\nFORM greet.\n  WRITE: 'hi'.\nENDFORM.\n");
            assert!(parse_abap_tree_sitter(&src).is_none());
            let tree: SemanticNode = serde_json::from_str(&parse_abap(&src)).unwrap();
            let report = tree
                .children
                .iter()
                .find(|node| node.node_type == "report")
                .expect("report declaration node");
            assert_eq!(report.position.start_line, 0);
            assert_eq!(report.position.start_col, 0);
            assert_eq!(report.position.end_line, 0);
            assert_eq!(
                report.position.end_col,
                declaration.len() as u32,
                "end_col must use UTF-8 byte columns, not characters"
            );
            assert!(report.position.end_col > report.position.start_col);
        }
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

    // -- Regressions: DATA declarations keep typing and initializer (#7) --

    fn tree(src: &str) -> SemanticNode {
        serde_json::from_str(&parse_abap(src)).expect("valid tree json")
    }

    fn find<'a>(node: &'a SemanticNode, node_type: &str) -> Vec<&'a SemanticNode> {
        let mut out = Vec::new();
        if node.node_type == node_type {
            out.push(node);
        }
        for child in &node.children {
            out.extend(find(child, node_type));
        }
        out
    }

    fn span(node: &SemanticNode) -> (u32, u32, u32, u32) {
        let p = &node.position;
        (p.start_line, p.start_col, p.end_line, p.end_col)
    }

    fn only<'a>(node: &'a SemanticNode, node_type: &str) -> &'a SemanticNode {
        let found = find(node, node_type);
        assert_eq!(found.len(), 1, "expected exactly one {node_type}: {found:?}");
        found[0]
    }

    #[test]
    fn test_data_type_change_is_semantic() {
        // Core case abap-declaration-api: `TYPE i` -> `TYPE int8`.
        let old = "REPORT zprobe.\nDATA lv_result TYPE i.\nlv_result = 1.\nWRITE lv_result.\n";
        let new = "REPORT zprobe.\nDATA lv_result TYPE int8.\nlv_result = 1.\nWRITE lv_result.\n";
        assert!(parse_abap_tree_sitter(old).is_some());
        let (old_t, new_t) = (tree(old), tree(new));
        let (old_d, new_d) = (only(&old_t, "data_declaration"), only(&new_t, "data_declaration"));
        assert_eq!(old_d.label, "LV_RESULT");
        assert_eq!(new_d.label, "LV_RESULT");
        let (old_ty, new_ty) = (only(old_d, "type_annotation"), only(new_d, "type_annotation"));
        assert_eq!(old_ty.label, "TYPE I");
        assert_eq!(new_ty.label, "TYPE INT8");
        assert_eq!(span(old_ty), (1, 15, 1, 21));
        assert_eq!(span(new_ty), (1, 15, 1, 24));
        assert_ne!(old_d.structural_hash, new_d.structural_hash);
        assert_ne!(old_t.structural_hash, new_t.structural_hash);
    }

    #[test]
    fn test_data_value_initializer_change_is_semantic() {
        // Core case abap-data-initializer: the grammar puts `VALUE 1` in an
        // ERROR node, which used to be dropped.
        let old = "REPORT zprobe.\nDATA lv_result TYPE i VALUE 1.\nWRITE lv_result.\n";
        let new = "REPORT zprobe.\nDATA lv_result TYPE i VALUE 2.\nWRITE lv_result.\n";
        assert!(parse_abap_tree_sitter(old).is_some());
        let (old_t, new_t) = (tree(old), tree(new));
        let (old_d, new_d) = (only(&old_t, "data_declaration"), only(&new_t, "data_declaration"));
        let (old_i, new_i) = (only(old_d, "initializer"), only(new_d, "initializer"));
        assert_eq!(old_i.label, "1");
        assert_eq!(new_i.label, "2");
        assert_eq!(span(old_i), (1, 28, 1, 29));
        assert_eq!(span(new_i), (1, 28, 1, 29));
        assert_eq!(only(old_d, "type_annotation").label, "TYPE I");
        assert_ne!(old_d.structural_hash, new_d.structural_hash);
    }

    #[test]
    fn test_fallback_inline_data_initializer_change_is_semantic() {
        // Exact repro from issue #7 (FORM forces the fallback scanner).
        let old = "REPORT ztest.\nFORM f.\n  DATA(x) = 1.\nENDFORM.\n";
        let new = "REPORT ztest.\nFORM f.\n  DATA(x) = 2.\nENDFORM.\n";
        assert!(parse_abap_tree_sitter(old).is_none());
        let (old_t, new_t) = (tree(old), tree(new));
        let (old_d, new_d) = (only(&old_t, "data_declaration"), only(&new_t, "data_declaration"));
        assert_eq!(old_d.label, "X");
        assert_eq!(span(only(old_d, "identifier")), (2, 7, 2, 8));
        let (old_i, new_i) = (only(old_d, "initializer"), only(new_d, "initializer"));
        assert_eq!((old_i.label.as_str(), new_i.label.as_str()), ("1", "2"));
        assert_eq!(span(old_i), (2, 12, 2, 13));
        assert_ne!(old_t.structural_hash, new_t.structural_hash);
    }

    #[test]
    fn test_fallback_data_type_and_value_change_is_semantic() {
        let base = "REPORT ztest.\nFORM f.\n  DATA y TYPE i VALUE 'Ab'.\nENDFORM.\n";
        let d = |src: &str| -> (String, String) {
            let t = tree(src);
            let decl = only(&t, "data_declaration");
            (only(decl, "type_annotation").label.clone(), only(decl, "initializer").label.clone())
        };
        assert_eq!(d(base), ("TYPE I".into(), "'Ab'".into()));
        // Literal case is payload, not a case-insensitive keyword.
        assert_eq!(d(&base.replace("'Ab'", "'AB'")).1, "'AB'");
        assert_eq!(d(&base.replace("TYPE i", "TYPE int8")).0, "TYPE INT8");
    }

    #[test]
    fn test_top_level_inline_data_error_region_is_kept() {
        // The grammar has no inline declarations; the ERROR region must not
        // vanish from the tree.
        let old = "REPORT zprobe.\nDATA(x) = 1.\n";
        let new = "REPORT zprobe.\nDATA(x) = 2.\n";
        let (old_t, new_t) = (tree(old), tree(new));
        assert_eq!(only(&old_t, "initializer").label, "1");
        assert_eq!(only(&new_t, "initializer").label, "2");
        assert_ne!(old_t.structural_hash, new_t.structural_hash);
    }

    #[test]
    fn test_chained_data_entries_and_quoted_delimiters() {
        let src = "REPORT zprobe.\nDATA: a TYPE i VALUE 1, \" note, with comma\n      b TYPE c LENGTH 5 VALUE 'x.y,\"z'.\n";
        let t = tree(src);
        let chain = only(&t, "data_declaration_chain");
        let entries = find(chain, "data_declaration");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].label, "A");
        assert_eq!(only(entries[0], "initializer").label, "1");
        assert_eq!(entries[1].label, "B");
        assert_eq!(only(entries[1], "type_annotation").label, "TYPE C LENGTH 5");
        assert_eq!(only(entries[1], "initializer").label, "'x.y,\"z'");
        assert_eq!(span(only(entries[1], "initializer")), (2, 30, 2, 38));

        let changed = tree(&src.replace("VALUE 1,", "VALUE 2,"));
        assert_ne!(t.structural_hash, changed.structural_hash);
    }

    #[test]
    fn test_data_comment_and_layout_controls_are_not_semantic() {
        let base = "REPORT zprobe.\nDATA lv_result TYPE i VALUE 1.\n";
        let hash = tree(base).structural_hash;
        assert_eq!(tree(base).structural_hash, hash, "identity must be deterministic");
        for variant in [
            "REPORT zprobe.\nDATA lv_result TYPE i VALUE 1. \" trailing comment\n",
            "REPORT zprobe.\n* full-line comment\nDATA lv_result TYPE i VALUE 1.\n",
            "\nREPORT zprobe.\n\nDATA   lv_result   TYPE i   VALUE 1.\n\n",
            "REPORT zprobe.\ndata lv_result type i value 1.\n",
        ] {
            assert_eq!(tree(variant).structural_hash, hash, "{variant:?}");
        }
    }

    // -- Regressions: backtick literals stay string payloads (#8) --

    #[test]
    fn test_backtick_literal_payload_change_is_not_identifier() {
        // Core case abap-unicode-payload: used to emit identifiers CAF/CAFE.
        let old = "REPORT zprobe.\nWRITE `café`.\n";
        let new = "REPORT zprobe.\nWRITE `cafe雪`.\n";
        let (old_t, new_t) = (tree(old), tree(new));
        let (old_w, new_w) = (only(&old_t, "write_statement"), only(&new_t, "write_statement"));
        assert!(find(old_w, "identifier").is_empty(), "{old_w:?}");
        assert!(find(new_w, "identifier").is_empty(), "{new_w:?}");
        let (old_s, new_s) = (only(old_w, "string_literal"), only(new_w, "string_literal"));
        assert_eq!(old_s.label, "`café`");
        assert_eq!(new_s.label, "`cafe雪`");
        // 0-based rows, UTF-8 byte columns.
        assert_eq!(span(old_s), (1, 6, 1, 13));
        assert_eq!(span(new_s), (1, 6, 1, 15));
        assert_ne!(old_s.structural_hash, new_s.structural_hash);
        assert_eq!(tree(old).structural_hash, old_t.structural_hash);
    }

    #[test]
    fn test_backtick_literals_with_escapes_and_delimiters() {
        let src = "REPORT zprobe.\nWRITE `it``s. a \"b`.\n";
        let t = tree(src);
        let w = only(&t, "write_statement");
        assert!(find(w, "identifier").is_empty(), "{w:?}");
        assert_eq!(only(w, "string_literal").label, "`it``s. a \"b`");
        let changed = tree("REPORT zprobe.\nWRITE `it``s. a \"c`.\n");
        assert_ne!(t.structural_hash, changed.structural_hash);
        // Layout around the literal is not semantic.
        let relaid = tree("REPORT zprobe.\nWRITE   `it``s. a \"b`. \" note\n");
        assert_eq!(t.structural_hash, relaid.structural_hash);
    }

    #[test]
    fn test_fallback_keeps_top_level_literal_statements() {
        // Enough unlexed literals push tree-sitter into the fallback scanner,
        // which used to drop every statement outside a block.
        let old = "REPORT zprobe.\nWRITE: `a b`, 'x'.\nWRITE |a { x } b|.\nWRITE `it``s`.\n";
        let new = old.replace("`a b`", "`a c`");
        let (old_t, new_t) = (tree(old), tree(&new));
        let writes = find(&old_t, "write_statement");
        assert_eq!(writes.len(), 3);
        assert_eq!(writes[0].label, "`a b`, 'x'");
        assert_ne!(old_t.structural_hash, new_t.structural_hash);
    }
}
