//! Simulation layer: deterministic game logic. Runs in `FixedUpdate`.
//! This module must never spawn sprites/meshes — only mutate sim state.

pub mod setup;

use bevy::prelude::*;

use crate::core::{
    map::NavigationGrid,
    units::{
        Building, ConstructionSite, HarvestOrder, MovePath, Player, PlayerCommand, PlayerEconomy,
        ResourceNode, Selected, ShedDefinition, ShedPlacement, Unit, UnitKind, WorldPosition,
    },
};

pub struct SimPlugin;

impl Plugin for SimPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, setup::spawn_initial_units)
            .add_event::<PlayerCommand>()
            .add_systems(Update, (apply_player_commands, update_shed_placement))
            .add_systems(
                FixedUpdate,
                (
                    movement_system,
                    harvest_system.after(movement_system),
                    construction_system.after(harvest_system),
                ),
            );
    }
}

fn apply_player_commands(
    mut messages: EventReader<PlayerCommand>,
    mut commands: Commands,
    mut navigation: ResMut<NavigationGrid>,
    mut economy: ResMut<PlayerEconomy>,
    shed: Res<ShedDefinition>,
    units: Query<
        (Entity, &Unit, Option<&Selected>, &WorldPosition),
        (With<Player>, Without<ResourceNode>),
    >,
    nodes: Query<&WorldPosition, (With<ResourceNode>, Without<Unit>)>,
) {
    for message in messages.read() {
        match *message {
            PlayerCommand::Select(target) => {
                for (entity, _, selected, _) in &units {
                    if selected.is_some() {
                        commands.entity(entity).remove::<Selected>();
                    }
                }
                if let Some(entity) = target {
                    commands.entity(entity).insert(Selected);
                }
            }
            PlayerCommand::MoveSelected(destination) => {
                let requested = navigation.world_to_cell(destination);
                let goal = navigation.nearest_walkable(requested);
                for (entity, _, selected, position) in &units {
                    if selected.is_some() {
                        let start = navigation.world_to_cell(position.0);
                        let path = goal
                            .and_then(|goal| navigation.find_path(start, goal))
                            .unwrap_or_default();
                        commands
                            .entity(entity)
                            .remove::<HarvestOrder>()
                            .insert(MovePath(path.into()));
                    }
                }
            }
            PlayerCommand::Harvest(target) => {
                let Ok(node_position) = nodes.get(target) else {
                    continue;
                };
                let goal = navigation.nearest_walkable(navigation.world_to_cell(node_position.0));
                for (entity, unit, selected, position) in &units {
                    if selected.is_none() || unit.kind != UnitKind::Peasant {
                        continue;
                    }
                    let start = navigation.world_to_cell(position.0);
                    let path = goal
                        .and_then(|goal| navigation.find_path(start, goal))
                        .unwrap_or_default();
                    commands.entity(entity).insert((
                        MovePath(path.into()),
                        HarvestOrder {
                            target,
                            elapsed: 0.0,
                        },
                    ));
                }
            }
            PlayerCommand::PlaceShed(origin) => {
                let occupied_cells: Vec<IVec2> = units
                    .iter()
                    .map(|(_, _, _, position)| navigation.world_to_cell(position.0))
                    .collect();
                if economy.wood < shed.wood_cost
                    || !can_place_shed(&navigation, origin, shed.footprint, &occupied_cells)
                    || !navigation.reserve_footprint(origin, shed.footprint)
                {
                    continue;
                }

                economy.wood -= shed.wood_cost;
                commands.spawn((
                    Building {
                        owner: 0,
                        footprint: shed.footprint,
                    },
                    ConstructionSite {
                        elapsed: 0.0,
                        duration: shed.construction_seconds.max(0.1),
                    },
                    WorldPosition(footprint_center(origin, shed.footprint)),
                ));
            }
        }
    }
}

fn can_place_shed(
    navigation: &NavigationGrid,
    origin: IVec2,
    footprint: IVec2,
    occupied_cells: &[IVec2],
) -> bool {
    navigation.can_reserve_footprint(origin, footprint)
        && !occupied_cells
            .iter()
            .any(|&cell| footprint_contains(origin, footprint, cell))
}

