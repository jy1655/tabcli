//! Keep subprocess work out of production `windows/security.rs` (#305, #306).
//!
//! The module once spawned `whoami.exe` for every private-permission operation, which
//! dominated the Windows serial test runtime. This policy parses its production items and
//! accepts only an exact set of external paths, imports and macros, so a new dependency or
//! indirection fails until it is reviewed here. Formatting, comments and string contents
//! do not matter. Only the exact `#[cfg(test)] mod tests;` declaration is exempt: its
//! oracle runs `whoami` deliberately.
//!
//! This is not a resolver. Methods and associated items through `Self` or a local type
//! alias are not resolved, so the allowlisted items are trusted not to spawn.
//! Import aliases are file-wide rather than lexically scoped, which can only reject
//! more. Unqualified `format!`, `vec!` and `bail!` are looked up in textual macro scope
//! first: a `macro_rules!` of that name in an ancestor module, or one brought in with
//! `#[macro_use]`, would shadow them undetected. Other modules are outside this policy;
//! moving the SID source out of this file leaves exceptions unused, which fails.
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
};
use syn::{
    AttrStyle, Expr, ImplItem, Item, Meta, Pat, Token, TraitItem, Type, TypeParamBound, Visibility,
    parse::ParseStream, punctuated::Punctuated, visit::Visit,
};

const MODULE: &str = "src/native/terminal/windows/security.rs";
const PARENT: &str = "src/native/terminal/windows/mod.rs";

// Every external item the production module names, after `use` aliases are expanded.
const ALLOWED: &[&str] = &[
    "String::from_utf16",
    "anyhow::Context",
    "anyhow::Error::new",
    "anyhow::Result",
    "std::ffi::OsStr",
    "std::ffi::OsStr::new",
    "std::io::Error::from_raw_os_error",
    "std::io::Error::last_os_error",
    "std::iter::once",
    "std::mem::size_of",
    "std::os::windows::ffi::OsStrExt",
    "std::os::windows::io::AsRawHandle",
    "std::os::windows::io::FromRawHandle",
    "std::os::windows::io::OwnedHandle",
    "std::os::windows::io::OwnedHandle::from_raw_handle",
    "std::path::Path",
    "std::ptr::null_mut",
    "std::slice::from_raw_parts",
    "windows_sys::Win32::Foundation::ERROR_INSUFFICIENT_BUFFER",
    "windows_sys::Win32::Foundation::LocalFree",
    "windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW",
    "windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW",
    "windows_sys::Win32::Security::Authorization::SDDL_REVISION_1",
    "windows_sys::Win32::Security::Authorization::SE_FILE_OBJECT",
    "windows_sys::Win32::Security::Authorization::SetNamedSecurityInfoW",
    "windows_sys::Win32::Security::DACL_SECURITY_INFORMATION",
    "windows_sys::Win32::Security::GetSecurityDescriptorDacl",
    "windows_sys::Win32::Security::GetTokenInformation",
    "windows_sys::Win32::Security::PROTECTED_DACL_SECURITY_INFORMATION",
    "windows_sys::Win32::Security::TOKEN_QUERY",
    "windows_sys::Win32::Security::TOKEN_USER",
    "windows_sys::Win32::Security::TokenUser",
    "windows_sys::Win32::System::Threading::GetCurrentProcess",
    "windows_sys::Win32::System::Threading::OpenProcessToken",
];
// Their arguments are parsed as expressions and inspected; other macros stay unexpanded.
const MACROS: &[&str] = &["anyhow::anyhow", "anyhow::bail", "format", "vec"];

struct Import {
    path: String,
    alias: Option<String>,
}

