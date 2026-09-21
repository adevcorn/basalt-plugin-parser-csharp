// basalt-plugin-parser-csharp/src/lib.rs — tree-sitter C# parser WASM plugin for Basalt

use tree_sitter::{Language, Node, Parser, Query, QueryCursor};

// ── Retrieval-chunk v2 contract ─────────────────────────────────────────────
//
// Mirrors the host contract in basalt-core/src/parser.rs (and the same-named
// constants in basalt-plugin-sdk for SDK-based plugins). v2 records are
// 106 bytes: the v1 104-byte record plus a flags byte at offset 104 and a
// reserved byte at 105. Flags: low 2 bits = visibility (0 unknown, 1 public,
// 2 internal, 3 private), bit 0x04 = test-attributed symbol.
const CHUNK_BYTES_V2: usize = 106;
const CHUNK_FLAGS_OFF_V2: usize = 104;
const VIS_UNKNOWN: u8 = 0;
const VIS_PUBLIC: u8 = 1;
const VIS_INTERNAL: u8 = 2;
const VIS_PRIVATE: u8 = 3;
const FLAG_TEST: u8 = 0x04;

fn encode_chunk_flags(visibility: u8, is_test: bool) -> u8 {
    (visibility & 0x03) | if is_test { FLAG_TEST } else { 0 }
}

const SRC_OFFSET: usize = 0;
const OUT_OFFSET: usize = 6 * 1024 * 1024;
const MEMORY_BYTES: usize = 12 * 1024 * 1024;

const SCOPE_KEYWORD:   u8 = 1;
const SCOPE_STRING:    u8 = 2;
const SCOPE_NUMBER:    u8 = 3;
const SCOPE_COMMENT:   u8 = 4;
const SCOPE_TYPE:      u8 = 5;
const SCOPE_FUNCTION:  u8 = 6;
const SCOPE_OPERATOR:  u8 = 7;
const SCOPE_VARIABLE:  u8 = 10;
const SCOPE_NAMESPACE: u8 = 11;

static mut MEMORY: [u8; MEMORY_BYTES] = [0u8; MEMORY_BYTES];
static LANG_EXT: &[u8] = b"cs\0";

extern "C" { fn tree_sitter_c_sharp() -> Language; }

struct ParserState {
    parser: Parser,
    parse_query: Query,
    retrieval_query: Query,
    call_sites_query: Query,
    parse_cap_names: Vec<String>,
    retrieval_cap_names: Vec<String>,
}

static mut STATE: Option<ParserState> = None;

unsafe fn get_state() -> Option<&'static mut ParserState> {
    if STATE.is_none() {
        let lang = tree_sitter_c_sharp();
        let mut parser = Parser::new();
        parser.set_language(lang).ok()?;

        let parse_query_src = r#"
            "using" @keyword "namespace" @keyword "class" @keyword "struct" @keyword
            "enum" @keyword "interface" @keyword "public" @keyword "private" @keyword
            "protected" @keyword "internal" @keyword "static" @keyword "readonly" @keyword
            "sealed" @keyword "abstract" @keyword "virtual" @keyword "override" @keyword
            "async" @keyword "await" @keyword "return" @keyword
            "if" @keyword "else" @keyword "for" @keyword "foreach" @keyword "while" @keyword
            "switch" @keyword "case" @keyword "break" @keyword "continue" @keyword
            "new" @keyword "this" @keyword "base" @keyword (null_literal) @keyword "true" @keyword "false" @keyword
            "try" @keyword "catch" @keyword "finally" @keyword "throw" @keyword
            "var" @keyword
            (comment) @comment
            (string_literal) @string
            (integer_literal) @number
            (predefined_type) @type
            (generic_name) @type
            (method_declaration name: (identifier) @function)
            (invocation_expression (identifier) @function)
            (invocation_expression
                (member_access_expression
                    name: (identifier) @function))
            "+" @operator "-" @operator "*" @operator "/" @operator
            "%" @operator "=" @operator "+=" @operator "-=" @operator
            "==" @operator "!=" @operator "<" @operator ">" @operator
            "&&" @operator "||" @operator "!" @operator
            "?" @operator "??" @operator "=>" @operator
            (parameter name: (identifier) @variable)
            (variable_declaration (variable_declarator (identifier) @variable))
        "#;
        let parse_query = Query::new(lang, parse_query_src).ok()?;
        let parse_cap_names: Vec<String> = parse_query.capture_names().iter().map(|s| s.to_string()).collect();

        let retrieval_query_src = r#"
            (method_declaration name: (_) @name.function) @chunk.function
            (constructor_declaration name: (_) @name.function) @chunk.function
            (class_declaration name: (_) @name.type) @chunk.type
            (struct_declaration name: (_) @name.type) @chunk.type
            (enum_declaration name: (_) @name.type) @chunk.type
            (interface_declaration name: (_) @name.type) @chunk.type
            (namespace_declaration name: (_) @name.module) @chunk.module
        "#;
        let retrieval_query = Query::new(lang, retrieval_query_src).ok()?;
        let retrieval_cap_names: Vec<String> = retrieval_query.capture_names().iter().map(|s| s.to_string()).collect();

        let call_sites_query_src = r#"
            (invocation_expression (identifier) @callee)
            (invocation_expression (member_access_expression name: (identifier) @callee))
        "#;
        let call_sites_query = Query::new(lang, call_sites_query_src).ok()?;

        STATE = Some(ParserState {
            parser,
            parse_query,
            retrieval_query,
            call_sites_query,
            parse_cap_names,
            retrieval_cap_names,
        });
    }
    STATE.as_mut()
}

