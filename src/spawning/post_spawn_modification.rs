use crate::shader::Materials;
use bevy::math::Vec3Swizzles;
use bevy::prelude::*;
use bevy::render::mesh::{PrimitiveTopology, VertexAttributeValues};
use bevy_pathmesh::PathMesh;
use bevy_rapier3d::prelude::*;
use itertools::Itertools;
use ordered_float::OrderedFloat;
use serde::{Deserialize, Serialize};
use std::iter;

#[derive(Debug, Clone, Eq, PartialEq, Component, Reflect, Serialize, Deserialize, Default)]
#[reflect(Component, Serialize, Deserialize)]
pub struct CustomCollider;

#[allow(clippy::type_complexity)]
pub fn read_colliders(
    mut commands: Commands,
    added_name: Query<(Entity, &Name, &Children), (Added<Name>, Without<CustomCollider>)>,
    meshes: Res<Assets<Mesh>>,
    mesh_handles: Query<&Handle<Mesh>>,
) {
    for (entity, name, children) in &added_name {
        if name.to_lowercase().contains("[collider]") {
            let (collider_entity, collider_mesh) = get_mesh(children, &meshes, &mesh_handles);
            commands.entity(collider_entity).despawn_recursive();

            let rapier_collider =
                Collider::from_bevy_mesh(collider_mesh, &ComputedColliderShape::TriMesh).unwrap();

            commands.entity(entity).insert(rapier_collider);
        }
    }
}

pub fn set_texture_to_repeat(
    mut commands: Commands,
    added_name: Query<(&Name, &Children), Added<Name>>,
    material_handles: Query<&Handle<StandardMaterial>>,
    materials: Res<Materials>,
) {
    for (name, children) in &added_name {
        if name.to_lowercase().contains("[ground]") {
            let child = children
                .iter()
                .find(|entity| material_handles.get(**entity).is_ok())
                .unwrap();

            commands
                .entity(*child)
                .remove::<Handle<StandardMaterial>>()
                .insert(materials.repeated.clone());
        }
    }
}

pub fn read_navmesh(
    mut commands: Commands,
    added_name: Query<(Entity, &Name, &Children), Added<Name>>,
    parents: Query<&Parent>,
    transforms: Query<&Transform>,
    meshes: Res<Assets<Mesh>>,
    mesh_handles: Query<&Handle<Mesh>>,
    mut path_meshes: ResMut<Assets<PathMesh>>,
) {
    for (parent, name, children) in &added_name {
        if name.to_lowercase().contains("[navmesh]") {
            // Necessary because at this stage the `GlobalTransform` is still at `default()` for some reason
            let global_transform = get_global_transform(parent, &parents, &transforms);
            let (child, mesh) = get_mesh(children, &meshes, &mesh_handles);
            let mesh_vertices = match mesh.attribute(Mesh::ATTRIBUTE_POSITION).unwrap() {
                VertexAttributeValues::Float32x3(values) => values,
                _ => panic!(),
            };

            let triangle_edge_indices = mesh.indices().unwrap();
            let triangles: Vec<_> = triangle_edge_indices
                .iter()
                .tuples()
                .map(|(a, b, c)| [a, b, c].map(|index| index.try_into().unwrap()).to_vec())
                .collect();

            let mut vertices: Vec<_> = mesh_vertices
                .into_iter()
                .map(|coords| (*coords).into())
                .map(|coords| global_transform.transform_point(coords))
                .map(|coords| coords.xz())
                .enumerate()
                .map(|(vertex_index, coords)| {
                    let neighbor_indices = triangles
                        .iter()
                        .enumerate()
                        .filter_map(|(polygon_index, vertex_indices_in_polygon)| {
                            vertex_indices_in_polygon
                                .contains(&(vertex_index as u32))
                                .then_some(polygon_index)
                        })
                        .map(|index| isize::try_from(index).unwrap())
                        .collect();
                    polyanya::Vertex::new(coords, neighbor_indices)
                })
                .collect();
            let polygons: Vec<_> = triangles
                .into_iter()
                .map(|vertex_indices_in_polygon| {
                    let is_one_way = vertex_indices_in_polygon
                        .iter()
                        .map(|index| &vertices[*index as usize])
                        .map(|vertex| &vertex.polygons)
                        .flatten()
                        .unique()
                        .take(3)
                        .count()
                        // One way means all vertices have at most 2 neighbors: the original polygon and one other
                        < 3;
                    polyanya::Polygon::new(vertex_indices_in_polygon, is_one_way)
                })
                .collect();
            let unordered_vertices = vertices.clone();
            for (vertex_index, vertex) in vertices.iter_mut().enumerate() {
                // Start is arbitrary
                vertex.polygons.sort_by_key(|index| {
                    // No -1 present yet, so the unwrap is safe
                    let index = usize::try_from(*index).unwrap();
                    let polygon = &polygons[index];
                    let counterclockwise_edge = polygon
                        .get_counterclockwise_edge_containing_vertex(vertex_index)
                        // All neighbor polygons will have exactly one counterclockwise and one clockwise edge connected to this vertex
                        .unwrap();
                    let other_vertex = &unordered_vertices[counterclockwise_edge.1];
                    let vector = other_vertex.coords - vertex.coords;
                    OrderedFloat(vector.y.atan2(vector.x))
                });
                let mut polygons_including_obstacles = vec![vertex.polygons[0]];
                for polygon_index in vertex
                    .polygons
                    .iter()
                    .cloned()
                    .skip(1)
                    .chain(iter::once(polygons_including_obstacles[0]))
                {
                    let last_index = *polygons_including_obstacles.last().unwrap();
                    if last_index == -1 {
                        polygons_including_obstacles.push(polygon_index);
                        continue;
                    }
                    let last_polygon = &polygons[usize::try_from(last_index).unwrap()];
                    let last_counterclockwise_edge = last_polygon
                        .get_counterclockwise_edge_containing_vertex(vertex_index)
                        .unwrap();

                    let next_polygon = &polygons[usize::try_from(polygon_index).unwrap()];
                    let next_clockwise_edge = next_polygon
                        .get_clockwise_edge_containing_vertex(vertex_index)
                        .unwrap();
                    if last_counterclockwise_edge.0 != next_clockwise_edge.1
                        || last_counterclockwise_edge.1 != next_clockwise_edge.0
                    {
                        // The edges don't align; there's an obstacle here
                        polygons_including_obstacles.push(-1);
                    }
                    polygons_including_obstacles.push(polygon_index);
                }
                // The first element is included in the end again
                polygons_including_obstacles.remove(0);
                vertex.polygons = polygons_including_obstacles;
            }
            let mut polyanya_mesh = polyanya::Mesh::new(vertices, polygons);
            polyanya_mesh.bake();
            let path_mesh = PathMesh::from_polyanya_mesh(polyanya_mesh);

            commands.entity(child).despawn_recursive();
            commands.entity(parent).insert(path_meshes.add(path_mesh));
        }
    }
}