// A glob keeps its `*` segment, which no exception names.
fn imports(tree: &syn::UseTree, prefix: &[String], found: &mut Vec<Import>) {
    let join = |ident: &syn::Ident| [prefix, &[ident.to_string()]].concat().join("::");
    match tree {
        syn::UseTree::Path(path) => imports(
            &path.tree,
            &[prefix, &[path.ident.to_string()]].concat(),
            found,
        ),
        syn::UseTree::Name(name) if name.ident == "self" => found.push(Import {
            path: prefix.join("::"),
            alias: prefix.last().cloned(),
        }),
        syn::UseTree::Name(name) => found.push(Import {
            path: join(&name.ident),
            alias: Some(name.ident.to_string()),
        }),
        syn::UseTree::Rename(rename) => found.push(Import {
            path: if rename.ident == "self" {
                prefix.join("::")
            } else {
                join(&rename.ident)
            },
            alias: (rename.rename != "_").then(|| rename.rename.to_string()),
        }),
        syn::UseTree::Glob(_) => found.push(Import {
            path: [prefix, &["*".into()]].concat().join("::"),
            alias: None,
        }),
        syn::UseTree::Group(group) => {
            for tree in &group.items {
                imports(tree, prefix, found);
            }
        }
    }
}

fn use_items(item: &syn::ItemUse) -> Vec<Import> {
    let mut found = Vec::new();
    let root = if item.leading_colon.is_some() {
        vec![String::new()]
    } else {
        Vec::new()
    };
    imports(&item.tree, &root, &mut found);
    found
}

fn path_text(path: &syn::Path) -> String {
    let segments = path
        .segments
        .iter()
        .map(|segment| segment.ident.to_string());
    let mut text = if path.leading_colon.is_some() {
        vec![String::new()]
    } else {
        Vec::new()
    };
    text.extend(segments);
    text.join("::")
}

#[derive(Default)]
struct Names {
    aliases: BTreeMap<String, String>,
    local: BTreeSet<String>,
    ambiguous: Vec<String>,
}
impl<'ast> Visit<'ast> for Names {
    fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
        for import in use_items(item) {
            if let Some(alias) = import.alias
                && let Some(old) = self.aliases.insert(alias.clone(), import.path.clone())
                && old != import.path
            {
                self.ambiguous.push(format!("ambiguous alias {alias}"));
            }
        }
    }
    fn visit_file(&mut self, file: &'ast syn::File) {
        // Only module-level type names shadow dependency roots throughout this file.
        // A function-local `struct std;` must not hide an external `std::...` elsewhere.
        for item in &file.items {
            let ident = match item {
                Item::Enum(item) => Some(&item.ident),
                Item::Struct(item) => Some(&item.ident),
                Item::Trait(item) => Some(&item.ident),
                Item::TraitAlias(item) => Some(&item.ident),
                Item::Type(item) => Some(&item.ident),
                Item::Union(item) => Some(&item.ident),
                _ => None,
            };
            if let Some(ident) = ident {
                self.local.insert(ident.to_string());
            }
        }
        syn::visit::visit_file(self, file);
    }
}

