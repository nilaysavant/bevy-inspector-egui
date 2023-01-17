//! A view into the world which may only access certain resources and components

use std::{any::TypeId, marker::PhantomData};

use bevy_ecs::{change_detection::MutUntyped, prelude::*};
use bevy_reflect::{Reflect, ReflectFromPtr, TypeRegistry};
use smallvec::{smallvec, SmallVec};

#[derive(Debug)]
pub enum Error {
    NoAccessToResource(TypeId),
    NoAccessToComponent(EntityComponent),

    ResourceDoesNotExist(TypeId),
    ComponentDoesNotExist(EntityComponent),
    NoComponentId(TypeId),
    NoTypeRegistration(TypeId),
    NoTypeData(TypeId, &'static str),
}

type EntityComponent = (Entity, TypeId);

/// A view into the world which may only access certain resources and components. A restricted form of [`&mut World`](bevy_ecs::world::World).
///
/// Can be used to access a value, and give out the remaining "&mut World" somewhere else.
///
/// # Example usage
///
/// ```no_run
/// use bevy_ecs::prelude::*;
/// use std::any::TypeId;
/// use bevy_inspector_egui::restricted_world_view::RestrictedWorldView;
/// # use bevy_asset::Assets;
/// # use bevy_pbr::StandardMaterial;
///
/// let mut world = World::new();
/// let mut world = RestrictedWorldView::new(&mut world);
///
/// let (mut materials, world) = world.split_off_resource(TypeId::of::<Assets<StandardMaterial>>());
/// let materials = materials.get_resource_mut::<Assets<StandardMaterial>>();
///
/// pass_somewhere_else(world);
/// # fn pass_somewhere_else(_: RestrictedWorldView) {}
/// ```
pub struct RestrictedWorldView<'w> {
    // In an ideal world, we'd use something like `InteriorMutableWorld` https://github.com/bevyengine/bevy/issues/5956,
    // which wraps a `&World` and can (unsafely) access resources and components through `&self`.
    // Alternatively, we should use `&World` and use `_unchecked_mut` methods that work with interior mutability, but they don't
    // exist for `EntityRef::get_mut_by_id` https://github.com/bevyengine/bevy/pull/5922
    // So instead we store a `*World` that can be briefly dereferenced turned into a `&mut World` to get a mutable reference into the world to a resource or component.
    // These live inside the world behind a `BlobVec` and `UnsafeCell`, and a `&mut *world` doesn't invalidate their references in SB, at least according to
    // `MIRIFLAGS="-Zmiri-retag-fields -Zmiri-strict-provenance" cargo miri test`.
    world: *mut World,
    _marker: PhantomData<&'w mut World>,

    resources: Allowed<TypeId>,
    components: Allowed<EntityComponent>,
}

#[derive(Clone)]
enum Allowed<T> {
    // Allowed if included
    AllowList(SmallVec<[T; 2]>),
    // Allowed if not included
    ForbidList(SmallVec<[T; 2]>),
}
impl<T: Clone + PartialEq> Allowed<T> {
    fn allow_just(value: T) -> Allowed<T> {
        Allowed::AllowList(smallvec![value])
    }
    fn allow(values: impl IntoIterator<Item = T>) -> Allowed<T> {
        Allowed::AllowList(values.into_iter().collect())
    }
    fn everything() -> Allowed<T> {
        Allowed::ForbidList(SmallVec::new())
    }
    fn nothing() -> Allowed<T> {
        Allowed::AllowList(SmallVec::new())
    }

    fn allows_access_to(&self, value: T) -> bool {
        match self {
            Allowed::AllowList(list) => list.contains(&value),
            Allowed::ForbidList(list) => !list.contains(&value),
        }
    }

