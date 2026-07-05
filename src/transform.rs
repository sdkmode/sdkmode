//! Rewrites a turn's JavaScript so it runs as one step of a persistent,
//! multi-step REPL session (see [`crate::sandbox::Session`]).
//!
//! Each step runs as a script against a shared global scope. Three problems
//! follow and are solved here:
//!
//!   1. **Top-level `await`** is illegal in a bare script, so the step is run
//!      inside an `async` IIFE.
//!   2. **`return` as the answer.** The agent ends its turn by `return`ing a
//!      value. Because the body runs inside the IIFE, a top-level `return`
//!      resolves that IIFE — we capture the resolved value as the answer.
//!   3. **Persistence even across a `return`.** Bindings are declared with
//!      `var` (so they live in the IIFE's function scope, visible to `finally`)
//!      and lifted onto `globalThis` in a `finally` block, which runs even when
//!      the body returns or throws.
//!
//! A bare trailing expression is echoed to the scratchpad via `console.log`, and
//! top-level `import` declarations are converted to dynamic `await import(...)`.

use deno_ast::swc::ast::{
    AssignOp, AssignTarget, AssignTargetPat, Callee, Decl, Expr, ImportDecl, ImportSpecifier,
    Module, ModuleDecl, ModuleExportName, ModuleItem, ObjectPatProp, Pat, SimpleAssignTarget,
    Stmt, VarDeclKind,
};
use deno_ast::{
    MediaType, ModuleSpecifier, ParseParams, ParsedSource, ProgramRef, SourceRangedForSpanned,
    StartSourcePos,
};

/// Wrap one step's source for execution in the shared session scope.
pub fn wrap_turn(code: &str) -> String {
    let (body, assign) = build_body(code);
    wrap(&body, &assign)
}

/// Whether `code` is a syntactically valid step — either as a module, or as
/// statements that allow a top-level `return` (how the agent answers). Used to
/// detect and trim any prose the model may have leaked before its code.
pub fn is_parseable(code: &str) -> bool {
    parse(code).is_some()
        || parse(&format!("async function __sdkmode_wrap() {{\n{code}\n}}")).is_some()
}

/// The prefix that makes a top-level `return` parseable (see [`build_body`]).
const WRAP_PREFIX: &str = "async function __sdkmode_wrap() {\n";

/// The names a step's top-level declarations bind — variables (including
/// destructuring), functions, classes, and import locals. This is exactly the
/// set the step's finally-lift persists onto `globalThis`, so it is what the
/// transcript deallocates when the step is deleted from `context`.
pub fn declared_names(code: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();

    if let Some(parsed) = parse(code)
        && let ProgramRef::Module(module) = parsed.program_ref()
    {
        for item in &module.body {
            match item {
                ModuleItem::ModuleDecl(ModuleDecl::Import(import)) => {
                    for specifier in &import.specifiers {
                        let local = match specifier {
                            ImportSpecifier::Default(spec) => &spec.local,
                            ImportSpecifier::Namespace(spec) => &spec.local,
                            ImportSpecifier::Named(spec) => &spec.local,
                        };
                        names.push(local.sym.to_string());
                    }
                }
                ModuleItem::Stmt(stmt) => collect_stmt_names(stmt, &mut names),
                _ => {}
            }
        }
    } else {
        let wrapped = format!("{WRAP_PREFIX}{code}\n}}");
        if let Some(parsed) = parse(&wrapped)
            && let ProgramRef::Module(module) = parsed.program_ref()
            && let Some(ModuleItem::Stmt(Stmt::Decl(Decl::Fn(func)))) = module.body.first()
            && let Some(block) = &func.function.body
        {
            for stmt in &block.stmts {
                collect_stmt_names(stmt, &mut names);
            }
        }
    }

    names.sort();
    names.dedup();
    names
}

