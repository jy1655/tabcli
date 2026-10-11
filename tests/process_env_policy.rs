//! Follow module declarations and parse items; names such as tests.rs confer no exemption.
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};
use syn::{Attribute, Item, Meta, Token, punctuated::Punctuated, visit::Visit};

fn requires_test(meta: &Meta) -> bool {
    match meta {
        Meta::Path(path) => path.is_ident("test"),
        Meta::List(list) => {
            let nested = list
                .parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)
                .unwrap();
            if list.path.is_ident("all") {
                nested.iter().any(requires_test)
            } else if list.path.is_ident("any") {
                !nested.is_empty() && nested.iter().all(requires_test)
            } else {
                false
            }
        }
        _ => false,
    }
}

fn test_only(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|attr| {
        attr.path().is_ident("cfg") && requires_test(&attr.parse_args::<Meta>().unwrap())
    })
}

fn declared_path(attrs: &[Attribute]) -> Option<PathBuf> {
    attrs.iter().find_map(|attr| {
        if attr.path().is_ident("path")
            && let Meta::NameValue(value) = &attr.meta
            && let syn::Expr::Lit(value) = &value.value
            && let syn::Lit::Str(path) = &value.lit
        {
            Some(PathBuf::from(path.value()))
        } else {
            None
        }
    })
}

fn contains_command(tokens: proc_macro2::TokenStream) -> bool {
    tokens.into_iter().any(|token| match token {
        proc_macro2::TokenTree::Ident(ident) => ident == "Command",
        proc_macro2::TokenTree::Group(group) => contains_command(group.stream()),
        _ => false,
    })
}

#[derive(Default)]
struct Constructors {
    function: String,
    module_directory: PathBuf,
    path_directory: PathBuf,
    external_modules: Vec<PathBuf>,
    found: Vec<String>,
    provider_constructors: Vec<String>,
    rejected: Vec<&'static str>,
    in_type_alias: bool,
}
impl<'ast> Visit<'ast> for Constructors {
    fn visit_file(&mut self, file: &'ast syn::File) {
        if !test_only(&file.attrs) {
            syn::visit::visit_file(self, file);
        }
    }
    fn visit_item(&mut self, item: &'ast Item) {
        let attrs = match item {
            Item::Mod(item) => &item.attrs,
            Item::Fn(item) => &item.attrs,
            Item::Impl(item) => &item.attrs,
            Item::Const(item) => &item.attrs,
            Item::Static(item) => &item.attrs,
            Item::Use(item) => &item.attrs,
            Item::Type(item) => &item.attrs,
            Item::Macro(item) => &item.attrs,
            _ => return syn::visit::visit_item(self, item),
        };
        if !test_only(attrs) {
            syn::visit::visit_item(self, item);
        }
    }
    fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
        let explicit = declared_path(&item.attrs).map(|path| self.path_directory.join(path));
        if let Some((_, items)) = &item.content {
            let directory =
                explicit.unwrap_or_else(|| self.module_directory.join(item.ident.to_string()));
            let old_module = std::mem::replace(&mut self.module_directory, directory.clone());
            let old_path = std::mem::replace(&mut self.path_directory, directory);
            for item in items {
                self.visit_item(item);
            }
            self.module_directory = old_module;
            self.path_directory = old_path;
        } else {
            let file = explicit.unwrap_or_else(|| {
                let file = self.module_directory.join(format!("{}.rs", item.ident));
                if file.is_file() {
                    file
                } else {
                    self.module_directory
                        .join(item.ident.to_string())
                        .join("mod.rs")
                }
            });
            self.external_modules.push(file);
        }
    }
    fn visit_item_fn(&mut self, item: &'ast syn::ItemFn) {
        let old = std::mem::replace(&mut self.function, item.sig.ident.to_string());
        syn::visit::visit_item_fn(self, item);
        self.function = old;
    }
    fn visit_impl_item_fn(&mut self, item: &'ast syn::ImplItemFn) {
        if test_only(&item.attrs) {
            return;
        }
        let old = std::mem::replace(&mut self.function, item.sig.ident.to_string());
        syn::visit::visit_impl_item_fn(self, item);
        self.function = old;
    }
    fn visit_use_rename(&mut self, rename: &'ast syn::UseRename) {
        if rename.ident == "Command" {
            self.rejected.push("renamed Command import");
        }
        syn::visit::visit_use_rename(self, rename);
    }
    fn visit_item_type(&mut self, item: &'ast syn::ItemType) {
        let old = std::mem::replace(&mut self.in_type_alias, true);
        syn::visit::visit_item_type(self, item);
        self.in_type_alias = old;
    }
    fn visit_path(&mut self, path: &'ast syn::Path) {
        let parts: Vec<_> = path
            .segments
            .iter()
            .map(|part| part.ident.to_string())
            .collect();
        if self.in_type_alias && parts.iter().any(|part| part == "Command") {
            self.rejected.push("Command type alias");
        }
        // Also detects a constructor used as a function value, without a call expression.
        if parts.ends_with(&["Command".into(), "new".into()]) {
            self.found.push(self.function.clone());
        }
        if parts.windows(2).any(|pair| {
            pair[0] == "provider_process"
                && matches!(pair[1].as_str(), "command" | "version_command")
        }) {
            self.provider_constructors.push(self.function.clone());
        }
        syn::visit::visit_path(self, path);
    }
    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        // Macro grammars are arbitrary. Fail closed on Command tokens, including nested
        // vec!/format! arguments, rather than silently skipping an unparsed constructor.
        // String literals and comments are not identifiers and do not trigger this rule.
        if contains_command(mac.tokens.clone()) {
            self.rejected.push("Command in macro tokens");
        }
    }
}