#[no_mangle]
pub extern "C" fn basalt_lang() -> i32 {
    LANG_EXT.as_ptr() as i32
}

#[no_mangle]
pub unsafe extern "C" fn basalt_src_ptr() -> i32 {
    MEMORY[SRC_OFFSET..].as_ptr() as i32
}

#[no_mangle]
pub unsafe extern "C" fn basalt_out_ptr() -> i32 {
    MEMORY[OUT_OFFSET..].as_ptr() as i32
}

#[no_mangle]
pub unsafe extern "C" fn basalt_parse(
    src_ptr: i32, src_len: i32, out_ptr: i32, max_spans: i32,
) -> i32 {
    let src = std::slice::from_raw_parts(src_ptr as usize as *const u8, src_len as usize);
    let out = std::slice::from_raw_parts_mut(out_ptr as usize as *mut u8, (max_spans as usize) * 12);
    let state = match get_state() { Some(s) => s, None => return 0 };
    state.parser.reset();
    let Some(tree) = state.parser.parse(src, None) else { return 0 };
    let mut cursor = QueryCursor::new();
    let matches = cursor.matches(&state.parse_query, tree.root_node(), src);
    let mut count = 0usize;
    for m in matches {
        for cap in m.captures {
            if count >= max_spans as usize { break; }
            let scope_id = match state.parse_cap_names[cap.index as usize].as_str() {
                "keyword"   => SCOPE_KEYWORD,
                "string"    => SCOPE_STRING,
                "number"    => SCOPE_NUMBER,
                "comment"   => SCOPE_COMMENT,
                "type"      => SCOPE_TYPE,
                "function"  => SCOPE_FUNCTION,
                "operator"  => SCOPE_OPERATOR,
                "variable"  => SCOPE_VARIABLE,
                "namespace" => SCOPE_NAMESPACE,
                _ => 0,
            };
            let offset = cap.node.start_byte() as u32;
            let length = (cap.node.end_byte() - cap.node.start_byte()) as u32;
            let base = count * 12;
            out[base..base+4].copy_from_slice(&offset.to_le_bytes());
            out[base+4..base+8].copy_from_slice(&length.to_le_bytes());
            out[base+8] = scope_id;
            out[base+9] = 0; out[base+10] = 0; out[base+11] = 0;
            count += 1;
        }
    }
    count as i32
}

fn kind_byte(k: &str) -> u8 {
    match k {
        "module"    => 1,
        "type"      => 2,
        "function"  => 3,
        "interface" => 7,
        _           => 0,
    }
}

/// Test-marker attribute names (method- and type-level, xUnit/NUnit/MSTest).
const TEST_ATTRIBUTES: [&str; 11] = [
    "Fact",
    "Theory",
    "Test",
    "TestMethod",
    "TestCase",
    "TestFixture",
    "TestClass",
    "DataTestMethod",
    "DataRow",
    "SetUp",
    "TearDown",
];

/// True when an attribute-list's text names a test attribute.
pub fn is_test_attribute_text(text: &str) -> bool {
    text.split(|c: char| !c.is_alphanumeric())
        .any(|tok| TEST_ATTRIBUTES.contains(&tok))
}

/// Map C# modifier keywords to a chunk visibility value.
/// `in_interface`: interface members without modifiers are implicitly public.
/// `is_type`: top-level types without modifiers default to internal, members
/// to private. `protected` alone counts as public: it is API surface for
/// derived classes. Explicit interface implementations are implicitly public.
pub fn visibility_from_modifiers(mods: &[String], in_interface: bool, is_type: bool) -> u8 {
    let has = |m: &str| mods.iter().any(|x| x == m);
    if has("public") {
        return VIS_PUBLIC;
    }
    if has("private") {
        return VIS_PRIVATE;
    }
    if has("internal") {
        return VIS_INTERNAL;
    }
    if has("protected") {
        return VIS_PUBLIC;
    }
    if in_interface {
        return VIS_PUBLIC;
    }
    if is_type {
        return VIS_INTERNAL;
    }
    VIS_PRIVATE
}