/// Record the names a single top-level statement declares. Bare assignments
/// (`test = 123`) count too: in the step's sloppy-mode scope they create a
/// global just like the finally-lift does, so they must be snapshotted and
/// deallocated the same way.
fn collect_stmt_names(stmt: &Stmt, names: &mut Vec<String>) {
    match stmt {
        Stmt::Decl(Decl::Var(var)) => {
            for declarator in &var.decls {
                collect_pat_names(&declarator.name, names);
            }
        }
        Stmt::Decl(Decl::Fn(func)) => names.push(func.ident.sym.to_string()),
        Stmt::Decl(Decl::Class(class)) => names.push(class.ident.sym.to_string()),
        Stmt::Expr(expr_stmt) => collect_assign_names(&expr_stmt.expr, names),
        _ => {}
    }
}

/// Record the targets of a plain assignment expression, following chains
/// (`a = b = 1`). Compound assignments (`x += 1`) mutate an existing binding
/// rather than creating one, and member targets (`obj.x = 1`) are not
/// globals; both are skipped.
fn collect_assign_names(expr: &Expr, names: &mut Vec<String>) {
    let Expr::Assign(assign) = expr else {
        return;
    };
    if assign.op != AssignOp::Assign {
        return;
    }
    match &assign.left {
        AssignTarget::Simple(SimpleAssignTarget::Ident(ident)) => {
            names.push(ident.id.sym.to_string());
        }
        AssignTarget::Pat(AssignTargetPat::Array(array)) => {
            collect_pat_names(&Pat::Array(array.clone()), names);
        }
        AssignTarget::Pat(AssignTargetPat::Object(object)) => {
            collect_pat_names(&Pat::Object(object.clone()), names);
        }
        _ => {}
    }
    collect_assign_names(&assign.right, names);
}

/// Rewrite top-level `const` declarations to `let` — for the *stored*
/// transcript text, not for execution (the runtime already erases the
/// distinction by rewriting to `var`). Any top-level binding can disappear
/// when its step is deleted from `context`, so a `const` in the rendered
/// history would promise a permanence that does not exist. Nested `const`
/// (inside functions and blocks) never persists, so it is left alone.
pub fn const_to_let(code: &str) -> String {
    let mut offsets: Vec<usize> = Vec::new();

    if let Some(parsed) = parse(code)
        && let ProgramRef::Module(module) = parsed.program_ref()
    {
        let start = parsed.text_info_lazy().range().start;
        for item in &module.body {
            if let ModuleItem::Stmt(stmt) = item {
                collect_const_offsets(stmt, start, &mut offsets);
            }
        }
    } else {
        let wrapped = format!("{WRAP_PREFIX}{code}\n}}");
        if let Some(parsed) = parse(&wrapped)
            && let ProgramRef::Module(module) = parsed.program_ref()
            && let Some(ModuleItem::Stmt(Stmt::Decl(Decl::Fn(func)))) = module.body.first()
            && let Some(block) = &func.function.body
        {
            let start = parsed.text_info_lazy().range().start;
            let mut wrapped_offsets: Vec<usize> = Vec::new();
            for stmt in &block.stmts {
                collect_const_offsets(stmt, start, &mut wrapped_offsets);
            }
            // Map offsets in the wrapped text back onto the original.
            offsets.extend(
                wrapped_offsets
                    .into_iter()
                    .filter_map(|offset| offset.checked_sub(WRAP_PREFIX.len())),
            );
        }
    }

    // Replace back-to-front so earlier offsets stay valid ("let" is shorter).
    let mut out = code.to_string();
    for offset in offsets.into_iter().rev() {
        if out[offset..].starts_with("const") {
            out.replace_range(offset..offset + "const".len(), "let");
        }
    }
    out
}

/// Record the byte offset of a top-level `const` keyword, if `stmt` is one.
fn collect_const_offsets(stmt: &Stmt, start: StartSourcePos, offsets: &mut Vec<usize>) {
    if let Stmt::Decl(Decl::Var(var)) = stmt
        && var.kind == VarDeclKind::Const
    {
        offsets.push(var.range().as_byte_range(start).start);
    }
}