    fn without(&self, value: T) -> Allowed<T> {
        match self {
            Allowed::AllowList(list) => {
                let position = list
                    .iter()
                    .position(|item| *item == value)
                    .expect("called `without` without access");
                let mut new = list.clone();
                new.swap_remove(position);
                Allowed::AllowList(new)
            }
            Allowed::ForbidList(list) => {
                let mut new = list.clone();
                new.push(value);
                Allowed::ForbidList(new)
            }
        }
    }
    fn without_many(&self, values: impl Iterator<Item = T>) -> Allowed<T>
    where
        T: Copy,
    {
        match self {
            Allowed::AllowList(list) => {
                let new = list.clone();
                for value in values {
                    let position = list
                        .iter()
                        .position(|item| *item == value)
                        .expect("called `without` without access");
                    let mut new = list.clone();
                    new.swap_remove(position);
                }
                Allowed::AllowList(new)
            }
            Allowed::ForbidList(list) => {
                let mut new = list.clone();
                new.extend(values);
                Allowed::ForbidList(new)
            }
        }
    }
}

impl<'a> From<&'a mut World> for RestrictedWorldView<'a> {
    fn from(value: &'a mut World) -> Self {
        RestrictedWorldView::new(value)
    }
}

/// Fundamental methods for working with a [`RestrictedWorldView`]
impl<'w> RestrictedWorldView<'w> {
    /// Create a new [`RestrictedWorldView`] with permission to access everything.
    pub fn new(world: &'w mut World) -> RestrictedWorldView<'w> {
        // INVARIANTS: `world` is `&mut` so we have access to everything
        RestrictedWorldView {
            world,
            _marker: PhantomData,
            resources: Allowed::everything(),
            components: Allowed::everything(),
        }
    }

    /// Splits the world into one view which may only be used for resource access, and another which may only be used for component access.
    pub fn resources_components(
        world: &'w mut World,
    ) -> (RestrictedWorldView<'w>, RestrictedWorldView<'w>) {
        // INVARIANTS: `world` is `&mut` so we have access to everything
        let resources = RestrictedWorldView {
            world,
            _marker: PhantomData,
            resources: Allowed::everything(),
            components: Allowed::nothing(),
        };
        let components = RestrictedWorldView {
            world,
            _marker: PhantomData,
            resources: Allowed::nothing(),
            components: Allowed::everything(),
        };

        (resources, components)
    }

    /// Get a reference to the inner [`World`].
    ///
    /// # Safety
    /// - The returned world reference may only be used to immediately access (mutably or immutably) resources and components
    /// that [`RestrictedWorldView::allows_access_to_resource`] and [`RestrictedWorldView::allows_access_to_component`] return `true` for.
    /// - No references into the world can remain when control is handed to unknown safe code
    pub(crate) unsafe fn get(&self) -> &'w World {
        // SAFETY: the caller
        unsafe { &mut *self.world }
    }
    // this is only used for the by_id methods that don't have unchecked variants.
    // same SAFETY as get, again absolutely *no* references to the world in the presence of other views,
    // you can only get a reference deep in the storage (like a resource) that doesn't get invalidated from a `&mut *` of the world.
    unsafe fn get_mut(&mut self) -> &'w mut World {
        // SAFETY: the caller
        unsafe { &mut *self.world }
    }

    // required because get_component_unchecked_by_id doesn't exist
    unsafe fn get_mut_from_shared(&self) -> &'w mut World {
        // SAFETY: the caller
        unsafe { &mut *self.world }
    }

    /// Whether the resource with the given [`TypeId`] may be accessed from this world view
    pub fn allows_access_to_resource(&self, type_id: TypeId) -> bool {
        self.resources.allows_access_to(type_id)
    }
    /// Whether the given component at the entity may be accessed from this world view
    pub fn allows_access_to_component(&self, component: EntityComponent) -> bool {
        self.components.allows_access_to(component)
    }

    /// Splits this view into one view that only has access the the resource `resource` (`.0`), and the rest (`.1`).
    pub fn split_off_resource(
        &mut self,
        resource: TypeId,
    ) -> (RestrictedWorldView<'_>, RestrictedWorldView<'_>) {
        assert!(self.allows_access_to_resource(resource));

        // INVARIANTS: `self` had `resource` access, so `split` has access if we remove it from `self`
        let split = RestrictedWorldView {
            world: self.world,
            _marker: PhantomData,
            resources: Allowed::allow_just(resource),
            components: Allowed::nothing(),
        };
        let rest = RestrictedWorldView {
            world: self.world,
            _marker: PhantomData,
            resources: self.resources.without(resource),
            components: self.components.clone(),
        };

        (split, rest)
    }

    /// Like [`RestrictedWorldView::split_off_resource`], but takes `self` and returns `'w` lifetimes.
    pub fn split_off_resource_typed<R: Resource>(
        self,
    ) -> Option<(Mut<'w, R>, RestrictedWorldView<'w>)> {
        let type_id = TypeId::of::<R>();
        assert!(self.allows_access_to_resource(type_id));

        // SAFETY: `self` had `R` access, so we have unique access if we remove it from `self`
        let resource = unsafe { self.get().get_resource_unchecked_mut::<R>()? };

        let rest = RestrictedWorldView {
            world: self.world,
            _marker: PhantomData,
            resources: self.resources.without(type_id),
            components: self.components,
        };

        Some((resource, rest))
    }

    /// Splits this view into one view that only has access the the component `component.1` at the entity `component.0` (`.0`), and the rest (`.1`).
    pub fn split_off_component(
        &mut self,
        component: EntityComponent,
    ) -> (RestrictedWorldView<'_>, RestrictedWorldView<'_>) {
        assert!(self.allows_access_to_component(component));

        // INVARIANTS: `self` had `component` access, so `split` has access if we remove it from `self`
        let split = RestrictedWorldView {
            world: self.world,
            _marker: PhantomData,
            resources: Allowed::nothing(),
            components: Allowed::allow_just(component),
        };
        let rest = RestrictedWorldView {
            world: self.world,
            _marker: PhantomData,
            resources: self.resources.clone(),
            components: self.components.without(component),
        };

        (split, rest)
    }

    /// Splits this view into one view that only has access the the component-entity pairs `components` (`.0`), and the rest (`.1`)
    pub fn split_off_components(
        &mut self,
        components: impl Iterator<Item = EntityComponent> + Copy,
    ) -> (RestrictedWorldView<'_>, RestrictedWorldView<'_>) {
        for component in components {
            assert!(self.allows_access_to_component(component));
        }

        // INVARIANTS: `self` had `component` access, so `split` has access if we remove it from `self`
        let split = RestrictedWorldView {
            world: self.world,
            _marker: PhantomData,
            resources: Allowed::nothing(),
            components: Allowed::allow(components),
        };
        let rest = RestrictedWorldView {
            world: self.world,
            _marker: PhantomData,
            resources: self.resources.clone(),
            components: self.components.without_many(components),
        };

        (split, rest)
    }
}

