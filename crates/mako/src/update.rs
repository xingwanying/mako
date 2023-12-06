use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fmt::Debug;
use std::path::PathBuf;
use std::sync::Arc;

use mako_core::anyhow::{anyhow, Ok, Result};
use mako_core::colored::Colorize;
use mako_core::rayon::prelude::*;
use mako_core::tracing::debug;

use crate::build::{GenericError, Task};
use crate::compiler::Compiler;
use crate::module::{Dependency, Module, ModuleId};
use crate::resolve::{self, get_resolvers, Resolvers};
use crate::transform_in_generate::transform_modules;
use crate::transformers::transform_virtual_css_modules::is_css_path;
use crate::util::create_thread_pool;

#[derive(Debug, Clone)]
pub enum UpdateType {
    Add,
    Remove,
    Modify,
}

#[derive(Default, Debug)]
pub struct UpdateResult {
    // 新增的模块Id
    pub added: HashSet<ModuleId>,
    // 删除的模块Id
    pub removed: HashSet<ModuleId>,
    // 修改的模块Id
    pub modified: HashSet<ModuleId>,
}

impl UpdateResult {
    pub fn is_updated(&self) -> bool {
        !self.modified.is_empty() || !self.added.is_empty() || !self.removed.is_empty()
    }
}

impl fmt::Display for UpdateResult {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let mut added = self.added.iter().map(|f| f.id.clone()).collect::<Vec<_>>();
        added.sort_by_key(|id| id.to_string());
        let mut modified = self
            .modified
            .iter()
            .map(|f| f.id.clone())
            .collect::<Vec<_>>();
        modified.sort_by_key(|id| id.to_string());
        let mut removed = self
            .removed
            .iter()
            .map(|f| f.id.clone())
            .collect::<Vec<_>>();
        removed.sort_by_key(|id| id.to_string());
        write!(
            f,
            r#"
added:{:?}
modified:{:?}
removed:{:?}
"#,
            &added, &modified, &removed
        )
    }
}

impl Compiler {
    pub fn update(&self, paths: Vec<PathBuf>) -> Result<UpdateResult> {
        let module_graph = self.context.module_graph.read().unwrap();
        let paths = paths
            .into_iter()
            .map(|path| {
                let update_type = if path.exists() {
                    let p_str = path.to_string_lossy().to_string();
                    let with_as_module = format!("{}?asmodule", p_str);
                    if module_graph.has_module(&path.clone().into())
                        || module_graph.has_module(&with_as_module.into())
                    {
                        UpdateType::Modify
                    } else {
                        UpdateType::Add
                    }
                } else {
                    UpdateType::Remove
                };
                (path, update_type)
            })
            .collect::<Vec<_>>();
        drop(module_graph);
        debug!("update: {:?}", &paths);
        let mut update_result: UpdateResult = Default::default();
        let resolvers = Arc::new(get_resolvers(&self.context.config));

        let mut modified = vec![];
        let mut removed = vec![];
        let mut added = vec![];

        debug!("checking added...");
        let mut has_added = false;
        for (path, update_type) in &paths {
            if matches!(update_type, UpdateType::Add) {
                debug!("  > {} is added", path.to_string_lossy());
                has_added = true;
                break;
            }
        }
        debug!("checking added...done, has_added:{}", has_added);

        // try to resolve modules with missing deps
        // if found, add to modified queue
        if has_added {
            debug!("checking modules_with_missing_deps... since has added modules");
            let mut modules_with_missing_deps =
                self.context.modules_with_missing_deps.write().unwrap();
            let mut module_graph = self.context.module_graph.write().unwrap();
            for module_id in modules_with_missing_deps.clone().iter() {
                let id = ModuleId::new(module_id.clone());
                let module = module_graph.get_module_mut(&id).unwrap();
                let missing_deps = module.info.clone().unwrap().missing_deps;
                for (_source, dep) in missing_deps {
                    let resolved =
                        resolve::resolve(module_id, &dep, &self.context.resolvers, &self.context);
                    if resolved.is_ok() {
                        debug!(
                            "  > missing deps resolved {:?} from {:?}",
                            dep.source, module_id
                        );
                        modified.push(PathBuf::from(module_id.clone()));
                        let info = module.info.as_mut().unwrap();
                        info.missing_deps.remove(&dep.source);
                        if info.missing_deps.is_empty() {
                            debug!("  > remove {} from modules_with_missing_deps", module_id);
                            modules_with_missing_deps.retain(|x| x == module_id);
                        }
                    }
                }
            }
            debug!("checking modules_with_missing_deps...done");
        }

        // watch 到变化的文件，如果不在之前的 module graph 中，需过滤掉
        debug!("filtering paths...");
        let paths: Vec<(PathBuf, UpdateType)> = {
            let module_graph = self.context.module_graph.read().unwrap();
            let mut new_paths = vec![];
            paths.into_iter().for_each(|(p, update_type)| {
                if module_graph.has_module(&p.clone().into()) {
                    debug!("  > {} is filtered", p.to_string_lossy());
                    new_paths.push((p.clone(), update_type.clone()));
                }
                let p_str = p.to_string_lossy().to_string();
                if is_css_path(&p_str) {
                    let with_as_module = format!("{}?asmodule", p_str);
                    if module_graph.has_module(&with_as_module.clone().into()) {
                        debug!("  > {} is filtered", with_as_module);
                        new_paths.push((PathBuf::from(with_as_module), update_type));
                    }
                }
            });
            new_paths
        };
        debug!("filtering paths...done");

        // 先分组
        for (path, update_type) in paths {
            match update_type {
                UpdateType::Add => {
                    added.push(path);
                }
                UpdateType::Remove => {
                    removed.push(path);
                }
                UpdateType::Modify => {
                    modified.push(path);
                }
            }
        }

        // 先做删除
        debug!("remove: {:?}", &removed);
        let (removed_module_ids, affected_module_ids) = self.build_by_remove(removed);
        debug!("after build_by_remove");
        debug!("  > removed_module_ids: {:?}", &removed_module_ids);
        debug!(
            "  > affected_module_ids: {:?} (these will be added to modified",
            &affected_module_ids
        );
        update_result.removed.extend(removed_module_ids);
        modified.extend(affected_module_ids.into_iter().map(|i| i.to_path()));

        // 分析修改的模块，结果中会包含新增的模块
        debug!("modify: {:?}", &modified);
        let (modified_module_ids, add_paths) = self
            .build_by_modify(modified, resolvers.clone())
            .map_err(|err| anyhow!(err))?;
        debug!("after build_by_modify");
        debug!("  > modified_module_ids: {:?}", &modified_module_ids);
        debug!(
            "  > add_paths: {:?} (these will be added to added)",
            &add_paths
        );

        added.extend(add_paths);
        debug!("added:{:?}", &added);
        update_result.modified.extend(modified_module_ids);

        // 最后做添加
        debug!("add: {:?}", &added);
        let added_module_ids = self.build_by_add(&added, resolvers)?;
        update_result.added.extend(
            added
                .into_iter()
                .map(ModuleId::from_path)
                .collect::<HashSet<_>>(),
        );
        update_result.added.extend(added_module_ids);

        debug!("update_result: {:?}", &update_result);
        Result::Ok(update_result)
    }