trait PolygonExtension {
    fn get_counterclockwise_edge_containing_vertex(&self, vertex: usize) -> Option<(usize, usize)>;
    fn get_clockwise_edge_containing_vertex(&self, vertex: usize) -> Option<(usize, usize)>;
}
impl PolygonExtension for polyanya::Polygon {
    fn get_counterclockwise_edge_containing_vertex(
        &self,
        vertex_index: usize,
    ) -> Option<(usize, usize)> {
        get_edges(self)
            // Our vertex will be the second of the line because the triangles are counterclockwise
            .find(|(_a, b)| *b == vertex_index)
    }
    fn get_clockwise_edge_containing_vertex(&self, vertex_index: usize) -> Option<(usize, usize)> {
        get_edges(self)
            // Our vertex will be the first of the line because the triangles are counterclockwise
            .find(|(a, _b)| *a == vertex_index)
    }
}

fn get_edges(polygon: &polyanya::Polygon) -> impl Iterator<Item = (usize, usize)> + '_ {
    polygon
        .vertices
        .iter()
        .chain(iter::once(&polygon.vertices[0]))
        .tuple_windows()
        .map(|(a, b)| (*a as usize, *b as usize))
}

fn get_global_transform(
    current_entity: Entity,
    parents: &Query<&Parent>,
    transforms: &Query<&Transform>,
) -> Transform {
    let own_transform = *transforms.get(current_entity).unwrap();
    match parents.get(current_entity).map(|parent| parent.get()) {
        Err(_) => own_transform,
        Ok(parent) => {
            let parent_transform = get_global_transform(parent, parents, transforms);
            parent_transform.mul_transform(own_transform)
        }
    }
}

fn get_mesh<'a>(
    children: &'a Children,
    meshes: &'a Assets<Mesh>,
    mesh_handles: &'a Query<&Handle<Mesh>>,
) -> (Entity, &'a Mesh) {
    let entity_handles: Vec<_> = children
        .iter()
        .filter_map(|entity| mesh_handles.get(*entity).ok().map(|mesh| (*entity, mesh)))
        .collect();
    assert_eq!(
        entity_handles.len(),
        1,
        "Collider must contain exactly one mesh, but found {}",
        entity_handles.len()
    );
    let (entity, mesh_handle) = entity_handles.first().unwrap();
    let mesh = meshes.get(mesh_handle).unwrap();
    assert_eq!(mesh.primitive_topology(), PrimitiveTopology::TriangleList);
    (*entity, mesh)
}