/// Some safe methods for getting values out of the [`RestrictedWorldView`].
/// Also has some methods for getting values in their [`Reflect`] form.
impl<'w> RestrictedWorldView<'w> {
    pub fn contains_entity(&self, entity: Entity) -> bool {
        // SAFETY: no access, just metadata
        let world = unsafe { self.get() };
        world.entities().contains(entity)
    }

    /// Gets a mutable reference to the resource of the given type
    pub fn get_resource_mut<R: Resource>(&mut self) -> Result<Mut<'_, R>, Error> {
        // SAFETY: &mut self
        unsafe { self.get_resource_unchecked_mut() }
    }

    /// Gets mutable reference to two resources. Panics if `R1 = R2`.
    pub fn get_two_resources_mut<R1: Resource, R2: Resource>(
        &mut self,
    ) -> (Result<Mut<'_, R1>, Error>, Result<Mut<'_, R2>, Error>) {
        assert_ne!(TypeId::of::<R1>(), TypeId::of::<R2>());
        // SAFETY: &mut self, R1!=R2
        let r1 = unsafe { self.get_resource_unchecked_mut::<R1>() };
        // SAFETY: &mut self, R1!=R2
        let r2 = unsafe { self.get_resource_unchecked_mut::<R2>() };

        (r1, r2)
    }

    /// # Safety
    /// This method does validate that we have access to `R`, but takes `&self`
    /// and as such doesn't check unique access.
    unsafe fn get_resource_unchecked_mut<R: Resource>(&self) -> Result<Mut<'_, R>, Error> {
        let type_id = TypeId::of::<R>();
        if !self.allows_access_to_resource(type_id) {
            return Err(Error::NoAccessToResource(type_id));
        }

        // SAFETY: we have access to `type_id`, get a reference into the world, and drop the `World` borrow
        let value = unsafe {
            self.get()
                .get_resource_unchecked_mut::<R>()
                .ok_or(Error::ResourceDoesNotExist(type_id))?
        };

        Ok(value)
    }

    /// Gets a mutable reference in form of a [`&mut dyn Reflect`](bevy_reflect::Reflect) to the resource given by `type_id`.
    ///
    /// Returns an error if the type does not register [`Reflect`].
    ///
    /// Also returns a `impl FnOnce()` to mark the value as changed.
    pub fn get_resource_reflect_mut_by_id(
        &mut self,
        type_id: TypeId,
        type_registry: &TypeRegistry,
    ) -> Result<(&'_ mut dyn Reflect, impl FnOnce() + '_), Error> {
        if !self.allows_access_to_resource(type_id) {
            return Err(Error::NoAccessToResource(type_id));
        }

        // SAFETY: this only accesses the component ID and doesn't keep any references
        let component_id = unsafe {
            self.get()
                .components()
                .get_resource_id(type_id)
                .ok_or(Error::ResourceDoesNotExist(type_id))?
        };

        // SAFETY: we have access to `type_id` and borrow `&mut self`
        let value = unsafe {
            self.get_mut()
                .get_resource_mut_by_id(component_id)
                .ok_or(Error::ResourceDoesNotExist(type_id))?
        };

        // SAFETY: value is of type type_id
        let value = unsafe { mut_untyped_to_reflect(value, type_registry, type_id)? };

        Ok(value)
    }

    /// Gets a mutable reference in form of a [`&mut dyn Reflect`](bevy_reflect::Reflect) to a component at an entity.
    ///
    /// Returns an error if the type does not register [`Reflect`].
    ///
    /// Also returns a `impl FnOnce()` to mark the value as changed.
    pub fn get_entity_component_reflect(
        &mut self,
        entity: Entity,
        component: TypeId,
        type_registry: &TypeRegistry,
    ) -> Result<(&'_ mut dyn Reflect, bool, impl FnOnce() + '_), Error> {
        if !self.allows_access_to_component((entity, component)) {
            return Err(Error::NoAccessToComponent((entity, component)));
        }

        // SAFETY: this only accesses the component ID and doesn't keep any references
        let component_id = unsafe {
            self.get()
                .components()
                .get_id(component)
                .ok_or(Error::NoComponentId(component))?
        };

        // SAFETY: we have access to (entity, component) and borrow `&mut self`
        let value = unsafe {
            self.get_mut()
                .get_mut_by_id(entity, component_id)
                .ok_or(Error::ComponentDoesNotExist((entity, component)))?
        };
        let changed = value.is_changed();

        let (value, set_changed) =
            // SAFETY: value is of type component
            unsafe { mut_untyped_to_reflect(value, type_registry, component) }?;
        Ok((value, changed, set_changed))
    }

    // SAFETY: must ensure distinct access
    pub(crate) unsafe fn get_entity_component_reflect_unchecked(
        &self,
        entity: Entity,
        component: TypeId,
        type_registry: &TypeRegistry,
    ) -> Result<(&'_ mut dyn Reflect, impl FnOnce() + '_), Error> {
        if !self.allows_access_to_component((entity, component)) {
            return Err(Error::NoAccessToComponent((entity, component)));
        }

        // SAFETY: this only accesses the component ID and doesn't keep any references
        let component_id = unsafe {
            self.get()
                .components()
                .get_id(component)
                .ok_or(Error::NoComponentId(component))?
        };

        // SAFETY: we have access to (entity, component) and caller ensures distinct access
        let value = unsafe {
            self.get_mut_from_shared()
                .get_mut_by_id(entity, component_id)
                .ok_or(Error::ComponentDoesNotExist((entity, component)))?
        };

        // SAFETY: value is of type component
        unsafe { mut_untyped_to_reflect(value, type_registry, component) }
    }
}