    pub fn transform_for_change(&self, update_result: &UpdateResult) -> Result<()> {
        let mut changes: Vec<ModuleId> = vec![];
        for module_id in &update_result.added {
            changes.push(module_id.clone());
        }
        for module_id in &update_result.modified {
            changes.push(module_id.clone());
        }
        transform_modules(changes, &self.context)?;
        Ok(())
    }

    fn build_by_modify(
        &self,
        mut modified: Vec<PathBuf>,
        resolvers: Arc<Resolvers>,
    ) -> Result<(HashSet<ModuleId>, Vec<PathBuf>)> {
        let module_graph = self.context.module_graph.read().unwrap();
        let modules = module_graph.modules();

        // if ?modules is modified, add ?asmodule to modified
        for module in modules
            .iter()
            .filter(|module| module.id.id.contains("?modules"))
        {
            let origin_id: &str = module.id.id.split('?').next().unwrap();
            let css_modules_virtual_id = format!("{}?asmodule", origin_id);
            if modified.contains(&PathBuf::from(css_modules_virtual_id)) {
                modified.push(PathBuf::from(module.id.id.clone()));
            }
        }
        drop(module_graph);

        let result = modified
            .par_iter()
            .map(|entry| {
                debug!("build by modify: {:?} start", entry);
                // first build
                let is_entry = {
                    // there must be a entry, so unwrap is safe
                    let mut entries = self.context.config.entry.values();
                    entries.any(|e| e.eq(entry))
                };

                let (module, dependencies) = Compiler::build_module(
                    &self.context,
                    &Task {
                        path: entry.to_string_lossy().to_string(),
                        is_entry,
                        parent_resource: None,
                    },
                    resolvers.clone(),
                )?;

                debug!(
                    "  > missing deps: {:?}",
                    module.info.clone().unwrap().missing_deps
                );

                // update modules_with_missing_deps
                if module.info.clone().unwrap().missing_deps.is_empty() {
                    self.context
                        .modules_with_missing_deps
                        .write()
                        .unwrap()
                        .retain(|id| id != &module.id.id);
                } else {
                    self.context
                        .modules_with_missing_deps
                        .write()
                        .unwrap()
                        .push(module.id.id.clone());
                }

                // diff
                let module_graph = self.context.module_graph.read().unwrap();
                let current_dependencies: Vec<(ModuleId, Dependency)> = module_graph
                    .get_dependencies(&module.id)
                    .into_iter()
                    .map(|(module_id, dep)| (module_id.clone(), dep.clone()))
                    .collect();
                drop(module_graph);

                let mut add_modules: HashMap<ModuleId, Module> = HashMap::new();
                let mut target_dependencies: Vec<(ModuleId, Dependency)> = vec![];
                dependencies.into_iter().for_each(|(resource, dep)| {
                    let resolved_path = resource.get_resolved_path();
                    let module_id = ModuleId::new(resolved_path);
                    // TODO: handle error
                    let module = Self::create_module(&resource, &module_id, &self.context).unwrap();
                    target_dependencies.push((module_id.clone(), dep));
                    add_modules.insert(module_id, module);
                });

                let d = diff(current_dependencies, target_dependencies);
                debug!("build by modify: {:?} end", entry);
                Result::Ok((module, d.added, d.removed, add_modules))
            })
            .collect::<Result<Vec<_>>>();
        let result = result?;

        let mut added = vec![];
        let mut modified_module_ids = HashSet::new();

        let mut module_graph = self.context.module_graph.write().unwrap();
        for (module, add, remove, mut add_modules) in result {
            // remove bind dependency
            for (remove_module_id, dep) in remove {
                module_graph.remove_dependency(&module.id, &remove_module_id, &dep);
            }

            // add bind dependency
            for (add_module_id, dep) in &add {
                let add_module = add_modules.remove(add_module_id).unwrap();

                // 只针对非 external 的模块设置 add Task
                if add_module.info.is_none() {
                    added.push(add_module_id.to_path());
                }

                module_graph.add_module(add_module);
                module_graph.add_dependency(&module.id, add_module_id, dep.clone());
            }

            modified_module_ids.insert(module.id.clone());

            // replace module
            module_graph.replace_module(module);
        }

        Result::Ok((modified_module_ids, added))
    }