fn inspect(source: &str) -> Constructors {
    let mut visitor = Constructors::default();
    visitor.visit_file(&syn::parse_file(source).unwrap());
    visitor
}

fn production_modules(entries: &[PathBuf]) -> Vec<(PathBuf, Constructors)> {
    let mut pending = entries.to_vec();
    let mut seen = BTreeSet::new();
    let mut result = Vec::new();
    while let Some(file) = pending.pop() {
        if !seen.insert(file.clone()) {
            continue;
        }
        let parent = file.parent().unwrap();
        let module_directory = if matches!(
            file.file_name().unwrap().to_str().unwrap(),
            "main.rs" | "lib.rs" | "mod.rs"
        ) {
            parent.to_owned()
        } else {
            parent.join(file.file_stem().unwrap())
        };
        let mut visitor = Constructors {
            module_directory,
            path_directory: parent.to_owned(),
            ..Default::default()
        };
        visitor.visit_file(&syn::parse_file(&fs::read_to_string(&file).unwrap()).unwrap());
        pending.extend(visitor.external_modules.iter().cloned());
        result.push((file, visitor));
    }
    result
}

#[test]
fn test_items_do_not_hide_production_constructors() {
    assert_eq!(
        inspect(
            r#"
        #[cfg(test)] fn test_only() { Command::new("ignored"); }
        fn production_after_test() { std::process::Command::new("found"); }
        #[cfg(all(target_os = "macos", test))]
        mod tests { fn test() { Command::new("ignored"); } }
        mod mixed {
            #[cfg(test)] fn test() { Command::new("ignored"); }
            fn production() { Command::new("found"); }
        }
        #[cfg(any(test, windows))] fn also_production() { Command::new("found"); }
    "#
        )
        .found,
        ["production_after_test", "production", "also_production"]
    );
}

