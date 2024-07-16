use super::Sources;
use crate::impl_map_arrow_fmt;
use itertools::Itertools;
use lumina_key as key;
use lumina_key::{Map, M};
use lumina_util::{Highlighting, Span};
use std::collections::HashMap;
use std::fmt;
use tracing::{error, trace};

#[derive(Clone, Copy, Debug)]
pub struct Mod<K> {
    pub visibility: Visibility,
    pub module: key::Module,
    pub key: K,
}

impl<K> Mod<K> {
    pub fn map<O>(self, f: impl FnOnce(K) -> O) -> Mod<O> {
        Mod {
            key: f(self.key),
            module: self.module,
            visibility: self.visibility,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Visibility {
    Project(key::Module),
    Public,
}

impl Visibility {
    pub fn from_public_flag(module: key::Module, public: bool) -> Self {
        if public {
            Visibility::Public
        } else {
            Visibility::Project(module)
        }
    }
}

pub struct Lookups<'s> {
    modules: Map<key::Module, Namespaces<'s>>,
    pub project: key::Module,
    libs: HashMap<&'static str, HashMap<String, key::Module>>,
}

impl<'s> Lookups<'s> {
    pub fn new() -> Lookups<'s> {
        let modules = Map::new();

        let mut libs = HashMap::new();
        libs.insert("std", HashMap::new());
        libs.insert("ext", HashMap::new());
        libs.insert("prelude", HashMap::new());

        Lookups { libs, modules, project: key::Module(u32::MAX) }
    }

    pub fn to_field_lookup(&self) -> Map<key::Module, HashMap<&'s str, Vec<M<key::Record>>>> {
        self.modules
            .values()
            .map(|namespace| {
                namespace
                    .accessors
                    .iter()
                    .map(|(name, pos)| (*name, pos.iter().map(|r| r.module.m(r.key.0)).collect()))
                    .collect()
            })
            .collect()
    }

    pub fn new_root_module(&mut self, parent: Option<key::Module>) -> key::Module {
        let mut namespaces = Namespaces::default();
        namespaces.kind = ModuleKind::Root { parent };
        self.modules.push(namespaces)
    }

    pub fn new_member_module(&mut self, root: key::Module) -> key::Module {
        let mut namespaces = Namespaces::default();
        namespaces.kind = ModuleKind::Member { root };
        self.modules.push(namespaces)
    }

    pub fn new_lib(&mut self, in_: &'static str, lib: String) -> key::Module {
        let module = self.modules.push(Namespaces::default());
        self.libs.get_mut(in_).unwrap().insert(lib, module);
        module
    }

    pub fn declare<T: EntityT>(
        &mut self,
        module: key::Module,
        visibility: Visibility,
        name: &'s str,
        dstmodule: key::Module,
        entity: T,
    ) -> Option<Mod<T>> {
        let m = Mod { visibility, module: dstmodule, key: entity };
        T::insert(m, name, &mut self.modules[module])
    }

    pub fn declare_accessor(
        &mut self,
        module: key::Module,
        visibility: Visibility,
        name: &'s str,
        type_: key::Record,
        field: key::RecordField,
    ) {
        let m = Mod { visibility, module, key: (type_, field) };
        self.modules[module]
            .accessors
            .entry(name)
            .or_default()
            .push(m);
    }

    pub fn declare_module_link(
        &mut self,
        module: key::Module,
        visibility: Visibility,
        name: String,
        dst: key::Module,
    ) {
        let m = Mod { module, visibility, key: dst };
        self.modules[module].child_modules.insert(name, m);
    }

