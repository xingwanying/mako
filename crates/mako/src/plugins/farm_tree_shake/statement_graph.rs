use std::collections::{HashMap, HashSet, VecDeque};

use mako_core::petgraph;
use mako_core::petgraph::stable_graph::NodeIndex;
use mako_core::swc_ecma_ast::{Module as SwcModule, ModuleItem};

pub(crate) mod analyze_imports_and_exports;
pub(crate) mod defined_idents_collector;
pub(crate) mod used_idents_collector;

use analyze_imports_and_exports::analyze_imports_and_exports;
use mako_core::swc_common::{Span, SyntaxContext};

use crate::plugins::farm_tree_shake::module::{is_ident_equal, is_ident_sym_equal, UsedIdent};
use crate::plugins::farm_tree_shake::shake::{strip_context, ReExportSource, ReExportType};
use crate::plugins::farm_tree_shake::statement_graph::analyze_imports_and_exports::StatementInfo;

pub type StatementId = usize;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportSpecifierInfo {
    Namespace(String),
    Named {
        local: String,
        imported: Option<String>,
    },
    Default(String),
}

#[derive(Debug, Clone)]
pub struct ImportInfo {
    pub source: String,
    pub specifiers: Vec<ImportSpecifierInfo>,
    pub stmt_id: StatementId,
}

impl ImportInfo {
    pub fn find_define_specifier(&self, ident: &String) -> Option<&ImportSpecifierInfo> {
        for specifier in self.specifiers.iter() {
            match specifier {
                ImportSpecifierInfo::Namespace(_n) => {
                    return Some(specifier);
                }
                ImportSpecifierInfo::Named { local, imported: _ } => {
                    if is_ident_equal(ident, local) {
                        return Some(specifier);
                    }
                }
                ImportSpecifierInfo::Default(local_name) => {
                    if ident == local_name {
                        return Some(specifier);
                    }
                }
            }
        }

        None
    }
}

// collect all exports and gathering them into a simpler structure
#[derive(Debug, Clone)]
pub enum ExportSpecifierInfo {
    // export * from 'foo';
    All(Vec<String>),
    // export { foo, bar, default as zoo } from 'foo';
    Named {
        local: String,
        exported: Option<String>,
    },
    // export default xxx;
    Default(Option<String>),
    // export * as foo from 'foo';
    Namespace(String),
    Ambiguous(Vec<String>),
}