struct Boundary {
    names: Names,
    used: BTreeSet<String>,
    rejected: Vec<String>,
    exempt: usize,
}
impl Boundary {
    fn require(&mut self, kind: &str, path: String, allowed: bool) {
        if allowed {
            self.used.insert(path);
        } else {
            self.rejected.push(format!("{kind} {path}"));
        }
    }
    // None is a local, prelude or single-segment name: without an import it cannot
    // reach another module. Macros have no such exemption.
    fn external(&self, path: &syn::Path, macro_path: bool) -> Option<String> {
        if path.leading_colon.is_some() {
            return Some(path_text(path));
        }
        let mut segments: Vec<_> = path.segments.iter().map(|s| s.ident.to_string()).collect();
        if let Some(target) = self.names.aliases.get(&segments[0]) {
            segments[0] = target.clone();
        } else if !macro_path
            && (segments.len() == 1
                || segments[0] == "Self"
                || self.names.local.contains(&segments[0]))
        {
            return None;
        }
        Some(segments.join("::"))
    }
}
impl<'ast> Visit<'ast> for Boundary {
    fn visit_item(&mut self, item: &'ast Item) {
        let kind = match item {
            Item::ExternCrate(_) => "extern crate".into(),
            Item::ForeignMod(_) => "extern block".into(),
            Item::Macro(item) => format!("item macro {}", path_text(&item.mac.path)),
            Item::Mod(item) => format!("module {}", item.ident),
            Item::Verbatim(_) => "unparsed item".into(),
            _ => return syn::visit::visit_item(self, item),
        };
        self.rejected.push(kind);
    }
    fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
        for attr in &item.attrs {
            self.visit_attribute(attr);
        }
        for import in use_items(item) {
            let allowed =
                ALLOWED.contains(&import.path.as_str()) || MACROS.contains(&import.path.as_str());
            self.require("import", import.path, allowed);
        }
    }
    fn visit_path(&mut self, path: &'ast syn::Path) {
        if let Some(external) = self.external(path, false) {
            let allowed = ALLOWED.contains(&external.as_str());
            self.require("path", external, allowed);
        }
        // Generic arguments carry their own paths.
        syn::visit::visit_path(self, path);
    }
    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        let name = self.external(&mac.path, true).unwrap();
        let allowed = MACROS.contains(&name.as_str());
        self.require("macro", name.clone(), allowed);
        if !allowed {
            return;
        }
        let arguments = mac
            .parse_body_with(Punctuated::<Expr, Token![,]>::parse_terminated)
            .map(|arguments| arguments.into_iter().collect::<Vec<_>>())
            .or_else(|_| {
                // `vec![element; length]`
                mac.parse_body_with(|input: ParseStream| {
                    let element = input.parse()?;
                    input.parse::<Token![;]>()?;
                    Ok(vec![element, input.parse()?])
                })
            });
        match arguments {
            Ok(arguments) => arguments
                .iter()
                .for_each(|argument| self.visit_expr(argument)),
            Err(_) => self
                .rejected
                .push(format!("uninspectable arguments of {name}")),
        }
    }
    fn visit_attribute(&mut self, attr: &'ast syn::Attribute) {
        let literal_doc = attr.path().is_ident("doc")
            && matches!(&attr.meta, Meta::NameValue(value) if matches!(value.value, Expr::Lit(_)));
        if !literal_doc {
            self.rejected
                .push(format!("attribute {}", path_text(attr.path())));
        }
    }
    // `pub(in crate::native)` names who may call, not a dependency.
    fn visit_visibility(&mut self, _: &'ast Visibility) {}
    fn visit_expr(&mut self, node: &'ast Expr) {
        if let Expr::Verbatim(_) = node {
            self.rejected.push("unparsed expression".into());
        }
        syn::visit::visit_expr(self, node);
    }
    fn visit_type(&mut self, node: &'ast Type) {
        if let Type::Verbatim(_) = node {
            self.rejected.push("unparsed type".into());
        }
        syn::visit::visit_type(self, node);
    }
    fn visit_pat(&mut self, node: &'ast Pat) {
        if let Pat::Verbatim(_) = node {
            self.rejected.push("unparsed pattern".into());
        }
        syn::visit::visit_pat(self, node);
    }
    fn visit_impl_item(&mut self, node: &'ast ImplItem) {
        if let ImplItem::Verbatim(_) = node {
            self.rejected.push("unparsed impl item".into());
        }
        syn::visit::visit_impl_item(self, node);
    }
    fn visit_trait_item(&mut self, node: &'ast TraitItem) {
        if let TraitItem::Verbatim(_) = node {
            self.rejected.push("unparsed trait item".into());
        }
        syn::visit::visit_trait_item(self, node);
    }
    fn visit_type_param_bound(&mut self, node: &'ast TypeParamBound) {
        if let TypeParamBound::Verbatim(_) = node {
            self.rejected.push("unparsed bound".into());
        }
        syn::visit::visit_type_param_bound(self, node);
    }
}