// SAFETY: MutUntyped is of type with `type_id`
unsafe fn mut_untyped_to_reflect<'a>(
    value: MutUntyped<'a>,
    type_registry: &TypeRegistry,
    type_id: TypeId,
) -> Result<(&'a mut dyn Reflect, impl FnOnce() + 'a), Error> {
    let registration = type_registry
        .get(type_id)
        .ok_or(Error::NoTypeRegistration(type_id))?;
    let reflect_from_ptr = registration
        .data::<ReflectFromPtr>()
        .ok_or(Error::NoTypeData(type_id, "ReflectFromPtr"))?;

    let (ptr, set_changed) = crate::utils::mut_untyped_split(value);
    assert_eq!(reflect_from_ptr.type_id(), type_id);
    // SAFETY: ptr is of type type_id as required in safety contract, type_id was checked above
    let value = unsafe { reflect_from_ptr.as_reflect_ptr_mut(ptr) };

    Ok((value, set_changed))
}

#[cfg(test)]
mod tests {
    use std::any::TypeId;

    use bevy_ecs::prelude::*;
    use bevy_reflect::{Reflect, TypeRegistry};

    use super::RestrictedWorldView;

    #[derive(Resource)]
    struct A(String);

    #[derive(Resource, Reflect, Default)]
    #[reflect(Resource)]
    struct B(String);