/// Parse and transform the step body, returning (body, globalThis-assign).
fn build_body(code: &str) -> (String, String) {
    // First parse as a module: supports `import` and ordinary declarations.
    if let Some(parsed) = parse(code)
        && let ProgramRef::Module(module) = parsed.program_ref()
    {
        return transform_module(module, &parsed);
    }

    // Retry wrapped in a function so a top-level `return` (how the agent answers)
    // is syntactically legal. Static imports are not valid here, but the agent is
    // told to use dynamic import instead.
    let wrapped = format!("async function __sdkmode_wrap() {{\n{code}\n}}");
    if let Some(parsed) = parse(&wrapped)
        && let ProgramRef::Module(module) = parsed.program_ref()
        && let Some(ModuleItem::Stmt(Stmt::Decl(Decl::Fn(func)))) = module.body.first()
        && let Some(block) = &func.function.body
    {
        return transform_stmts(&block.stmts, &parsed);
    }

    // Unparseable: run verbatim so the runtime surfaces the syntax error.
    (code.to_string(), String::new())
}

fn transform_module(module: &Module, parsed: &ParsedSource) -> (String, String) {
    let text_info = parsed.text_info_lazy();
    let mut names: Vec<String> = Vec::new();
    let mut body = String::new();
    let last = module.body.len().saturating_sub(1);

    for (index, item) in module.body.iter().enumerate() {
        match item {
            ModuleItem::ModuleDecl(ModuleDecl::Import(import)) => {
                body.push_str(&import_to_dynamic(import, &mut names));
            }
            ModuleItem::Stmt(stmt) => emit_stmt(stmt, index == last, parsed, &mut body, &mut names),
            other => {
                body.push_str(other.text_fast(text_info).trim_end());
                body.push('\n');
            }
        }
    }

    (body, assemble_assign(names))
}

fn transform_stmts(stmts: &[Stmt], parsed: &ParsedSource) -> (String, String) {
    let mut names: Vec<String> = Vec::new();
    let mut body = String::new();
    let last = stmts.len().saturating_sub(1);

    for (index, stmt) in stmts.iter().enumerate() {
        emit_stmt(stmt, index == last, parsed, &mut body, &mut names);
    }

    (body, assemble_assign(names))
}

/// Emit one statement into the body, swapping `const`/`let` for `var`, turning a
/// `class` declaration into a `var` assignment, echoing a bare trailing
/// expression, and recording declared names for persistence.
fn emit_stmt(
    stmt: &Stmt,
    is_last: bool,
    parsed: &ParsedSource,
    body: &mut String,
    names: &mut Vec<String>,
) {
    let text_info = parsed.text_info_lazy();
    match stmt {
        Stmt::Decl(Decl::Var(var)) => {
            for declarator in &var.decls {
                collect_pat_names(&declarator.name, names);
            }
            let text = stmt.text_fast(text_info);
            let trimmed = text.trim_start();
            let keyword = match var.kind {
                VarDeclKind::Const => "const",
                VarDeclKind::Let => "let",
                VarDeclKind::Var => "var",
            };
            body.push_str("var");
            body.push_str(trimmed.strip_prefix(keyword).unwrap_or(trimmed).trim_end());
            body.push('\n');
        }
        Stmt::Decl(Decl::Fn(func)) => {
            names.push(func.ident.sym.to_string());
            body.push_str(stmt.text_fast(text_info).trim_end());
            body.push('\n');
        }
        Stmt::Decl(Decl::Class(class)) => {
            let name = class.ident.sym.to_string();
            let text = stmt.text_fast(text_info).trim_end().trim_end_matches(';');
            body.push_str(&format!("var {name} = {text};\n"));
            names.push(name);
        }
        Stmt::Expr(expr_stmt) if is_last && should_log(&expr_stmt.expr) => {
            let expr_text = expr_stmt.expr.text_fast(text_info).trim();
            body.push_str(&format!("console.log({expr_text});\n"));
        }
        _ => {
            body.push_str(stmt.text_fast(text_info).trim_end());
            body.push('\n');
        }
    }
}

fn assemble_assign(mut names: Vec<String>) -> String {
    if names.is_empty() {
        return String::new();
    }
    names.sort();
    names.dedup();
    format!(";Object.assign(globalThis, {{ {} }});\n", names.join(", "))
}