    fn build_by_add(
        &self,
        added: &Vec<PathBuf>,
        resolvers: Arc<Resolvers>,
    ) -> Result<HashSet<ModuleId>> {
        let (pool, rs, rr) = create_thread_pool::<Result<ModuleId>>();
        for path in added {
            Self::build_module_graph_threaded(
                pool.clone(),
                self.context.clone(),
                Task {
                    path: path.to_string_lossy().to_string(),
                    is_entry: false,
                    parent_resource: None,
                },
                rs.clone(),
                resolvers.clone(),
            );
        }

        drop(rs);

        let mut errors = vec![];
        let mut module_ids = HashSet::new();
        for r in rr {
            match r {
                Result::Ok(module_id) => {
                    module_ids.insert(module_id);
                }
                Err(err) => {
                    // unescape
                    let mut err = err
                        .to_string()
                        .replace("\\n", "\n")
                        .replace("\\u{1b}", "\u{1b}")
                        .replace("\\\\", "\\");
                    // remove first char and last char
                    if err.starts_with('"') && err.ends_with('"') {
                        err = err[1..err.len() - 1].to_string();
                    }
                    errors.push(err);
                }
            }
        }

        if !errors.is_empty() {
            eprintln!("{}", "Build failed.".to_string().red());
            return Err(anyhow!(GenericError(errors.join(", "))));
        }

        Ok(module_ids)
    }

    fn build_by_remove(&self, removed: Vec<PathBuf>) -> (HashSet<ModuleId>, HashSet<ModuleId>) {
        let mut module_graph = self.context.module_graph.write().unwrap();
        let mut removed_module_ids = HashSet::new();
        let mut affected_module_ids = HashSet::new();
        for path in removed {
            let module_id = ModuleId::from_path(path);
            let dependants = module_graph.dependant_module_ids(&module_id);
            module_graph.remove_module_and_deps(&module_id);
            affected_module_ids.extend(dependants);
            removed_module_ids.insert(module_id);
        }
        (removed_module_ids, affected_module_ids)
    }
}

pub struct Diff {
    added: HashSet<(ModuleId, Dependency)>,
    removed: HashSet<(ModuleId, Dependency)>,
}

// 对比两颗 Dependency 的差异
fn diff(origin: Vec<(ModuleId, Dependency)>, target: Vec<(ModuleId, Dependency)>) -> Diff {
    let origin_module_ids = origin
        .iter()
        .map(|(module_id, _dep)| module_id)
        .collect::<HashSet<_>>();
    let target_module_ids = target
        .iter()
        .map(|(module_id, _dep)| module_id)
        .collect::<HashSet<_>>();
    let mut added: HashSet<(ModuleId, Dependency)> = HashSet::new();
    let mut removed: HashSet<(ModuleId, Dependency)> = HashSet::new();
    target
        .iter()
        .filter(|(module_id, _dep)| !origin_module_ids.contains(module_id))
        .for_each(|(module_id, dep)| {
            added.insert((module_id.clone(), dep.clone()));
        });
    origin
        .iter()
        .filter(|(module_id, _dep)| !target_module_ids.contains(module_id))
        .for_each(|(module_id, dep)| {
            removed.insert((module_id.clone(), dep.clone()));
        });
    Diff { added, removed }
}