fn footprint_contains(origin: IVec2, footprint: IVec2, cell: IVec2) -> bool {
    cell.x >= origin.x
        && cell.y >= origin.y
        && cell.x < origin.x + footprint.x
        && cell.y < origin.y + footprint.y
}

fn footprint_center(origin: IVec2, footprint: IVec2) -> Vec2 {
    let offset = Vec2::new(
        (footprint.x - 1) as f32 / 2.0,
        (footprint.y - 1) as f32 / 2.0,
    );
    NavigationGrid::cell_center(origin)
        + Vec2::new(16.0 * (offset.x - offset.y), -8.0 * (offset.x + offset.y))
}

fn update_shed_placement(
    navigation: Res<NavigationGrid>,
    economy: Res<PlayerEconomy>,
    shed: Res<ShedDefinition>,
    units: Query<&WorldPosition, (With<Unit>, With<Player>, Without<ResourceNode>)>,
    mut placement: ResMut<ShedPlacement>,
) {
    if !placement.active {
        placement.valid = false;
        return;
    }
    let occupied_cells: Vec<IVec2> = units
        .iter()
        .map(|position| navigation.world_to_cell(position.0))
        .collect();
    placement.valid = economy.wood >= shed.wood_cost
        && can_place_shed(
            &navigation,
            placement.origin,
            shed.footprint,
            &occupied_cells,
        );
}

fn harvest_system(
    time: Res<Time<Fixed>>,
    mut economy: ResMut<PlayerEconomy>,
    mut commands: Commands,
    mut workers: Query<
        (Entity, &WorldPosition, &mut HarvestOrder),
        (With<Unit>, With<Player>, Without<ResourceNode>),
    >,
    mut nodes: Query<(&WorldPosition, &mut ResourceNode), Without<Unit>>,
) {
    for (worker, position, mut order) in &mut workers {
        let Ok((node_position, mut node)) = nodes.get_mut(order.target) else {
            commands.entity(worker).remove::<HarvestOrder>();
            continue;
        };
        if position.0.distance(node_position.0) > 42.0 {
            continue;
        }

        order.elapsed += time.delta_secs();
        while order.elapsed >= 1.0 && node.wood > 0 {
            order.elapsed -= 1.0;
            node.wood -= 1;
            economy.wood += 1;
        }
        if node.wood == 0 {
            commands.entity(order.target).despawn();
            commands.entity(worker).remove::<HarvestOrder>();
        }
    }
}

fn construction_system(
    time: Res<Time<Fixed>>,
    mut commands: Commands,
    mut sites: Query<(Entity, &mut ConstructionSite)>,
) {
    for (entity, mut site) in &mut sites {
        site.elapsed += time.delta_secs();
        if site.elapsed >= site.duration {
            commands.entity(entity).remove::<ConstructionSite>();
        }
    }
}

/// Move units toward their current destination in isometric world space.
fn movement_system(
    time: Res<Time<Fixed>>,
    navigation: Res<NavigationGrid>,
    mut q: Query<(&mut WorldPosition, &Unit, &mut MovePath), With<Player>>,
) {
    let dt = time.delta_secs();
    for (mut position, unit, mut path) in q.iter_mut() {
        let Some(&next_cell) = path.0.front() else {
            continue;
        };
        let pos = position.0;
        let current_cell = navigation.world_to_cell(pos);
        let diagonal = current_cell.x != next_cell.x && current_cell.y != next_cell.y;
        if !navigation.is_walkable(next_cell)
            || (diagonal
                && (!navigation.is_walkable(IVec2::new(next_cell.x, current_cell.y))
                    || !navigation.is_walkable(IVec2::new(current_cell.x, next_cell.y))))
        {
            path.0.clear();
            continue;
        }
        let target = NavigationGrid::cell_center(next_cell);
        let delta = target - pos;
        let dist = delta.length();
        let step = unit.kind.speed() * 32.0 * dt;
        if dist <= step {
            position.0 = target;
            path.0.pop_front();
            continue;
        }
        if dist < 0.01 {
            continue;
        }
        let next = pos + delta.normalize() * step;
        position.0 = next;
    }
}