/// Wrap the transformed body in the step harness.
fn wrap(body: &str, assign: &str) -> String {
    const TEMPLATE: &str = r#"(async () => {
globalThis.__sdkmode_returned = false;
globalThis.__sdkmode_value = undefined;
const __sdkmode_result = await (async () => {
try {
__BODY__
} finally {
__ASSIGN__}
})();
if (typeof __sdkmode_result !== "undefined") {
globalThis.__sdkmode_returned = true;
globalThis.__sdkmode_value = (typeof __sdkmode_result === "string") ? __sdkmode_result : (() => { try { return JSON.stringify(__sdkmode_result); } catch (_e) { return String(__sdkmode_result); } })();
}
})();"#;
    TEMPLATE
        .replace("__BODY__", body)
        .replace("__ASSIGN__", assign)
}

fn parse(code: &str) -> Option<ParsedSource> {
    deno_ast::parse_module(ParseParams {
        specifier: ModuleSpecifier::parse("file:///turn.js").unwrap(),
        text: code.into(),
        media_type: MediaType::JavaScript,
        capture_tokens: false,
        scope_analysis: false,
        maybe_syntax: None,
    })
    .ok()
}

/// Whether a trailing expression is worth echoing: skip assignments (no useful
/// value) and `console.*` calls (already printing).
fn should_log(expr: &Expr) -> bool {
    match expr {
        Expr::Assign(_) => false,
        Expr::Call(call) if is_console_call(call) => false,
        _ => true,
    }
}

fn is_console_call(call: &deno_ast::swc::ast::CallExpr) -> bool {
    let Callee::Expr(callee) = &call.callee else {
        return false;
    };
    let Expr::Member(member) = &**callee else {
        return false;
    };
    matches!(&*member.obj, Expr::Ident(ident) if ident.sym.as_ref() == "console")
}

/// Record every identifier bound by a (possibly destructuring) pattern.
fn collect_pat_names(pat: &Pat, out: &mut Vec<String>) {
    match pat {
        Pat::Ident(ident) => out.push(ident.sym.to_string()),
        Pat::Array(array) => {
            for element in array.elems.iter().flatten() {
                collect_pat_names(element, out);
            }
        }
        Pat::Object(object) => {
            for prop in &object.props {
                match prop {
                    ObjectPatProp::KeyValue(kv) => collect_pat_names(&kv.value, out),
                    ObjectPatProp::Assign(assign) => out.push(assign.key.sym.to_string()),
                    ObjectPatProp::Rest(rest) => collect_pat_names(&rest.arg, out),
                }
            }
        }
        Pat::Assign(assign) => collect_pat_names(&assign.left, out),
        Pat::Rest(rest) => collect_pat_names(&rest.arg, out),
        _ => {}
    }
}

/// Convert `import ... from "m"` into `var ... = await import("m")` statements
/// (`var` so they persist via the `finally` assign), recording local names.
fn import_to_dynamic(import: &ImportDecl, names: &mut Vec<String>) -> String {
    let src = js_string(&import.src.value.to_string_lossy());
    let mut out = String::new();
    let mut named: Vec<String> = Vec::new();

    for specifier in &import.specifiers {
        match specifier {
            ImportSpecifier::Default(default) => {
                let local = default.local.sym.to_string();
                names.push(local.clone());
                out.push_str(&format!("var {local} = (await import({src})).default;\n"));
            }
            ImportSpecifier::Namespace(namespace) => {
                let local = namespace.local.sym.to_string();
                names.push(local.clone());
                out.push_str(&format!("var {local} = await import({src});\n"));
            }
            ImportSpecifier::Named(spec) => {
                let local = spec.local.sym.to_string();
                let imported = match &spec.imported {
                    Some(ModuleExportName::Ident(ident)) => ident.sym.to_string(),
                    Some(ModuleExportName::Str(string)) => {
                        js_string(&string.value.to_string_lossy())
                    }
                    None => local.clone(),
                };
                names.push(local.clone());
                if imported == local {
                    named.push(local);
                } else {
                    named.push(format!("{imported}: {local}"));
                }
            }
        }
    }

    if !named.is_empty() {
        out.push_str(&format!(
            "var {{ {} }} = await import({src});\n",
            named.join(", ")
        ));
    }

    out
}