    #[test]
    fn disjoint_resource_access() {
        let mut world = World::new();
        world.insert_resource(A("a".to_string()));
        world.insert_resource(B("b".to_string()));

        let mut world = RestrictedWorldView::new(&mut world);

        let (mut a_view, mut world) = world.split_off_resource(TypeId::of::<A>());
        let mut a = a_view.get_resource_mut::<A>().unwrap();
        let mut b = world.get_resource_mut::<B>().unwrap();

        a.0.clear();
        b.0.clear();
    }

    #[test]
    fn disjoint_resource_access_by_id() {
        let mut world = World::new();
        world.insert_resource(A("a".to_string()));
        world.insert_resource(B("b".to_string()));

        let mut world = RestrictedWorldView::new(&mut world);

        let (mut a_view, mut world) = world.split_off_resource(TypeId::of::<A>());
        let mut a = a_view.get_resource_mut::<A>().unwrap();

        let mut type_registry = TypeRegistry::empty();
        type_registry.register::<B>();
        let b = world
            .get_resource_reflect_mut_by_id(TypeId::of::<B>(), &type_registry)
            .unwrap();

        a.0.clear();
        b.0.downcast_mut::<B>().unwrap().0.clear();
    }

    #[test]
    fn get_two_resources_mut() {
        let mut world = World::new();
        world.insert_resource(A("a".to_string()));
        world.insert_resource(B("b".to_string()));

        let mut world = RestrictedWorldView::new(&mut world);
        let (a, b) = world.get_two_resources_mut::<A, B>();
        a.unwrap().0.clear();
        b.unwrap().0.clear();
    }

    #[test]
    fn invalid_resource_access() {
        let mut world = World::new();
        let mut world = RestrictedWorldView::new(&mut world);

        let (a_view, mut a_remaining) = world.split_off_resource(TypeId::of::<A>());

        assert!(a_view.allows_access_to_resource(TypeId::of::<A>()));
        assert!(!a_remaining.allows_access_to_resource(TypeId::of::<A>()));
        assert!(!a_view.allows_access_to_resource(TypeId::of::<B>()));
        assert!(a_remaining.allows_access_to_resource(TypeId::of::<B>()));

        let (b_view, b_remaining) = a_remaining.split_off_resource(TypeId::of::<B>());

        assert!(b_view.allows_access_to_resource(TypeId::of::<B>()));
        assert!(!b_remaining.allows_access_to_resource(TypeId::of::<B>()));
    }

    #[derive(Component, Reflect)]
    struct ComponentA(String);

    #[test]
    fn disjoint_component_access() {
        let mut type_registry = TypeRegistry::empty();
        type_registry.register::<ComponentA>();
        type_registry.register::<String>();

        let mut world = World::new();
        world.insert_resource(A("a".to_string()));
        let entity = world.spawn(ComponentA("a".to_string())).id();

        let mut world = RestrictedWorldView::new(&mut world);

        let (mut component_view, mut world) =
            world.split_off_component((entity, TypeId::of::<ComponentA>()));
        let component = component_view
            .get_entity_component_reflect(entity, TypeId::of::<ComponentA>(), &type_registry)
            .unwrap();
        let mut resource = world.get_resource_mut::<A>().unwrap();

        component.0.downcast_mut::<ComponentA>().unwrap().0.clear();
        resource.0.clear();
    }
}
