use fxhash::FxHasher64;
use hecs::World;
use resources::Resources;
use std::{
    collections::HashMap,
    fmt::Debug,
    hash::{BuildHasherDefault, Hash},
};

use crate::{
    error::{CantInsertSystem, NoSuchSystem},
    system_container::SystemContainer,
    ModQueuePool, System,
};

#[cfg(feature = "parallel")]
use crossbeam::channel::{self, Receiver, Sender};
#[cfg(feature = "parallel")]
use hecs::ArchetypesGeneration;
#[cfg(feature = "parallel")]
use std::collections::HashSet;

#[cfg(feature = "parallel")]
use crate::{
    borrows::{BorrowsContainer, TypeSet},
    Scope,
};

pub(crate) const INVALID_INDEX: &str = "system handles should always map to valid system indices";

#[derive(Clone, Copy, Eq, PartialEq, Hash)]
pub(crate) struct SystemIndex(usize);

pub struct Executor<H>
where
    H: Hash + Eq + PartialEq + Debug,
{
    pub(crate) systems: HashMap<SystemIndex, SystemContainer<H>, BuildHasherDefault<FxHasher64>>,
    pub(crate) system_handles: HashMap<H, SystemIndex>,
    pub(crate) free_indices: Vec<SystemIndex>,
    pub(crate) systems_sorted: Vec<SystemIndex>,

    #[cfg(feature = "parallel")]
    pub(crate) archetypes_generation: Option<ArchetypesGeneration>,
    #[cfg(feature = "parallel")]
    pub(crate) borrows: HashMap<SystemIndex, BorrowsContainer, BuildHasherDefault<FxHasher64>>,
    #[cfg(feature = "parallel")]
    pub(crate) all_resources: TypeSet,
    #[cfg(feature = "parallel")]
    pub(crate) all_components: TypeSet,
    #[cfg(feature = "parallel")]
    pub(crate) systems_to_run: Vec<SystemIndex>,
    #[cfg(feature = "parallel")]
    pub(crate) current_systems: HashSet<SystemIndex, BuildHasherDefault<FxHasher64>>,
    #[cfg(feature = "parallel")]
    pub(crate) finished_systems: HashSet<SystemIndex, BuildHasherDefault<FxHasher64>>,
    #[cfg(feature = "parallel")]
    pub(crate) sender: Sender<SystemIndex>,
    #[cfg(feature = "parallel")]
    pub(crate) receiver: Receiver<SystemIndex>,
}

impl<H> Default for Executor<H>
where
    H: Hash + Eq + PartialEq + Debug,
{
    fn default() -> Self {
        #[cfg(feature = "parallel")]
        let (sender, receiver) = channel::unbounded();
        Self {
            systems: Default::default(),
            system_handles: Default::default(),
            free_indices: Default::default(),
            systems_sorted: Default::default(),

            #[cfg(feature = "parallel")]
            archetypes_generation: None,
            #[cfg(feature = "parallel")]
            borrows: Default::default(),
            #[cfg(feature = "parallel")]
            all_resources: Default::default(),
            #[cfg(feature = "parallel")]
            all_components: Default::default(),
            #[cfg(feature = "parallel")]
            systems_to_run: Default::default(),
            #[cfg(feature = "parallel")]
            current_systems: Default::default(),
            #[cfg(feature = "parallel")]
            finished_systems: Default::default(),
            #[cfg(feature = "parallel")]
            sender,
            #[cfg(feature = "parallel")]
            receiver,
        }
    }
}