/// Quote a string as a JavaScript string literal.
fn js_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string())
}

#[cfg(test)]
mod tests {
    use super::wrap_turn;

    #[test]
    fn echoes_bare_trailing_expression() {
        let out = wrap_turn("17 * 23");
        assert!(out.contains("console.log(17 * 23);"), "{out}");
    }

    #[test]
    fn persists_const_as_var() {
        let out = wrap_turn("const x = 21;");
        assert!(out.contains("var x = 21;"), "{out}");
        assert!(out.contains("Object.assign(globalThis, { x });"), "{out}");
    }

    #[test]
    fn persists_destructured_and_function_names() {
        let out = wrap_turn("const { a, b: c } = obj;\nfunction greet() {}");
        assert!(out.contains("var { a, b: c } = obj;"), "{out}");
        assert!(
            out.contains("Object.assign(globalThis, { a, c, greet });"),
            "{out}"
        );
    }

    #[test]
    fn handles_top_level_return() {
        let out = wrap_turn("const total = 42;\nreturn total;");
        assert!(out.contains("var total = 42;"), "{out}");
        assert!(out.contains("return total;"), "{out}");
        assert!(
            out.contains("Object.assign(globalThis, { total });"),
            "{out}"
        );
    }

    #[test]
    fn converts_static_import_to_dynamic() {
        let out = wrap_turn("import { Octokit } from \"@octokit/rest\";");
        assert!(out.contains("await import(\"@octokit/rest\")"), "{out}");
        assert!(
            out.contains("Object.assign(globalThis, { Octokit });"),
            "{out}"
        );
    }

    #[test]
    fn does_not_echo_console_or_assignment() {
        let log = wrap_turn("console.log(42)");
        assert!(!log.contains("console.log(console.log(42))"), "{log}");
        let assign = wrap_turn("let y; y = 5");
        assert!(!assign.contains("console.log(y = 5)"), "{assign}");
    }

    #[test]
    fn const_to_let_rewrites_only_top_level() {
        let out = super::const_to_let(
            "const a = 1;\nfunction f() { const b = 2; }\nfor (const c of []) {}",
        );
        assert_eq!(
            out,
            "let a = 1;\nfunction f() { const b = 2; }\nfor (const c of []) {}"
        );
    }

    #[test]
    fn const_to_let_handles_top_level_return_and_multiple_decls() {
        let out = super::const_to_let("const total = 42;\nconst extra = 1;\nreturn total;");
        assert_eq!(out, "let total = 42;\nlet extra = 1;\nreturn total;");
    }

    #[test]
    fn const_to_let_leaves_unparseable_code_alone() {
        let broken = "const oops = ;";
        assert_eq!(super::const_to_let(broken), broken);
    }

    #[test]
    fn declared_names_covers_declarations_imports_and_destructuring() {
        let names = super::declared_names(
            "import { walk } from \"@std/fs\";\nconst { a, b: c } = obj;\nlet d = 1;\nfunction e() {}\nclass F {}",
        );
        assert_eq!(names, vec!["F", "a", "c", "d", "e", "walk"]);
    }

    #[test]
    fn declared_names_works_with_a_top_level_return() {
        assert_eq!(
            super::declared_names("const x = 1; return x;"),
            vec!["x"]
        );
    }

    /// A bare assignment creates a global exactly like a declaration does, so
    /// it must be tracked the same way — otherwise `test = 123` would persist
    /// in-session but silently vanish from snapshots.
    #[test]
    fn declared_names_includes_bare_assignments() {
        assert_eq!(
            super::declared_names("test = 123;\na = b = 2;\nexisting += 1;\nobj.field = 3;"),
            vec!["a", "b", "test"]
        );
    }
}