// Exactly `#[cfg(test)] mod tests;`: a path, cfg_attr, body or visibility changes what it names.
fn test_oracle(item: &Item) -> bool {
    let Item::Mod(module) = item else {
        return false;
    };
    module.ident == "tests"
        && module.content.is_none()
        && module.unsafety.is_none()
        && matches!(module.vis, Visibility::Inherited)
        && matches!(module.attrs.as_slice(), [attr] if matches!(attr.style, AttrStyle::Outer)
            && attr.path().is_ident("cfg")
            && matches!(attr.parse_args::<Meta>(), Ok(Meta::Path(path)) if path.is_ident("test")))
}

fn inspect(source: &str) -> Boundary {
    let file = syn::parse_file(source).unwrap();
    let mut names = Names::default();
    names.visit_file(&file);
    let mut boundary = Boundary {
        rejected: std::mem::take(&mut names.ambiguous),
        names,
        used: BTreeSet::new(),
        exempt: 0,
    };
    for attr in &file.attrs {
        boundary.visit_attribute(attr);
    }
    for item in &file.items {
        if test_oracle(item) {
            boundary.exempt += 1;
        } else {
            boundary.visit_item(item);
        }
    }
    if boundary.exempt > 1 {
        boundary.rejected.push("duplicate test module".into());
    }
    boundary
}

// The policy must read the file Cargo compiles: an attribute or body could redirect it.
fn declares_module(source: &str) -> bool {
    let file = syn::parse_file(source).unwrap();
    let declarations: Vec<_> = file
        .items
        .iter()
        .filter_map(|item| match item {
            Item::Mod(module) if module.ident == "security" => Some(module),
            _ => None,
        })
        .collect();
    matches!(declarations.as_slice(), [module] if module.attrs.is_empty() && module.content.is_none())
}

fn source(relative: &str) -> String {
    fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(relative)).unwrap()
}

#[test]
fn production_security_module_stays_within_its_dependency_boundary() {
    assert!(
        declares_module(&source(PARENT)),
        "{PARENT} must declare `mod security;` without attributes"
    );
    let module = source(MODULE);
    let boundary = inspect(&module);
    assert!(
        boundary.rejected.is_empty(),
        "{MODULE}: {:?}",
        boundary.rejected
    );
    assert_eq!(
        boundary.exempt, 1,
        "the whoami oracle must stay `#[cfg(test)] mod tests;`"
    );
    let stale: Vec<_> = ALLOWED
        .iter()
        .chain(MACROS)
        .filter(|entry| !boundary.used.contains(**entry))
        .collect();
    assert!(stale.is_empty(), "stale exceptions: {stale:?}");
    // The parent re-exports this function; the inspected file must be the one defining it.
    assert!(syn::parse_file(&module).unwrap().items.iter().any(
        |item| matches!(item, Item::Fn(function) if function.sig.ident == "set_private_permissions")
    ));
}

#[test]
fn the_parent_declaration_cannot_redirect_the_module() {
    assert!(declares_module("mod focus; mod security; mod tab;"));
    for source in [
        "mod focus;",
        "#[path = \"security_impl.rs\"] mod security;",
        "#[cfg_attr(windows, path = \"security_impl.rs\")] mod security;",
        "#[cfg(not(windows))] mod security;",
        "mod security { pub fn set_private_permissions() {} }",
        "mod security; #[cfg(test)] mod security;",
    ] {
        assert!(!declares_module(source), "{source}");
    }
}