    /// Resolve an entity and prioritise the function namespace
    pub fn resolve_func(
        &self,
        from: key::Module,
        path: &[&'s str],
    ) -> Result<Mod<Entity<'s>>, ImportError<'s>> {
        self.resolve(from, Namespace::Functions, path, false)
    }
    /// Resolve an entity and prioritise the type namespace
    pub fn resolve_type(
        &self,
        from: key::Module,
        path: &[&'s str],
    ) -> Result<Mod<Entity<'s>>, ImportError<'s>> {
        self.resolve(from, Namespace::Types, path, false)
    }
    /// Resolve an entity and prioritise the module/imports namespace
    pub fn resolve_module(
        &self,
        from: key::Module,
        path: &[&'s str],
    ) -> Result<Mod<Entity<'s>>, ImportError<'s>> {
        self.resolve(from, Namespace::Modules, path, false)
    }
    pub fn resolve_import(&self, from: key::Module, name: &'s str) -> Option<Mod<key::Module>> {
        self.modules[from]
            .child_modules
            .get(name)
            .copied()
            .or_else(|| match self.modules[from].kind {
                ModuleKind::Member { root } => self.resolve_import(root, name),
                _ => None,
            })
    }

    pub fn resolve_langitem(&self, names: &[&'s str]) -> Result<Mod<Entity<'s>>, ImportError<'s>> {
        self.resolve(self.project, Namespace::Functions, names, true)
    }

    fn resolve(
        &self,
        from: key::Module,
        namespace: Namespace,
        mut path: &[&'s str],
        mut ignore_vis: bool,
    ) -> Result<Mod<Entity<'s>>, ImportError<'s>> {
        trace!(
            "attempting to resolve {} from {from} in namespace {namespace:?}",
            path.iter().format(":")
        );

        // If we access prelude via absolute path, we want to ignore namespace rules
        // This is mainly so that standard library modules can directly call private langitems
        if Some(["std", "prelude"].as_slice()) == path.get(..2) {
            ignore_vis = true;
        }

        let start_at = match self.libs.get(path[0]) {
            Some(libs) => {
                let lname = path.get(1).copied().unwrap_or("");
                path = &path[2..];

                match libs.get(lname) {
                    None => return Err(ImportError::LibNotInstalled(lname)),
                    Some(module) => *module,
                }
            }
            None if path[0] == "project" => {
                path = &path[1..];
                self.project
            }
            None => from,
        };

        self.resolve_in(from, namespace, start_at, path, ignore_vis)
            .or_else(|err| match err {
                ImportError::BadAccess(..) | ImportError::LibNotInstalled(..) => Err(err),
                err => self
                    .resolve_in(from, namespace, key::PRELUDE, path, ignore_vis)
                    .map_err(|_| err),
            })
    }

    pub fn resolve_entity_in<'a>(
        &self,
        origin: key::Module,
        module: key::Module,
        name: &'a str,
        igv: bool,
    ) -> Result<Mod<Entity<'a>>, ImportError<'a>> {
        self.resolve_in(origin, Namespace::Types, module, &[name], igv)
    }

    fn resolve_in<'a>(
        &self,
        origin: key::Module,
        namespace: Namespace,
        module: key::Module,
        path: &[&'a str],
        ignore_vis: bool,
    ) -> Result<Mod<Entity<'a>>, ImportError<'a>> {
        let entity = match path {
            [] => Ok(Mod {
                key: Entity::Module(module),
                module,
                visibility: Visibility::Public,
            }),
            [x] => match namespace {
                Namespace::Modules => self
                    .resolve_import(module, *x)
                    .map(|m| m.map(Entity::Module))
                    .or_else(|| self.modules[module].try_namespace(namespace, *x))
                    .ok_or(ImportError::NotFound(module, *x)),
                _ => self.modules[module]
                    .try_namespace(namespace, *x)
                    .ok_or(ImportError::NotFound(module, *x)),
            },
            [x, xs @ ..] => {
                match self.resolve_import(module, x) {
                    Some(m) => {
                        if !self.is_valid_reachability(origin, m.visibility) && !ignore_vis {
                            return Err(ImportError::BadAccess(m.visibility, "module", x));
                        }

                        self.resolve_in(origin, namespace, m.key, xs, ignore_vis)
                    }

                    // no module of this name found, but it could still be a type/trait
                    None if xs.len() == 1 => {
                        match self.modules[module].try_namespace(Namespace::Types, *x) {
                            None => Err(ImportError::ModNotFound(module, *x)),
                            Some(entity) => match entity.key {
                                Entity::Type(type_) => {
                                    Ok(entity.map(|_| Entity::Member(type_, xs[0])))
                                }
                                _ => Err(ImportError::ModNotFound(module, *x)),
                            },
                        }
                    }

                    None => Err(ImportError::ModNotFound(module, *x)),
                }
            }
        }?;

        if !self.is_valid_reachability(origin, entity.visibility) && !ignore_vis {
            return Err(ImportError::BadAccess(
                entity.visibility,
                entity.key.describe(),
                path.last().unwrap(),
            ));
        }

        Ok(entity)
    }

    pub fn resolve_accessor(
        &self,
        module: key::Module,
        name: &'s str,
    ) -> &[Mod<(key::Record, key::RecordField)>] {
        self.modules[module]
            .accessors
            .get(name)
            .map(Vec::as_ref)
            .unwrap_or(&[])
    }

    pub fn is_valid_reachability<'a>(&self, current: key::Module, visibility: Visibility) -> bool {
        match visibility {
            Visibility::Project(m) if self.are_members_of_same_project(&[current, m]) => true,
            Visibility::Public => true,
            _ => false,
        }
    }

    fn are_members_of_same_project(&self, modules: &[key::Module]) -> bool {
        let mut ms = modules.iter().copied().map(|of| self.get_root_module(of));
        let m = ms.next().unwrap();
        ms.all(|m_| m_ == m)
    }

    fn get_root_module(&self, of: key::Module) -> key::Module {
        match self.modules[of].kind {
            ModuleKind::Root { parent } => match parent {
                Some(p) => self.get_root_module(p),
                None => of,
            },
            ModuleKind::Member { root } => root,
        }
    }

    /// Standard libraries do not have to be declared as dependencies but are still lazily included
    /// when imported. This checks whether the given path is an un-included standard library
    pub fn lib_should_be_included<'a, 'b>(
        &self,
        path: &'a [&'b str],
    ) -> Option<(&'b str, &'a [&'b str])> {
        (path[0] == "std")
            .then(|| {
                self.libs
                    .get("std")
                    .expect("compiler is running without standard library")
                    .get(path[1])
                    .is_none()
                    .then_some((path[1], &path[2..]))
            })
            .flatten()
    }
}

#[derive(Debug, Clone, Copy)]
enum Namespace {
    Functions,
    Types,
    Modules,
}

#[derive(Debug)]
pub enum Entity<'s> {
    Module(key::Module),
    Func(NFunc),
    Type(key::TypeKind),
    Member(key::TypeKind, &'s str),
    // Method((key::Type, key::Method)),
    // since these can be multiple, we can't really handle it in normal resolve
    // Accessors((key::Type, key::RecordField)),
}

impl<'s> Entity<'s> {
    pub fn describe(&self) -> &'static str {
        match self {
            Entity::Module(_) => "module",
            Entity::Func(_) => "function",
            Entity::Type(_) => "type",
            Entity::Member(..) => "member",
        }
    }
}

#[derive(Debug)]
pub enum ImportError<'s> {
    BadAccess(Visibility, &'static str, &'s str),
    LibNotInstalled(&'s str),
    NotFound(key::Module, &'s str),
    ModNotFound(key::Module, &'s str),
}

pub trait EntityT: Sized {
    fn insert<'s>(
        m: Mod<Self>,
        name: &'s str,
        namespaces: &mut Namespaces<'s>,
    ) -> Option<Mod<Self>>;
}

macro_rules! impl_entityt {
    ($t:ty, $field:ident) => {
        impl EntityT for $t {
            fn insert<'s>(
                m: Mod<Self>,
                name: &'s str,
                namespaces: &mut Namespaces<'s>,
            ) -> Option<Mod<Self>> {
                namespaces.$field.insert(name, m)
            }
        }
    };
}

impl_entityt!(NFunc, funcs);
impl_entityt!(key::TypeKind, types);

#[derive(Default, Debug)]
pub struct Namespaces<'s> {
    funcs: HashMap<&'s str, Mod<NFunc>>,
    types: HashMap<&'s str, Mod<key::TypeKind>>,

    child_modules: HashMap<String, Mod<key::Module>>,

    kind: ModuleKind,

    accessors: HashMap<&'s str, Vec<Mod<(key::Record, key::RecordField)>>>,
}

#[derive(Debug)]
pub enum ModuleKind {
    Root { parent: Option<key::Module> },
    Member { root: key::Module },
}

impl Default for ModuleKind {
    fn default() -> Self {
        ModuleKind::Root { parent: None }
    }
}

impl<'s> Namespaces<'s> {
    fn try_namespace<'a>(&self, namespace: Namespace, name: &'a str) -> Option<Mod<Entity<'a>>> {
        match namespace {
            Namespace::Functions => self
                .try_function_namespace(name)
                .or_else(|| self.try_type_namespace(name))
                .or_else(|| self.try_child_imports(name)),
            Namespace::Types => self
                .try_type_namespace(name)
                .or_else(|| self.try_function_namespace(name))
                .or_else(|| self.try_child_imports(name)),
            Namespace::Modules => self
                .try_child_imports(name)
                .or_else(|| self.try_function_namespace(name))
                .or_else(|| self.try_type_namespace(name)),
        }
    }

    fn try_function_namespace<'a>(&self, name: &'a str) -> Option<Mod<Entity<'a>>> {
        self.funcs.get(name).copied().map(|m| m.map(Entity::Func))
    }

    fn try_child_imports<'a>(&self, name: &'a str) -> Option<Mod<Entity<'a>>> {
        self.child_modules
            .get(name)
            .copied()
            .map(|m| m.map(Entity::Module))
    }

    fn try_type_namespace<'a>(&self, name: &'a str) -> Option<Mod<Entity<'a>>> {
        self.types.get(name).copied().map(|m| m.map(Entity::Type))
    }
}