impl<H> Executor<H>
where
    H: Hash + Eq + PartialEq + Debug,
{
    pub fn new() -> Self {
        Default::default()
    }

    pub fn builder() -> ExecutorBuilder<H> {
        ExecutorBuilder::new()
    }

    fn new_system_index(&mut self) -> SystemIndex {
        if let Some(index) = self.free_indices.pop() {
            index
        } else {
            SystemIndex(self.systems.len())
        }
    }

    pub(crate) fn resolve_handle(&self, handle: &H) -> Result<SystemIndex, NoSuchSystem> {
        self.system_handles.get(handle).copied().ok_or(NoSuchSystem)
    }

    fn insert_inner(
        &mut self,
        system: System,
        handle: Option<H>,
        dependencies: Vec<H>,
    ) -> Result<Option<(Vec<H>, System)>, CantInsertSystem> {
        #[cfg(feature = "parallel")]
        let borrows_container = BorrowsContainer::new(&system);
        let system_container = SystemContainer::new(system, dependencies);
        let new_index = match handle {
            Some(handle) => self
                .system_handles
                .get(&handle)
                .copied()
                .unwrap_or_else(|| {
                    let index = self.new_system_index();
                    self.system_handles.insert(handle, index);
                    index
                }),
            None => self.new_system_index(),
        };

        let has_dependencies = !system_container.dependencies.is_empty();

        #[cfg(feature = "parallel")]
        let removed_borrows = self.borrows.insert(new_index, borrows_container);
        let removed_system = self
            .systems
            .insert(new_index, system_container)
            .map(|system_container| system_container.unwrap_container());

        if has_dependencies {
            // TODO test thoroughly
            self.systems_sorted.clear();
            while self.systems_sorted.len() != self.systems.len() {
                let mut cycles = true;
                let mut invalid_dependency = None;
                for index in self
                    .systems
                    .keys()
                    .filter(|index| !self.systems_sorted.contains(index))
                {
                    let mut dependencies_satisfied = true;
                    for dependency in &self.systems.get(index).expect(INVALID_INDEX).dependencies {
                        match self.resolve_handle(dependency) {
                            Ok(dependency_index) => {
                                if !self.systems_sorted.contains(&dependency_index) {
                                    dependencies_satisfied = false;
                                    break;
                                }
                            }
                            Err(_) => {
                                invalid_dependency = Some(format!("{:?}", dependency));
                                break;
                            }
                        }
                    }
                    if invalid_dependency.is_some() {
                        break;
                    }
                    if dependencies_satisfied {
                        cycles = false;
                        self.systems_sorted.push(*index);
                        break;
                    }
                }
                if cycles || invalid_dependency.is_some() {
                    #[cfg(feature = "parallel")]
                    {
                        if let Some(borrows_container) = removed_borrows {
                            self.borrows.insert(new_index, borrows_container);
                        }
                    }
                    if let Some(system_container) = removed_system {
                        self.systems.insert(
                            new_index,
                            SystemContainer::new(system_container.1, system_container.0),
                        );
                    }
                    if let Some(dependency) = invalid_dependency {
                        return Err(CantInsertSystem::DependencyNotFound(dependency));
                    }
                    return Err(CantInsertSystem::CyclicDependency);
                }
            }
        } else {
            self.systems_sorted.push(new_index);
        }
        #[cfg(feature = "parallel")]
        self.condense_borrows();

        Ok(removed_system)
    }

    pub fn insert(&mut self, system: System) -> Result<Option<(Vec<H>, System)>, CantInsertSystem> {
        self.insert_inner(system, None, vec![])
    }

    pub fn insert_with_handle(
        &mut self,
        system: System,
        handle: H,
    ) -> Result<Option<(Vec<H>, System)>, CantInsertSystem> {
        self.insert_inner(system, Some(handle), vec![])
    }

    pub fn insert_with_deps(
        &mut self,
        system: System,
        dependencies: Vec<H>,
    ) -> Result<Option<(Vec<H>, System)>, CantInsertSystem> {
        self.insert_inner(system, None, dependencies)
    }

    pub fn insert_with_handle_and_deps(
        &mut self,
        system: System,
        handle: H,
        dependencies: Vec<H>,
    ) -> Result<Option<(Vec<H>, System)>, CantInsertSystem> {
        self.insert_inner(system, Some(handle), dependencies)
    }

    pub fn remove(&mut self, handle: &H) -> Option<(Vec<H>, System)> {
        self.system_handles
            .remove(handle)
            .and_then(|index| {
                #[cfg(feature = "parallel")]
                {
                    self.borrows.remove(&index);
                    self.condense_borrows();
                }
                self.systems.remove(&index)
            })
            .map(|system_container| system_container.unwrap_container())
    }

    pub fn contains(&mut self, handle: &H) -> bool {
        self.system_handles.contains_key(handle)
    }

    pub fn get_mut(
        &mut self,
        handle: &H,
    ) -> Result<impl std::ops::DerefMut<Target = System> + '_, NoSuchSystem> {
        Ok(self
            .systems
            .get_mut(&self.resolve_handle(handle)?)
            .expect(INVALID_INDEX)
            .system_mut())
    }

    pub fn is_active(&self, handle: &H) -> Result<bool, NoSuchSystem> {
        Ok(self
            .systems
            .get(&self.resolve_handle(handle)?)
            .expect(INVALID_INDEX)
            .active)
    }

    pub fn set_active(&mut self, handle: &H, active: bool) -> Result<(), NoSuchSystem> {
        self.systems
            .get_mut(&self.resolve_handle(handle)?)
            .expect(INVALID_INDEX)
            .active = active;
        Ok(())
    }

    pub fn run(&mut self, world: &World, resources: &Resources, mod_queues: &ModQueuePool) {
        for index in &self.systems_sorted {
            let system_container = self.systems.get_mut(&index).expect(INVALID_INDEX);
            if system_container.active {
                system_container
                    .system_mut()
                    .run(world, resources, mod_queues)
            }
        }
    }

    #[cfg(feature = "parallel")]
    pub fn run_with_scope(
        &mut self,
        world: &World,
        resources: &Resources,
        mod_queues: &ModQueuePool,
        scope: &Scope,
    ) {
        for index in &self.systems_sorted {
            let system_container = self.systems.get_mut(&index).expect(INVALID_INDEX);
            if system_container.active {
                system_container
                    .system_mut()
                    .run_with_scope(world, resources, mod_queues, scope)
            }
        }
    }
}

pub struct ExecutorBuilder<H>
where
    H: Hash + Eq + PartialEq + Debug,
{
    executor: Executor<H>,
}

impl<H> Default for ExecutorBuilder<H>
where
    H: Hash + Eq + PartialEq + Debug,
{
    fn default() -> Self {
        Self {
            executor: Default::default(),
        }
    }
}

impl<H> ExecutorBuilder<H>
where
    H: Hash + Eq + PartialEq + Debug,
{
    pub fn new() -> Self {
        Default::default()
    }

    pub fn system(mut self, system: System) -> Self {
        self.executor.insert(system).unwrap();
        self
    }

    pub fn system_with_handle(mut self, system: System, handle: H) -> Self {
        self.executor.insert_with_handle(system, handle).unwrap();
        self
    }

    pub fn system_with_deps(mut self, system: System, dependencies: Vec<H>) -> Self {
        self.executor
            .insert_with_deps(system, dependencies)
            .unwrap();
        self
    }

    pub fn system_with_handle_and_deps(
        mut self,
        system: System,
        handle: H,
        dependencies: Vec<H>,
    ) -> Self {
        self.executor
            .insert_with_handle_and_deps(system, handle, dependencies)
            .unwrap();
        self
    }

    pub fn build(self) -> Executor<H> {
        self.executor
    }
}