fn has_child_kind(node: Node, kind: &str) -> bool {
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            if cursor.node().kind() == kind {
                return true;
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    false
}

/// Visibility of one declaration node from its `modifier` children
/// (tree-sitter-c-sharp attaches modifiers directly, unwrapped).
fn node_visibility(node: Node, src: &[u8], in_interface: bool, is_type: bool) -> u8 {
    if has_child_kind(node, "explicit_interface_specifier") {
        return VIS_PUBLIC;
    }
    let mut mods: Vec<String> = Vec::new();
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            if child.kind() == "modifier" {
                if let Ok(t) = child.utf8_text(src) {
                    mods.push(t.to_string());
                }
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    visibility_from_modifiers(&mods, in_interface, is_type)
}

/// Test-ness of one declaration node from `attribute_list` children
/// (tree-sitter-c-sharp nests them inside the declaration).
fn node_test_attr(node: Node, src: &[u8]) -> bool {
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            if child.kind() == "attribute_list" {
                if let Ok(t) = child.utf8_text(src) {
                    if is_test_attribute_text(t) {
                        return true;
                    }
                }
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    false
}

/// Ancestor-derived context for a declaration node: whether it sits inside an
/// interface body (members implicitly public) and whether an enclosing type
/// carries test attributes (members inherit test-ness).
fn enclosing_ctx(node: Node, src: &[u8]) -> (bool, bool) {
    let mut in_interface = false;
    let mut enclosing_test = false;
    let mut cur = node.parent();
    while let Some(n) = cur {
        match n.kind() {
            "interface_declaration" => {
                in_interface = true;
                if node_test_attr(n, src) {
                    enclosing_test = true;
                }
            }
            "class_declaration" | "struct_declaration" | "record_declaration" => {
                if node_test_attr(n, src) {
                    enclosing_test = true;
                }
            }
            _ => {}
        }
        cur = n.parent();
    }
    (in_interface, enclosing_test)
}

#[no_mangle]
pub unsafe extern "C" fn basalt_retrieval_chunks(
    src_ptr: i32, src_len: i32, out_ptr: i32, max_chunks: i32,
) -> i32 {
    let src = std::slice::from_raw_parts(src_ptr as usize as *const u8, src_len as usize);
    let out = std::slice::from_raw_parts_mut(out_ptr as usize as *mut u8, (max_chunks as usize) * 104);
    let state = match get_state() { Some(s) => s, None => return 0 };
    state.parser.reset();
    let Some(tree) = state.parser.parse(src, None) else { return 0 };
    let mut cursor = QueryCursor::new();
    let matches = cursor.matches(&state.retrieval_query, tree.root_node(), src);
    let mut count = 0usize;
    for m in matches {
        if count >= max_chunks as usize { break; }
        let mut offset = None::<u32>;
        let mut length = None::<u32>;
        let mut kind = None::<&str>;
        let mut name = None::<&str>;
        for cap in m.captures {
            let cn = &state.retrieval_cap_names[cap.index as usize];
            if let Some(k) = cn.strip_prefix("chunk.") {
                offset = Some(cap.node.start_byte() as u32);
                length = Some((cap.node.end_byte() - cap.node.start_byte()) as u32);
                kind = Some(k);
            } else if cn.starts_with("name.") {
                if let Ok(t) = cap.node.utf8_text(src) { name = Some(t.trim()); }
            }
        }
        let (Some(off), Some(len), Some(k)) = (offset, length, kind) else { continue };
        let label = if let Some(n) = name {
            let mut s = k.to_string(); s.push(' '); s.push_str(n); s
        } else { k.to_string() };
        let base = count * 104;
        out[base..base+4].copy_from_slice(&off.to_le_bytes());
        out[base+4..base+8].copy_from_slice(&len.to_le_bytes());
        let lbytes = label.as_bytes();
        let llen = lbytes.len().min(95);
        out[base+8..base+8+llen].copy_from_slice(&lbytes[..llen]);
        out[base+8+llen] = 0;
        out[base+103] = kind_byte(k);
        count += 1;
    }
    count as i32
}

/// v2 retrieval chunks: the v1 104-byte record plus the flags byte at offset
/// 104 (visibility low 2 bits, `FLAG_TEST`) and reserved byte at 105.
/// Flags come from the declaration node's own modifiers/attributes plus
/// ancestor context (interface bodies, test-attributed enclosing types).
#[no_mangle]
pub unsafe extern "C" fn basalt_retrieval_chunks_v2(
    src_ptr: i32, src_len: i32, out_ptr: i32, max_chunks: i32,
) -> i32 {
    let src = std::slice::from_raw_parts(src_ptr as usize as *const u8, src_len as usize);
    let out = std::slice::from_raw_parts_mut(out_ptr as usize as *mut u8, (max_chunks as usize) * CHUNK_BYTES_V2);
    let state = match get_state() { Some(s) => s, None => return 0 };
    state.parser.reset();
    let Some(tree) = state.parser.parse(src, None) else { return 0 };
    let mut cursor = QueryCursor::new();
    let matches = cursor.matches(&state.retrieval_query, tree.root_node(), src);
    let mut count = 0usize;
    for m in matches {
        if count >= max_chunks as usize { break; }
        let mut offset = None::<u32>;
        let mut length = None::<u32>;
        let mut kind = None::<&str>;
        let mut name = None::<&str>;
        let mut decl = None::<Node>;
        for cap in m.captures {
            let cn = &state.retrieval_cap_names[cap.index as usize];
            if let Some(k) = cn.strip_prefix("chunk.") {
                offset = Some(cap.node.start_byte() as u32);
                length = Some((cap.node.end_byte() - cap.node.start_byte()) as u32);
                kind = Some(k);
                decl = Some(cap.node);
            } else if cn.starts_with("name.") {
                if let Ok(t) = cap.node.utf8_text(src) { name = Some(t.trim()); }
            }
        }
        let (Some(off), Some(len), Some(k), Some(dn)) = (offset, length, kind, decl) else { continue };
        let label = if let Some(n) = name {
            let mut s = k.to_string(); s.push(' '); s.push_str(n); s
        } else { k.to_string() };
        let is_type = matches!(k, "type" | "interface" | "module");
        let (in_iface, enc_test) = enclosing_ctx(dn, src);
        let flags = encode_chunk_flags(
            node_visibility(dn, src, in_iface, is_type),
            node_test_attr(dn, src) || enc_test,
        );
        let base = count * CHUNK_BYTES_V2;
        out[base..base+4].copy_from_slice(&off.to_le_bytes());
        out[base+4..base+8].copy_from_slice(&len.to_le_bytes());
        let lbytes = label.as_bytes();
        let llen = lbytes.len().min(95);
        out[base+8..base+8+llen].copy_from_slice(&lbytes[..llen]);
        out[base+8+llen] = 0;
        out[base+103] = kind_byte(k);
        out[base+CHUNK_FLAGS_OFF_V2] = flags;
        out[base+105] = 0;
        count += 1;
    }
    count as i32
}

#[no_mangle]
pub unsafe extern "C" fn basalt_call_sites(
    src_ptr: i32, src_len: i32, out_ptr: i32, max_sites: i32,
) -> i32 {
    let src = std::slice::from_raw_parts(src_ptr as usize as *const u8, src_len as usize);
    let out = std::slice::from_raw_parts_mut(out_ptr as usize as *mut u8, (max_sites as usize) * 68);
    let state = match get_state() { Some(s) => s, None => return 0 };
    state.parser.reset();
    let Some(tree) = state.parser.parse(src, None) else { return 0 };
    let mut cursor = QueryCursor::new();
    let matches = cursor.matches(&state.call_sites_query, tree.root_node(), src);
    let mut count = 0usize;
    for m in matches {
        if count >= max_sites as usize { break; }
        for cap in m.captures {
            let Ok(name) = cap.node.utf8_text(src) else { continue };
            let name = name.trim();
            if name.is_empty() { continue; }
            let offset = cap.node.start_byte() as u32;
            let base = count * 68;
            out[base..base+4].copy_from_slice(&offset.to_le_bytes());
            let nb = name.as_bytes();
            let nlen = nb.len().min(63);
            out[base+4..base+4+nlen].copy_from_slice(&nb[..nlen]);
            out[base+4+nlen] = 0;
            count += 1;
        }
    }
    count as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mods(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    #[test]
    fn visibility_from_modifiers_cases() {
        assert_eq!(visibility_from_modifiers(&mods(&["public"]), false, false), VIS_PUBLIC);
        assert_eq!(
            visibility_from_modifiers(&mods(&["public", "static"]), false, false),
            VIS_PUBLIC
        );
        assert_eq!(visibility_from_modifiers(&mods(&["private"]), false, false), VIS_PRIVATE);
        assert_eq!(visibility_from_modifiers(&mods(&["internal"]), false, false), VIS_INTERNAL);
        assert_eq!(
            visibility_from_modifiers(&mods(&["protected", "internal"]), false, false),
            VIS_INTERNAL
        );
        // No modifiers: members default to private, types to internal,
        // interface members are implicitly public.
        assert_eq!(visibility_from_modifiers(&[], false, false), VIS_PRIVATE);
        assert_eq!(visibility_from_modifiers(&[], false, true), VIS_INTERNAL);
        assert_eq!(visibility_from_modifiers(&[], true, false), VIS_PUBLIC);
    }

    #[test]
    fn test_attribute_text_cases() {
        assert!(is_test_attribute_text("[Fact]"));
        assert!(is_test_attribute_text("[Theory(DisplayName = \"x\")]"));
        assert!(is_test_attribute_text("[TestMethod]"));
        assert!(is_test_attribute_text("[TestCase(1, 2)]"));
        assert!(is_test_attribute_text("[TestClass]"));
        assert!(!is_test_attribute_text("[HttpGet]"));
        assert!(!is_test_attribute_text("[MyFactory]"));
        assert!(!is_test_attribute_text("[Factotum]"));
    }

    fn parse(src: &str) -> tree_sitter::Tree {
        let mut parser = Parser::new();
        parser
            .set_language(tree_sitter_c_sharp::language())
            .expect("c-sharp grammar");
        parser.parse(src, None).expect("parse ok")
    }

    fn find_method<'a>(node: Node<'a>, name: &str, src: &[u8]) -> Option<Node<'a>> {
        if node.kind() == "method_declaration" {
            if let Some(nn) = node.child_by_field_name("name") {
                if nn.utf8_text(src).map_or(false, |t| t.trim() == name) {
                    return Some(node);
                }
            }
        }
        let mut cursor = node.walk();
        if cursor.goto_first_child() {
            loop {
                if let Some(found) = find_method(cursor.node(), name, src) {
                    return Some(found);
                }
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }
        None
    }

    const SAMPLE: &str = r#"
using System;
using System.Threading.Tasks;

namespace MyApp.Services;

public class OrderService
{
    public async Task<Order> GetOrderAsync(int id) { return null; }
    private void Log(string msg) { }
    [Fact]
    public void GetOrder_ReturnsOrder() { }
}
"#;

    #[test]
    fn node_flags_match_sample() {
        let src = SAMPLE.as_bytes();
        let tree = parse(SAMPLE);
        let root = tree.root_node();

        let m = find_method(root, "GetOrderAsync", src).expect("method found");
        assert_eq!(node_visibility(m, src, false, false), VIS_PUBLIC);
        assert!(!node_test_attr(m, src));

        let m = find_method(root, "Log", src).expect("method found");
        assert_eq!(node_visibility(m, src, false, false), VIS_PRIVATE);

        let m = find_method(root, "GetOrder_ReturnsOrder", src).expect("method found");
        assert_eq!(node_visibility(m, src, false, false), VIS_PUBLIC);
        assert!(node_test_attr(m, src));
    }

    #[test]
    fn enclosing_ctx_propagates_test_class() {
        let src = b"[TestClass] public class T { public void N() { } }";
        let tree = parse(std::str::from_utf8(src).unwrap());
        let root = tree.root_node();
        let m = find_method(root, "N", src).expect("method found");
        let (in_iface, enc_test) = enclosing_ctx(m, src);
        assert!(!in_iface);
        assert!(enc_test);
    }

    fn find_ctor(node: Node<'_>) -> Option<Node<'_>> {
        if node.kind() == "constructor_declaration" {
            return Some(node);
        }
        let mut cursor = node.walk();
        if cursor.goto_first_child() {
            loop {
                if let Some(found) = find_ctor(cursor.node()) {
                    return Some(found);
                }
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }
        None
    }

    #[test]
    fn constructor_capture_pattern_is_valid_and_flagged() {
        // The exact capture added to retrieval_query_src must compile;
        // a bad pattern would fail Query::new and disable the whole plugin.
        let q = tree_sitter::Query::new(
            tree_sitter_c_sharp::language(),
            "(constructor_declaration name: (_) @name.function) @chunk.function",
        );
        assert!(q.is_ok(), "constructor capture pattern must be valid");

        let src = "public class A { public A(int x) { } static A() { } }";
        let tree = parse(src);
        let pub_ctor = find_ctor(tree.root_node()).expect("ctor found");
        assert_eq!(
            node_visibility(pub_ctor, src.as_bytes(), false, false),
            VIS_PUBLIC
        );
    }
}