/// Pointer to something in the function namespace
#[derive(Clone, Copy, Debug)]
pub enum NFunc {
    Key(key::Func),
    Method(key::Trait, key::Method),
    SumVar(key::Sum, key::SumVariant),
    Val(key::Val),
}

impl Sources {
    pub fn emit_lookup_err(&self, span: Span, module: key::Module, kind: &str, err: ImportError) {
        match err {
            ImportError::LibNotInstalled(str) => self
                .error("library not found")
                .m(module)
                .eline(span, format!("no library named {str} is installed"))
                .emit(),
            ImportError::NotFound(_, name) => self
                .error("identifier not found")
                .m(module)
                .eline(span, format!("no {kind} named {name}"))
                .emit(),
            ImportError::ModNotFound(m, name) => self
                .error("module not found")
                .m(module)
                .eline(
                    span,
                    format!("`{}` has no module named `{name}`", self.name_of_module(m)),
                )
                .emit(),
            ImportError::BadAccess(_vis, k, name) if k == "module" => self
                .error("module not found")
                .m(module)
                .eline(
                    span,
                    format!("there is a module named {name} but it's not public"),
                )
                .emit(),
            ImportError::BadAccess(_vis, k, name) => self
                .error("identifier not found")
                .m(module)
                .eline(span, "")
                .text(format!("there is a {k} named {name} but it's not public"))
                .emit(),
        }
    }
}

impl_map_arrow_fmt!(<'s> std::fmt::Debug; for Lookups<'s>;  ("modules", modules, |(k, v)| format!("{k} → {v:#?}")));

impl fmt::Display for NFunc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NFunc::Key(key) => key.fmt(f),
            NFunc::Method(trait_, method) => write!(f, "{trait_}{}{method}", ':'.symbol()),
            NFunc::SumVar(sum, var) => write!(f, "{sum}{}{var}", ':'.symbol()),
            NFunc::Val(ro) => ro.fmt(f),
        }
    }
}

impl<K: fmt::Display> fmt::Display for Mod<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}({}) {}", "pub".keyword(), self.visibility, self.key)
    }
}

impl fmt::Display for Visibility {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Visibility::Project(m) => write!(f, "project_of({m})"),
            Visibility::Public => "public".fmt(f),
        }
    }
}