impl ExportSpecifierInfo {
    pub fn to_idents(&self) -> Vec<String> {
        match self {
            ExportSpecifierInfo::All(_what) => {
                vec![]
            }
            ExportSpecifierInfo::Named { local, exported } => {
                if let Some(exp) = exported {
                    vec![strip_context(exp)]
                } else {
                    vec![strip_context(local)]
                }
            }
            ExportSpecifierInfo::Default(_) => {
                vec!["default".to_string()]
            }
            ExportSpecifierInfo::Namespace(ns) => {
                vec![strip_context(ns)]
            }
            ExportSpecifierInfo::Ambiguous(_) => {
                vec![]
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct ExportInfo {
    pub source: Option<String>,
    pub specifiers: Vec<ExportSpecifierInfo>,
    pub stmt_id: StatementId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExportInfoMatch {
    Matched,
    Unmatched,
    Ambiguous,
}

impl ExportInfo {
    pub fn find_define_specifier(&self, ident: &String) -> Option<&ExportSpecifierInfo> {
        for specifier in self.specifiers.iter() {
            match specifier {
                ExportSpecifierInfo::Default(_) => {
                    if ident == "default" {
                        return Some(specifier);
                    }
                }
                ExportSpecifierInfo::Named { local, exported } => {
                    let exported_ident = if let Some(exported) = exported {
                        exported
                    } else {
                        local
                    };

                    if is_ident_equal(ident, exported_ident) {
                        return Some(specifier);
                    }
                }
                ExportSpecifierInfo::Namespace(ns) => {
                    if is_ident_equal(ident, ns) {
                        return Some(specifier);
                    }
                }
                ExportSpecifierInfo::All(exported_idents) => {
                    let found = exported_idents.iter().find(|i| is_ident_equal(ident, i));

                    if found.is_some() {
                        return Some(specifier);
                    }
                }
                ExportSpecifierInfo::Ambiguous(idents) => {
                    if idents.iter().any(|i| is_ident_equal(ident, i)) {
                        return Some(specifier);
                    }

                    return None;
                }
            }
        }

        None
    }

    pub fn matches_ident(&self, ident: &String) -> ExportInfoMatch {
        let mut res = ExportInfoMatch::Unmatched;

        for specifier in self.specifiers.iter() {
            match specifier {
                ExportSpecifierInfo::Default(_) => {
                    if ident == "default" {
                        return ExportInfoMatch::Matched;
                    }
                }
                ExportSpecifierInfo::Named { local, exported } => {
                    let exported_ident = if let Some(exported) = exported {
                        exported
                    } else {
                        local
                    };

                    if is_ident_equal(ident, exported_ident) {
                        return ExportInfoMatch::Matched;
                    }
                }
                ExportSpecifierInfo::Namespace(ns) => {
                    if is_ident_equal(ident, ns) {
                        return ExportInfoMatch::Matched;
                    }
                }
                ExportSpecifierInfo::All(exported_idents) => {
                    let found = exported_idents.iter().find(|i| is_ident_equal(ident, i));

                    if found.is_some() {
                        return ExportInfoMatch::Matched;
                    }
                }
                ExportSpecifierInfo::Ambiguous(idents) => {
                    if idents.iter().any(|i| is_ident_equal(ident, i)) {
                        return ExportInfoMatch::Matched;
                    }

                    res = ExportInfoMatch::Ambiguous;
                }
            }
        }

        res
    }
}

#[derive(Debug)]
pub struct Statement {
    pub id: StatementId,
    pub import_info: Option<ImportInfo>,
    pub export_info: Option<ExportInfo>,
    pub defined_idents: HashSet<String>,
    pub used_idents: HashSet<String>,
    /// Use String to replace Ident as key, because Ident has position info and it will make hash map not work as expected,
    /// transform it to Ident.to_string() is exactly what we want
    pub defined_idents_map: HashMap<String, HashSet<String>>,
    pub is_self_executed: bool,
    pub has_side_effects: bool,
    pub span: Span,
}

impl Statement {
    pub(crate) fn to_re_export_type(&self, ref_ident: &String) -> Option<ReExportSource> {
        if let Some(export_info) = &self.export_info {
            for x in export_info.specifiers.iter() {
                match x {
                    // export * from 'foo';
                    ExportSpecifierInfo::All(_) => {
                        todo!()
                    }

                    // export { foo } from "foo"
                    // export { foo as bar } from "foo"
                    ExportSpecifierInfo::Named { local, exported } => {
                        if let Some(exported) = exported {
                            if is_ident_equal(ref_ident, exported) {
                                return Some(ReExportSource {
                                    re_export_type: ReExportType::Named(
                                        strip_context(local),
                                        Some(strip_context(exported)),
                                    ),
                                    source: export_info.source.clone(),
                                });
                            }
                        } else if is_ident_equal(ref_ident, local) {
                            return Some(ReExportSource {
                                re_export_type: ReExportType::Named(strip_context(local), None),
                                source: export_info.source.clone(),
                            });
                        }
                    }

                    // export foo from "foo"
                    // export default from "foo"
                    ExportSpecifierInfo::Default(_) => {
                        todo!()
                    }

                    // export * as foo from 'foo';
                    ExportSpecifierInfo::Namespace(_) => {}
                    ExportSpecifierInfo::Ambiguous(_) => {
                        return None;
                    }
                }
            }
        }

        if let Some(import_info) = &self.import_info {
            for import_specifier in import_info.specifiers.iter() {
                match import_specifier {
                    // import * as foo from 'foo';
                    ImportSpecifierInfo::Namespace(_name) => {
                        todo!()
                    }

                    // import { foo } from "foo"
                    // import { foo as bar } from "foo"
                    ImportSpecifierInfo::Named { local, imported } => {
                        if is_ident_sym_equal(local, ref_ident) {
                            return Some(ReExportSource {
                                re_export_type: ReExportType::Named(
                                    strip_context(local),
                                    imported.as_ref().map(|i| strip_context(i)),
                                ),
                                source: Some(import_info.source.clone()),
                            });
                        }
                    }

                    // import foo from "foo"
                    ImportSpecifierInfo::Default(_) => {
                        todo!()
                    }
                }
            }
        }

        None
    }
}

impl Statement {
    pub fn new(id: StatementId, stmt: &ModuleItem, unresolved_ctxt: SyntaxContext) -> Self {
        let StatementInfo {
            import_info,
            export_info,
            defined_idents,
            used_idents,
            defined_idents_map,
            is_self_executed,
            span,
            has_side_effects,
        } = analyze_imports_and_exports(&id, stmt, None, unresolved_ctxt);

        // transform defined_idents_map from HashMap<Ident, Vec<Ident>> to HashMap<String, Ident> using ToString
        let defined_idents_map = defined_idents_map
            .into_iter()
            .map(|(key, value)| (key, value))
            .collect();

        Self {
            id,
            import_info,
            export_info,
            defined_idents,
            used_idents,
            defined_idents_map,
            is_self_executed,
            has_side_effects,
            span,
        }
    }
}

pub struct StatementGraphEdge {
    pub idents: HashSet<String>,
}

pub struct StatementGraph {
    g: petgraph::graph::Graph<Statement, StatementGraphEdge>,
    id_index_map: HashMap<StatementId, NodeIndex>,
}

impl StatementGraph {
    pub fn new(module: &SwcModule, unresolved_ctxt: SyntaxContext) -> Self {
        let mut g = petgraph::graph::Graph::new();
        let mut id_index_map = HashMap::new();

        for (index, stmt) in module.body.iter().enumerate() {
            let statement = Statement::new(index, stmt, unresolved_ctxt);

            let node = g.add_node(statement);
            id_index_map.insert(index, node);
        }

        let mut graph = Self { g, id_index_map };
        let mut edges_to_add = Vec::new();

        for stmt in graph.stmts() {
            // find the statement that defines the ident
            for def_stmt in graph.stmts() {
                let mut deps_idents = HashSet::new();

                for di in &def_stmt.defined_idents {
                    if stmt.used_idents.contains(di) {
                        deps_idents.insert(di.clone());
                    }
                }

                if !deps_idents.is_empty() {
                    edges_to_add.push((stmt.id, def_stmt.id, deps_idents));
                }
            }
        }

        for (from, to, idents) in edges_to_add {
            graph.add_edge(from, to, idents);
        }

        graph
    }

    pub fn empty() -> Self {
        Self {
            g: petgraph::graph::Graph::new(),
            id_index_map: HashMap::new(),
        }
    }

    pub fn add_edge(&mut self, from: StatementId, to: StatementId, idents: HashSet<String>) {
        let from_node = self.id_index_map.get(&from).unwrap();
        let to_node = self.id_index_map.get(&to).unwrap();

        // if self.g contains edge, insert idents into edge
        if let Some(edge) = self.g.find_edge(*from_node, *to_node) {
            let edge = self.g.edge_weight_mut(edge).unwrap();

            edge.idents.extend(idents);
            return;
        }

        self.g
            .add_edge(*from_node, *to_node, StatementGraphEdge { idents });
    }

    pub fn stmt(&self, id: &StatementId) -> &Statement {
        let node = self.id_index_map.get(id).unwrap();
        &self.g[*node]
    }

    #[allow(dead_code)]
    pub fn stmt_mut(&mut self, id: &StatementId) -> &mut Statement {
        let node = self.id_index_map.get(id).unwrap();
        &mut self.g[*node]
    }

    pub fn dependencies(&self, id: &StatementId) -> Vec<(&Statement, HashSet<String>)> {
        let node = self.id_index_map.get(id).unwrap();
        self.g
            .neighbors(*node)
            .map(|i| {
                let edge = self.g.find_edge(*node, i).unwrap();
                let edge = self.g.edge_weight(edge).unwrap();
                (&self.g[i], edge.idents.clone())
            })
            .collect()
    }

    pub fn stmts(&self) -> Vec<&Statement> {
        self.g.node_indices().map(|i| &self.g[i]).collect()
    }

    #[allow(dead_code)]
    pub fn edges(&self) -> Vec<(&Statement, &Statement, &StatementGraphEdge)> {
        self.g
            .edge_indices()
            .map(|i| {
                let (from, to) = self.g.edge_endpoints(i).unwrap();
                let edge = self.g.edge_weight(i).unwrap();
                (&self.g[from], &self.g[to], edge)
            })
            .collect()
    }

    pub fn analyze_used_statements_and_idents(
        &self,
        used_exports: HashMap<StatementId, HashSet<UsedIdent>>,
    ) -> HashMap<StatementId, HashSet<String>> {
        let mut used_statements: HashMap<usize, HashSet<String>> = HashMap::new();

        // sort used_exports by statement id
        let mut used_exports: Vec<_> = used_exports.into_iter().collect();
        used_exports.sort_by(|a, b| a.0.cmp(&b.0));

        for (stmt_id, used_export_idents) in used_exports {
            let mut used_dep_idents = HashSet::new();
            let mut used_defined_idents = HashSet::new();
            let mut skip = false;

            for ident in used_export_idents {
                match ident {
                    UsedIdent::SwcIdent(i) => {
                        used_defined_idents.insert(i.to_string());
                        let dep_idents = self.stmt(&stmt_id).defined_idents_map.get(&i.to_string());

                        if let Some(dep_idents) = dep_idents {
                            used_dep_idents.extend(dep_idents.iter().map(|i| i.to_string()));
                        }
                    }
                    UsedIdent::Default => {
                        let stmt = self.stmt(&stmt_id);
                        used_dep_idents.extend(stmt.used_idents.iter().map(|i| i.to_string()));
                    }
                    UsedIdent::InExportAll(specifier) => {
                        // if used_statements already contains this statement, add specifier to it
                        if let Some(specifiers) = used_statements.get_mut(&stmt_id) {
                            specifiers.insert(specifier);
                        } else {
                            used_statements.insert(stmt_id, [specifier].into());
                        }
                        skip = true;
                    }
                    UsedIdent::ExportAll => {
                        used_statements.insert(stmt_id, ["*".to_string()].into());
                        skip = true;
                    }
                }
            }

            if skip {
                continue;
            }

            let mut stmts = VecDeque::from([(stmt_id, used_defined_idents, used_dep_idents)]);
            let mut visited = HashSet::new();

            let hash_stmt = |stmt_id: &StatementId, used_defined_idents: &HashSet<String>| {
                let mut sorted_idents =
                    used_defined_idents.iter().cloned().collect::<Vec<String>>();
                sorted_idents.sort();

                format!("{}:{}", stmt_id, sorted_idents.join(""))
            };

            while let Some((stmt_id, used_defined_idents, used_dep_idents)) = stmts.pop_front() {
                let hash = hash_stmt(&stmt_id, &used_defined_idents);

                // if stmt_id is already in used_statements, add used_defined_idents to it
                if let Some(idents) = used_statements.get_mut(&stmt_id) {
                    idents.extend(used_defined_idents);
                } else {
                    used_statements.insert(stmt_id, used_defined_idents);
                }

                if visited.contains(&hash) {
                    continue;
                }

                visited.insert(hash);

                let deps = self.dependencies(&stmt_id);

                for (dep_stmt, dep_idents) in deps {
                    if dep_idents.iter().any(|di| used_dep_idents.contains(di)) {
                        let mut dep_stmt_idents = HashSet::new();
                        let mut dep_used_defined_idents = HashSet::new();

                        for ident in &used_dep_idents {
                            if let Some(dep_idents) =
                                dep_stmt.defined_idents_map.get(&ident.to_string())
                            {
                                dep_used_defined_idents.insert(ident.to_string());
                                dep_stmt_idents.extend(dep_idents.clone());
                            } else {
                                // if dep_stmt.defined_idents contains ident, push it to dep_used_defined_idents
                                if let Some(find_defined_ident) = dep_stmt.defined_idents.get(ident)
                                {
                                    dep_used_defined_idents.insert(find_defined_ident.to_string());
                                }
                            }
                        }

                        // if dep_stmt is already in stmts, merge dep_stmt_idents
                        if let Some((_, used_dep_defined_idents, used_dep_idents)) =
                            stmts.iter_mut().find(|(id, _, _)| *id == dep_stmt.id)
                        {
                            used_dep_defined_idents.extend(dep_used_defined_idents);
                            used_dep_idents.extend(dep_stmt_idents);
                        } else {
                            stmts.push_back((
                                dep_stmt.id,
                                dep_used_defined_idents,
                                dep_stmt_idents,
                            ));
                        }
                    }
                }
            }
        }

        used_statements
    }
}