#[test]
fn aliases_and_macro_constructors_cannot_bypass_the_policy() {
    for source in [
        r#"use std::process::Command as X; fn f() { X::new("bad"); }"#,
        r#"type X = std::process::Command; fn f() { X::new("bad"); }"#,
        r#"use std::process::Command; type X = Command; fn f() { X::new("bad"); }"#,
        r#"fn f() { vec![Command::new("bad")]; }"#,
        r#"fn f() { format!("{:?}", std::process::Command::new("bad")); }"#,
        r#"fn f() { unknown!(nested([Command::new("bad")])); }"#,
    ] {
        assert!(!inspect(source).rejected.is_empty(), "{source}");
    }
    assert!(
        inspect(r#"fn f() { format!("Command::new is forbidden"); }"#)
            .rejected
            .is_empty()
    );
    assert!(inspect(r#"#[cfg(test)] mod tests { use std::process::Command as X; fn f() { vec![X::new("ok")]; } }"#).rejected.is_empty());
}

#[test]
fn constructor_function_values_cannot_bypass_the_policy() {
    for source in [
        "fn f() { let build = Command::new; }",
        "fn f() { accept(std::process::Command::new); }",
    ] {
        assert_eq!(inspect(source).found, ["f"], "{source}");
    }
}

#[test]
fn module_declarations_determine_test_exclusions_not_file_names() {
    let root = tempfile::tempdir().unwrap();
    let main = root.path().join("main.rs");
    let tests = root.path().join("tests.rs");
    fs::write(&tests, "fn hidden() { Command::new(\"bad\"); }").unwrap();
    for declaration in ["mod tests;", "#[cfg(any(test, windows))] mod tests;"] {
        fs::write(&main, declaration).unwrap();
        let modules = production_modules(std::slice::from_ref(&main));
        assert!(
            modules
                .iter()
                .any(|(file, visitor)| file == &tests && visitor.found == ["hidden"])
        );
    }
    for declaration in [
        "#[cfg(test)] mod tests;",
        "#[cfg(test)] #[path = \"tests.rs\"] mod renamed;",
        "#[cfg(test)] mod inline { #[path = \"../tests.rs\"] mod nested; }",
    ] {
        fs::write(&main, declaration).unwrap();
        assert_eq!(production_modules(std::slice::from_ref(&main)).len(), 1);
    }
}

#[test]
fn production_commands_use_the_environment_policy() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut found = Vec::new();
    for (file, visitor) in production_modules(&[root.join("main.rs"), root.join("lib.rs")]) {
        let relative = file
            .strip_prefix(&root)
            .unwrap()
            .to_str()
            .unwrap()
            .replace('\\', "/");
        assert!(
            visitor.rejected.is_empty(),
            "uninspectable Command construction in {relative}: {:?}",
            visitor.rejected
        );
        assert!(
            visitor.provider_constructors.is_empty() || relative == "native/process_env.rs",
            "provider construction bypasses process_env in {relative}"
        );
        found.extend(
            visitor
                .found
                .into_iter()
                .map(|function| (relative.clone(), function)),
        );
    }
    // provider_process is construction mechanics behind process_env's provider path,
    // never a helper: its commands must retain the chosen adapter's removals only.
    let allowed = [
        ("native/process_env.rs", "helper_command"),
        ("native/provider_process.rs", "command"),
        ("native/provider_process.rs", "version_command"),
        // #111: leave the Windows provider PowerShell constructor byte-for-byte intact.
        ("native/provider_process.rs", "powershell_file_command"),
        // #111: Windows console control retains its current environment.
        ("native/terminal/windows/mod.rs", "console_helper_command"),
        // #111: Windows security helper retains its current environment.
        (
            "native/terminal/windows/security.rs",
            "set_private_permissions",
        ),
    ];
    for (file, function) in &found {
        assert!(
            allowed.contains(&(file.as_str(), function.as_str())),
            "unrouted Command::new in {file}::{function}"
        );
    }
    for (file, function) in allowed {
        assert!(
            found.contains(&(file.into(), function.into())),
            "stale exception {file}::{function}"
        );
    }
}