#[test]
fn formatting_comments_strings_and_local_items_do_not_change_the_boundary() {
    let module = source(MODULE);
    // A token round trip drops comments and rewrites every space and line break.
    let reflowed = module
        .parse::<proc_macro2::TokenStream>()
        .unwrap()
        .to_string();
    assert_ne!(reflowed, module);
    let (original, reflowed) = (inspect(&module), inspect(&reflowed));
    assert!(reflowed.rejected.is_empty(), "{:?}", reflowed.rejected);
    assert_eq!(reflowed.used, original.used);
    assert_eq!(reflowed.exempt, 1);

    let mentions = format!(
        "{module}
// whoami.exe through helper_command or Command::new
/// Former route: crate::native::process_env::helper_command(whoami.exe).
fn documented() -> &'static str {{ \"std::process::Command::new(\\\"whoami\\\")\" }}
enum Mode {{ File }}
fn mode() -> Mode {{ let _ = format!(\"{{}}\", documented()); Mode::File }}
"
    );
    let boundary = inspect(&mentions);
    assert!(boundary.rejected.is_empty(), "{:?}", boundary.rejected);
}

// Each is appended to the real module, which alone has no rejection, so the expected
// reason must come from the mutation itself.
const MUTATIONS: &[(&str, &str)] = &[
    // Value-namespace items do not shadow crates in a multi-segment path.
    (
        r#"fn std() {} fn f() { std::process::Command::new("whoami"); }"#,
        "path std::process::Command::new",
    ),
    (
        "const windows_sys: () = (); fn f() { unsafe { windows_sys::Win32::System::Threading::CreateProcessW() }; }",
        "path windows_sys::Win32::System::Threading::CreateProcessW",
    ),
    (
        r#"static std: () = (); fn f() { std::process::Command::new("whoami"); }"#,
        "path std::process::Command::new",
    ),
    (
        r#"fn local() { struct std; } fn f() { std::process::Command::new("whoami"); }"#,
        "path std::process::Command::new",
    ),
    (
        "fn local() { enum windows_sys {} } fn f() { unsafe { windows_sys::Win32::System::Threading::CreateProcessW() }; }",
        "path windows_sys::Win32::System::Threading::CreateProcessW",
    ),
    // The route #306 removed, shortened.
    (
        r#"fn sid() -> Result<String> {
            let output = crate::native::process_env::helper_command(Path::new("whoami.exe"))
                .args(["/user", "/fo", "csv", "/nh"]).output()?;
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        }"#,
        "path crate::native::process_env::helper_command",
    ),
    (
        "use crate::native::process_env::helper_command as sid_source;",
        "import crate::native::process_env::helper_command",
    ),
    (
        r#"use crate::native::process_env as helpers;
        fn f() { helpers::helper_command(Path::new("whoami.exe")); }"#,
        "import crate::native::process_env",
    ),
    ("use super::*;", "import super::*"),
    (
        "use super::super::process_env::helper_command;",
        "import super::super::process_env",
    ),
    (
        "fn f() { super::process::query_process_identity(); }",
        "path super::process",
    ),
    ("fn f() { self::tests::whoami_sid(); }", "path self::tests"),
    ("fn f() { tests::whoami_sid(); }", "path tests::whoami_sid"),
    (
        "use std::process::Command as Whoami;",
        "import std::process::Command",
    ),
    (
        r#"fn f() { use std::process::Command; Command::new("whoami"); }"#,
        "import std::process::Command",
    ),
    (
        "use std::process::Command as OsStr;",
        "ambiguous alias OsStr",
    ),
    (
        r#"fn f() { std::process::Command::new("whoami"); }"#,
        "path std::process::Command::new",
    ),
    (
        r#"fn f() { ::std::process::Command::new("whoami"); }"#,
        "path ::std::process",
    ),
    (
        r#"fn f() { <std::process::Command>::new("whoami"); }"#,
        "path std::process::Command",
    ),
    (
        "type Sid = std::process::Command;",
        "path std::process::Command",
    ),
    (
        "use std::os::windows::process::CommandExt;",
        "import std::os::windows::process::CommandExt",
    ),
    (
        r#"fn f() { std::env::var("USERNAME"); }"#,
        "path std::env::var",
    ),
    (
        "use windows_sys::Win32::System::Threading::CreateProcessW;",
        "import windows_sys::Win32::System::Threading::CreateProcessW",
    ),
    (
        "fn f() { unsafe { windows_sys::Win32::System::Threading::CreateProcessW() }; }",
        "path windows_sys::Win32::System::Threading::CreateProcessW",
    ),
    (
        "fn f() { unsafe { windows_sys::Win32::System::LibraryLoader::GetProcAddress() }; }",
        "path windows_sys::Win32::System::LibraryLoader::GetProcAddress",
    ),
    // In-file wrappers are production items and are inspected like any other.
    (
        r#"fn whoami() { crate::native::process_env::helper_command(Path::new("whoami.exe")); }
        fn caller() { whoami() }"#,
        "path crate::native::process_env::helper_command",
    ),
    (
        r#"struct Probe;
        impl Probe {
            fn sid() { Self::run() }
            fn run() { crate::native::process_env::helper_command(Path::new("whoami.exe")); }
        }"#,
        "path crate::native::process_env::helper_command",
    ),
    (
        r#"fn f() -> String {
            format!("{:?}", crate::native::process_env::helper_command(Path::new("whoami.exe")))
        }"#,
        "path crate::native::process_env::helper_command",
    ),
    (
        "fn f() { let _ = vec![super::sid(); 1]; }",
        "path super::sid",
    ),
    (
        r#"fn f() -> anyhow::Error { anyhow::anyhow!("{:?}", std::process::Command::new("whoami")) }"#,
        "path std::process::Command::new",
    ),
    (
        r#"fn f() -> Result<()> { bail!("{:?}", std::process::Command::new("whoami")) }"#,
        "path std::process::Command::new",
    ),
    (
        r#"fn f() -> String { format!("{}" crate::native::sid()) }"#,
        "uninspectable arguments of format",
    ),
    ("fn f() { spawn_whoami!(); }", "macro spawn_whoami"),
    (r#"fn f() { include!("identity.rs"); }"#, "macro include"),
    (
        r#"fn f() { let _ = format!(concat!("who", "ami")); }"#,
        "macro concat",
    ),
    (
        r#"macro_rules! sid { () => { std::process::Command::new("whoami") }; }"#,
        "item macro macro_rules",
    ),
    ("fn f() { macro_rules! sid { () => {} } }", "macro_rules"),
    ("extern crate std as standard;", "extern crate"),
    (
        r#"extern "system" { fn CreateProcessW(); }"#,
        "extern block",
    ),
    (r#"#[link(name = "kernel32")] fn f() {}"#, "attribute link"),
    (
        "#[identity::from_whoami] fn f() {}",
        "attribute identity::from_whoami",
    ),
    (
        r#"#[doc = include_str!("identity.rs")] fn f() {}"#,
        "attribute doc",
    ),
    ("#[cfg(test)] fn helper() {}", "attribute cfg"),
    // Only the exact test-module declaration is exempt.
    ("#[cfg(any(test, windows))] mod tests;", "module tests"),
    (
        r#"#[cfg(test)] #[path = "identity.rs"] mod tests;"#,
        "module tests",
    ),
    (
        r#"#[cfg_attr(test, path = "identity.rs")] mod tests;"#,
        "module tests",
    ),
    ("#[cfg(test)] pub mod tests;", "module tests"),
    ("#[cfg(test)] mod tests { fn f() {} }", "module tests"),
    ("#[cfg(test)] mod tests;", "duplicate test module"),
    ("mod identity;", "module identity"),
];

#[test]
fn new_dependencies_and_indirections_fail_closed() {
    let module = source(MODULE);
    for (mutation, expected) in MUTATIONS {
        let rejected = inspect(&format!("{module}\n{mutation}\n")).rejected;
        assert!(
            rejected.iter().any(|reason| reason.contains(expected)),
            "{mutation}\nexpected {expected:?} in {rejected:?}"
        );
    }
}
