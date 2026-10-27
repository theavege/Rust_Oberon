//! Simulation layer: deterministic game logic. Runs in `FixedUpdate`.
//! This module must never spawn sprites/meshes — only mutate sim state.

pub mod setup;

use bevy::app::FixedMain;
use bevy::ecs::world::EntityWorldMut;
use bevy::prelude::*;
use bevy::time::TimeUpdateStrategy;
use std::collections::{HashMap, HashSet};
use std::time::Duration;

use crate::core::{
    map::NavigationGrid,
    units::{
        AttackCooldown, AttackOrder, Attacking, Building, CombatDefinition, ConstructionSite, Dead,
        HarvestOrder, Health, MovePath, Player, PlayerCommand, PlayerEconomy, ResourceNode,
        Selected, ShedDefinition, ShedPlacement, Unit, UnitKind, WorldPosition,
    },
};

/// Nominal fixed timestep of the simulation, in seconds (20 Hz).
pub const SIM_STEP_SECONDS: f64 = 0.05;

pub struct SimPlugin;

impl Plugin for SimPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, setup::spawn_initial_units)
            .add_event::<PlayerCommand>()
            .add_systems(Update, (apply_player_commands, update_shed_placement))
            // The simulation runs on `FixedMain`, driven at a fixed frequency
            // by the time plugin's catch-up loop. Tests drive the same
            // schedule deterministically with a manual virtual clock.
            .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
                SIM_STEP_SECONDS,
            )))
            .add_systems(
                FixedMain,
                (
                    opponent_ai_system,
                    movement_system.after(opponent_ai_system),
                    harvest_system.after(movement_system),
                    construction_system.after(harvest_system),
                    attack_system.after(construction_system),
                    death_system.after(attack_system),
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
    units: Query<(Entity, &Unit, Option<&Selected>, &WorldPosition), With<Player>>,
    nodes: Query<&WorldPosition, (With<ResourceNode>, Without<Unit>)>,
    enemy_targets: Query<(&Health, &Player, &WorldPosition), With<Unit>>,
    combat: Res<CombatDefinition>,
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
                            .remove::<AttackOrder>()
                            .remove::<Attacking>()
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
            PlayerCommand::Attack(target) => {
                // Validate the target: it must be a unit and not belong to the
                // issuing player (no friendly fire, no attacking scenery).
                let Ok((target_health, target_player, target_position)) = enemy_targets.get(target)
                else {
                    continue;
                };
                if target_player.0 == 0 || target_health.current <= 0.0 {
                    continue;
                }
                for (entity, unit, selected, position) in &units {
                    if selected.is_none() || !combat.can_attack(unit.kind) {
                        continue;
                    }
                    commands.entity(entity).insert((
                        AttackOrder { target },
                        AttackCooldown::default(),
                        Attacking,
                    ));
                    // Move into range if we are not already there. Approach one
                    // cell short of the target so units settle adjacent to it.
                    let distance = position.0.distance(target_position.0);
                    let range = combat.range_for(unit.kind);
                    if distance > range {
                        let start = navigation.world_to_cell(position.0);
                        let goal_cell = navigation.world_to_cell(target_position.0);
                        let path = approach_path(&navigation, start, goal_cell, range);
                        commands.entity(entity).insert(MovePath(path.into()));
                    }
                }
            }
        }
    }
}

/// Path an attacking unit walks toward a target standing on `goal`, aiming to
/// stop within `range` world units of it instead of walking onto its cell.
/// Walkable cells inside that radius are ranked by their *worst-case* distance
/// to any point of the target's footprint cell (the cell center plus half the
/// cell's diagonal extent): if even the far corner is within range, the unit
/// is guaranteed to be able to strike as soon as it arrives, no matter where
/// in the cell the target actually stands. Cells outside the radius are still
/// usable as fallbacks, ranked by their closest-point distance. When nothing
/// reachable exists near the target, path straight to the target itself.
fn approach_path(navigation: &NavigationGrid, start: IVec2, goal: IVec2, range: f32) -> Vec<IVec2> {
    let range_sq = range * range;
    let goal_center = NavigationGrid::cell_center(goal);

    let mut candidates: Vec<(f32, IVec2)> = Vec::new();
    for y in (goal.y - 5)..=(goal.y + 5) {
        for x in (goal.x - 5)..=(goal.x + 5) {
            let cell = IVec2::new(x, y);
            if !navigation.is_walkable(cell) {
                continue;
            }
            let center = NavigationGrid::cell_center(cell);
            let dx = (center.x - goal_center.x).abs();
            let dy = (center.y - goal_center.y).abs();
            // Worst-case distance to any point of the target's footprint cell:
            // the cell-center offset plus the half-extents (16, 8) of one tile.
            let far_x = dx + 16.0;
            let far_y = dy + 8.0;
            let far_sq = far_x * far_x + far_y * far_y;
            // Closest-approach distance to the footprint rectangle.
            let near_x = (dx - 16.0).max(0.0);
            let near_y = (dy - 8.0).max(0.0);
            let near_sq = near_x * near_x + near_y * near_y;
            let proximity =
                (start.x - cell.x).unsigned_abs() as f32 + (start.y - cell.y).unsigned_abs() as f32;
            let score = if far_sq <= range_sq {
                // In range from anywhere inside this cell: prefer guaranteed
                // cells (by worst case, then by proximity to the attacker).
                far_sq + 0.001 * proximity
            } else {
                // Fallback cells rank behind every guaranteed cell.
                range_sq + 1.0 + near_sq + 0.001 * proximity
            };
            candidates.push((score, cell));
        }
    }
    let destination = candidates
        .into_iter()
        .min_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, cell)| cell)
        .or_else(|| navigation.nearest_walkable(goal))
        .unwrap_or(goal);
    navigation.find_path(start, destination).unwrap_or_default()
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
    units: Query<&WorldPosition, (With<Unit>, With<Player>)>,
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
    mut workers: Query<(Entity, &WorldPosition, &mut HarvestOrder), (With<Unit>, With<Player>)>,
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
    mut q: Query<(&mut WorldPosition, &Unit, &mut MovePath), With<Unit>>,
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

/// Fixed-step combat resolution: units with an `AttackOrder` move toward their
/// target (handled by `movement_system`) and strike whenever the target is in
/// range and the cooldown has elapsed. Invalid or dead targets are dropped.
/// The system reads one consistent snapshot per tick, decides every strike up
/// front, then applies damage so a second hit inside the same tick sees the
/// reduced health. Structural changes go through deferred `Commands`.
///
/// The two mutable queries are deliberately disjoint so Bevy's scheduler can
/// prove they never touch the same entity: fighters are exactly the units
/// carrying the `Attacking` marker — an invariant every order assignment
/// maintains by inserting the marker together with the order — and pure
/// victims are everything else (`Without<Attacking>`). A freshly ordered
/// attacker is already marked, so it strikes on the very next fixed step.
#[allow(clippy::type_complexity)]
fn attack_system(
    time: Res<Time<Fixed>>,
    combat: Res<CombatDefinition>,
    mut commands: Commands,
    // Fighters — units marked `Attacking` — together with their cooldown
    // clocks and health. Cooldown and attacker-health writes land directly
    // here; the decision pass snapshots from this query before mutating
    // through it, so a fighter struck by several enemies accumulates one
    // merged in-place write per target.
    mut fighters: Query<
        (
            Entity,
            &Unit,
            &WorldPosition,
            &Player,
            &AttackOrder,
            Option<&mut AttackCooldown>,
            &mut Health,
        ),
        (With<Unit>, With<Attacking>),
    >,
    // Pure victims — every unit not marked as attacking this tick. Their
    // `Health` is disjoint from the fighter query above
    // (`Without<Attacking>`), so damage lands in place and a second strike
    // inside the same tick sees the reduced health. Victims are read-only
    // here; their snapshot rows come from this same query.
    mut victims: Query<
        (Entity, &Player, &WorldPosition, &Health),
        (With<Unit>, Without<Attacking>),
    >,
) {
    // The cooldown is a countdown clock, so it must advance by exactly one
    // fixed step per tick. `Time<Fixed>::delta_secs` reports the accumulated
    // real-time delta of the whole catch-up loop (and drifts with the wall
    // clock under the virtual clock in tests), which would make strikes
    // non-deterministic; the timestep itself is the production cadence.
    let dt = time.timestep().as_secs_f32();
    // Snapshot every unit up front: a lethal strike inside this same step
    // must not stop another attacker from also landing its blow, and the
    // damage pass below never re-reads state while a query is iterating.
    // Victims (units without the `Attacking` marker) come from the read-only
    // side of the split; fighters snapshot their own rows further below.
    #[allow(clippy::type_complexity)]
    let mut snapshot: Vec<(Entity, u8, Vec2, f32)> = Vec::new();
    for (entity, player, position, health) in victims.iter() {
        snapshot.push((entity, player.0, position.0, health.current));
    }
    // Map from entity to its snapshotted row, for O(1) target lookups. The
    // health column freezes the HP seen at the top of the tick; later
    // in-place writes are tracked through `victim_health`, so staleness
    // checks use this value only to decide whether an order started the
    // tick against a living enemy.
    #[allow(clippy::type_complexity)]
    let rows: HashMap<Entity, (u8, Vec2, f32)> = snapshot
        .iter()
        .map(|&(entity, player, position, health)| (entity, (player, position, health)))
        .collect();
    std::mem::drop(snapshot);

    // Snapshot the fighters (units marked `Attacking`) together with their
    // live health and cooldown clocks before any mutable pass runs; the
    // decision loop below works purely on this data, and every write goes
    // back through the fighter query afterwards.
    #[allow(clippy::type_complexity)]
    let fighter_snapshot: Vec<(Entity, UnitKind, Vec2, u8, Entity, f32, Option<f32>)> = fighters
        .iter()
        .map(
            |(entity, unit, position, owner, order, cooldown, health)| {
                (
                    entity,
                    unit.kind,
                    position.0,
                    owner.0,
                    order.target,
                    health.current,
                    cooldown.map(|cooldown| cooldown.0),
                )
            },
        )
        .collect();

    // Decide every strike for this tick first, then apply them in one pass.
    let mut pending_damage: Vec<(Entity, f32)> = Vec::new();
    let mut stale_orders: Vec<Entity> = Vec::new();
    let mut strike_cooldowns: Vec<(Entity, f32)> = Vec::new();
    // Fighters that take damage this tick, merged per target so several
    // blows landing within one tick accumulate into a single in-place write.
    let mut fighter_damage: HashMap<Entity, f32> = HashMap::new();
    // Cooldown bookkeeping for attackers that are still out of range and do
    // not carry the component yet — inserting it here would overlap the
    // mutable fighter query above, so these few writes stay deferred.
    let mut ensure_cooldown: Vec<Entity> = Vec::new();

    for &(entity, kind, position, owner, target, health, cooldown) in &fighter_snapshot {
        // Drop stale orders: the target must still exist, be alive, and not
        // belong to the attacker's own player (no friendly fire).
        let target_row = rows.get(&target);
        let target_ok = target_row
            .is_some_and(|(player, _, health, _)| *player != owner && *health > 0.0);
        if !target_ok {
            stale_orders.push(entity);
            continue;
        }

        let distance = target_row.map_or(f32::INFINITY, |(_, target_position, _, _)| {
            position.distance(*target_position)
        });
        // A freshly assigned order starts ready: treat a missing component as
        // "0.0, ready to strike" instead of skipping this step.
        let ready = cooldown.is_none_or(|value| value <= 0.0);
        if distance > combat.range_for(kind) {
            if cooldown.is_none() {
                ensure_cooldown.push(entity);
            }
            continue;
        }
        if !ready {
            // Still on cooldown: tick it down by exactly one fixed step.
            let remaining = cooldown.map_or(0.0, |value| (value - dt).max(0.0));
            strike_cooldowns.push((entity, remaining));
            continue;
        }

        let amount = combat.damage_for(kind);
        if rows.get(&target).is_some_and(|(_, _, _, attacking)| *attacking) {
            // The victim is a fighter: merge its blows and apply them
            // in place through the fighter query below.
            let taken = fighter_damage.entry(target).or_insert(0.0);
            *taken = (*taken + amount).min(health.max(0.0));
        } else {
            pending_damage.push((target, amount));
        }
        strike_cooldowns.push((entity, combat.cooldown_for(kind)));
    }

    // Units that hold an attack order always own their cooldown component
    // (every order assignment inserts one), so cooldown writes land directly
    // on the fighter query; anything unexpected falls back to a deferred
    // insert. Fighters whose order was removed this tick (stale targets) take
    // their cooldown through the same deferred path instead, because applying
    // structural edits mid-system is unsafe.
    let demoted: HashSet<Entity> = stale_orders.iter().copied().collect();
    for entity in &stale_orders {
        commands.entity(*entity).remove::<AttackOrder>();
    }
    // Sync the `Attacking` marker with the orders decided above: every
    // fighter keeps it while its target stays valid, and units whose order
    // was dropped this tick lose it immediately. Removals apply before
    // insertions during command flushing, so an attacker that loses its
    // order this tick is never left marked — which keeps the victim query's
    // disjointness provable to the scheduler on the next tick.
    for &(entity, _, _, owner, target, _, _) in &fighter_snapshot {
        if !demoted.contains(&entity)
            && rows.get(&target).is_some_and(|(player, _, health, _)| {
                *player != owner && *health > 0.0
            })
        {
            commands.entity(entity).insert(Attacking);
        }
    }
    for entity in &demoted {
        commands.entity(*entity).remove::<Attacking>();
    }
    for entity in &ensure_cooldown {
        commands.entity(*entity).insert(AttackCooldown(0.0));
    }
    for (entity, value) in strike_cooldowns {
        if demoted.contains(&entity) {
            commands.entity(entity).insert(AttackCooldown(value));
        } else if let Ok((_, _, _, _, _, Some(mut cooldown), _)) = fighters.get_mut(entity) {
            cooldown.0 = value;
        } else {
            commands.entity(entity).insert(AttackCooldown(value));
        }
    }

    // Apply the accumulated damage in strike order and mark anyone who
    // reaches zero as dead; `death_system` despawns them. Pure victims are
    // edited in place through the disjoint victim query, so a second strike
    // inside this same tick sees the reduced health. Fighters are also
    // damaged in place — through the fighter query, one merged write per
    // target — because their `Health` cannot be touched through the victim
    // query without aliasing.
    for (victim, amount) in pending_damage {
        if let Ok(mut health) = victim_health.get_mut(victim) {
            if health.take_damage(amount) {
                commands.entity(victim).insert(Dead);
            }
            running_health.insert(victim, health.current);
        }
        // A victim that vanished between the snapshot and now simply takes
        // no blow; `take_damage` clamps at zero so two strikes can never
        // overkill past the unit's remaining health.
    }
    for (victim, total) in fighter_damage {
        if let Ok((_, _, _, _, _, _, mut health)) = fighters.get_mut(victim) {
            if health.take_damage(total) {
                commands.entity(victim).insert(Dead);
            }
        }
    }
}

/// Remove units that reached zero health inside the fixed-step simulation,
/// independently of rendering.
fn death_system(mut commands: Commands, dead: Query<Entity, With<Dead>>) {
    for entity in &dead {
        commands.entity(entity).despawn();
    }
}

/// Basic opponent: enemy (player 1) units pick the closest living unit of
/// another player as their target and move into range when needed. The regular
/// `attack_system` then resolves these orders with the same cooldown and damage
/// rules as player attacks, keeping the AI purely simulation-side. Player-0
/// units are never auto-ordered; they only fight on explicit commands.
fn opponent_ai_system(
    combat: Res<CombatDefinition>,
    navigation: Res<NavigationGrid>,
    mut commands: Commands,
    opponents: Query<
        (Entity, &Unit, &WorldPosition, Option<&AttackOrder>),
        (With<Player>, With<Health>),
    >,
    victims: Query<(Entity, &Player, &WorldPosition, &Health), With<Unit>>,
    players: Query<&Player>,
) {
    for (entity, unit, position, order) in &opponents {
        let Ok(owner) = players.get(entity) else {
            continue;
        };
        if owner.0 != 1 || !combat.can_attack(unit.kind) {
            continue;
        }
        // Already ordered against a living unit: nothing to reassign.
        if order.is_some_and(|order| {
            victims
                .get(order.target)
                .is_ok_and(|(_, _, _, health)| health.current > 0.0)
        }) {
            continue;
        }
        let Some((target, target_position)) = victims
            .iter()
            .filter(|(_, victim_player, _, health)| {
                victim_player.0 != owner.0 && health.current > 0.0
            })
            .min_by(|a, b| {
                a.2 .0
                    .distance(position.0)
                    .total_cmp(&b.2 .0.distance(position.0))
            })
            .map(|(target, _, target_position, _)| (target, *target_position))
        else {
            continue;
        };
        commands.entity(entity).insert((
            AttackOrder { target },
            AttackCooldown::default(),
            Attacking,
        ));
        if position.0.distance(target_position.0) > combat.range_for(unit.kind) {
            let start = navigation.world_to_cell(position.0);
            let goal_cell = navigation.world_to_cell(target_position.0);
            let path = approach_path(&navigation, start, goal_cell, combat.range_for(unit.kind));
            commands.entity(entity).insert(MovePath(path.into()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::units::KindCombat;
    use bevy::time::Virtual;

    /// Nominal fixed-timestep duration in seconds. The virtual clock is
    /// advanced by exactly this amount per `step()`, so every call runs one
    /// full production `FixedUpdate` tick — fully deterministic.
    const DT: f32 = 0.05;

    /// Small walkable open area so units can path and approach targets.
    const TEST_MAP: u32 = 64;

    /// Build a bare Bevy app that runs exactly the production schedules
    /// (same systems, same ordering) without any rendering, window or real
    /// clock. Time uses a virtual clock, so each `step()` below advances the
    /// simulation by precisely one fixed tick of `DT` seconds.
    fn combat_test_app(combat: CombatDefinition) -> App {
        let mut app = App::new();
        // `MinimalPlugins` already provides `TimePlugin`, so only the virtual
        // clock and the manual update strategy are added here. Without a
        // strategy the virtual clock advances by wall-clock time, which makes
        // fixed ticks (and cooldowns) non-deterministic in tests.
        app.add_plugins(MinimalPlugins)
            .insert_resource(Time::<Virtual>::default())
            .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
                SIM_STEP_SECONDS,
            )))
            .insert_resource(open_grid())
            .insert_resource(combat)
            .init_resource::<PlayerEconomy>()
            .init_resource::<ShedPlacement>()
            .init_resource::<ShedDefinition>()
            .add_event::<PlayerCommand>()
            // Player commands are applied on `Update` in the real game; tests
            // drive the same schedule so `send(...)` + `step()` behaves like
            // production (commands flush before the fixed combat step runs).
            .add_systems(Update, apply_player_commands)
            .add_systems(
                FixedMain,
                (
                    opponent_ai_system,
                    movement_system.after(opponent_ai_system),
                    harvest_system.after(movement_system),
                    construction_system.after(harvest_system),
                    attack_system.after(construction_system),
                    death_system.after(attack_system),
                ),
            );
        // Run at exactly the nominal timestep so no accumulator drift builds
        // up between steps.
        app.world_mut()
            .resource_mut::<Time<Fixed>>()
            .set_timestep_seconds(DT as f64);
        app
    }

    fn open_grid() -> NavigationGrid {
        NavigationGrid::open_field(TEST_MAP)
    }

    struct TestApp {
        app: App,
    }

    impl TestApp {
        fn new() -> Self {
            Self::with_combat(CombatDefinition::default())
        }

        fn with_combat(combat: CombatDefinition) -> Self {
            let mut app = combat_test_app(combat);
            // Prime the loop once. With `TimeUpdateStrategy::ManualDuration`
            // every update has a delta of exactly one timestep, so this first
            // `app.update()` runs one full fixed tick; tests account for it in
            // their step budgets (e.g. "strike after two steps" = prime + 1).
            app.update();
            TestApp { app }
        }

        fn spawn_unit(
            &mut self,
            kind: UnitKind,
            owner: u8,
            position: Vec2,
            selected: bool,
        ) -> Entity {
            let entity = self
                .app
                .world_mut()
                .spawn((
                    Unit { kind },
                    Player(owner),
                    Health {
                        current: kind.max_hp(),
                        max: kind.max_hp(),
                    },
                    MovePath::default(),
                    WorldPosition(position),
                ))
                .id();
            if selected {
                self.app.world_mut().entity_mut(entity).insert(Selected);
            }
            entity
        }

        fn send(&mut self, command: PlayerCommand) {
            self.app.world_mut().send_event(command);
        }

        /// Advance the simulation by exactly one fixed step of `DT` seconds.
        /// The time plugin runs with `TimeUpdateStrategy::ManualDuration(DT)`,
        /// so each `app.update()` produces a virtual delta of precisely `DT`
        /// and the fixed-step driver executes exactly one `FixedMain` tick.
        /// Pending events flush through `apply_player_commands` on `Update`
        /// before the fixed combat systems run, matching production ordering.
        fn step(&mut self) {
            self.app.update();
        }

        fn steps(&mut self, count: usize) {
            for _ in 0..count {
                self.step();
            }
        }

        fn health(&self, entity: Entity) -> Option<f32> {
            self.app
                .world()
                .get::<Health>(entity)
                .map(|health| health.current)
        }

        fn has_attack_order(&self, entity: Entity) -> bool {
            self.app.world().get::<AttackOrder>(entity).is_some()
        }

        fn attack_target(&self, entity: Entity) -> Option<Entity> {
            self.app
                .world()
                .get::<AttackOrder>(entity)
                .map(|order| order.target)
        }

        fn cooldown(&self, entity: Entity) -> Option<f32> {
            self.app.world().get::<AttackCooldown>(entity).map(|c| c.0)
        }

        fn position(&self, entity: Entity) -> Option<Vec2> {
            self.app.world().get::<WorldPosition>(entity).map(|p| p.0)
        }

        fn set_position(&mut self, entity: Entity, position: Vec2) {
            self.app
                .world_mut()
                .entity_mut(entity)
                .insert(WorldPosition(position));
            // A teleported unit may now stand nowhere near its queued route;
            // drop any stale path so movement does not drag it back.
            if let Some(mut path) = self.app.world_mut().get_mut::<MovePath>(entity) {
                path.0.clear();
            }
        }

        fn set_health(&mut self, entity: Entity, current: f32) {
            let max = self.health(entity).unwrap_or(current);
            self.app
                .world_mut()
                .entity_mut(entity)
                .insert(Health { current, max });
        }

        fn mark_dead(&mut self, entity: Entity) {
            self.app.world_mut().entity_mut(entity).insert(Dead);
        }

        fn despawn(&mut self, entity: Entity) {
            self.app.world_mut().despawn(entity);
        }

        fn alive_count(&mut self) -> usize {
            self.app
                .world_mut()
                .query_filtered::<Entity, With<Unit>>()
                .iter(self.app.world())
                .count()
        }
    }

    #[test]
    fn damage_kills_at_zero_health_and_despawns_the_unit() {
        let mut test = TestApp::new();
        let attacker = test.spawn_unit(UnitKind::Warrior, 0, Vec2::ZERO, true);
        let target = test.spawn_unit(UnitKind::Peasant, 1, Vec2::X * 10.0, false);

        test.send(PlayerCommand::Attack(target));
        // Step 1 applies the order and walks the attacker into range; step 2
        // lands hit one once the cooldown tick has elapsed.
        test.steps(2);
        assert_eq!(test.health(target), Some(40.0 - 20.0));
        assert!(test.alive_count() == 2, "one hit does not kill");

        test.steps(20); // hit two brings the 40 HP peasant to zero.
        assert!(
            test.health(target).is_none(),
            "dead units are removed by the fixed-step sim"
        );
        assert!(
            !test.has_attack_order(attacker),
            "orders against dead targets drop"
        );
    }

    #[test]
    fn attacks_only_land_within_range() {
        let mut test = TestApp::new();
        // Warrior melee range is 30.0; place the target just outside it.
        let attacker = test.spawn_unit(UnitKind::Warrior, 0, Vec2::ZERO, true);
        let target = test.spawn_unit(UnitKind::Warrior, 1, Vec2::X * 40.0, false);

        test.send(PlayerCommand::Attack(target));
        test.steps(60); // three full cooldown periods while out of reach
        assert_eq!(test.health(target), Some(100.0), "no damage beyond range");
        assert!(
            test.has_attack_order(attacker),
            "order persists while pursuing"
        );

        // Teleport into range: the next eligible step must deal damage.
        test.set_position(attacker, Vec2::X * 20.0);
        test.steps(2);
        assert_eq!(test.health(target), Some(80.0), "damage once in range");
    }

    #[test]
    fn attacks_follow_the_configured_cooldown() {
        let mut test = TestApp::new();
        let attacker = test.spawn_unit(UnitKind::Warrior, 0, Vec2::ZERO, true);
        let target = test.spawn_unit(UnitKind::Warrior, 1, Vec2::X * 10.0, false);

        test.send(PlayerCommand::Attack(target));
        test.steps(2); // first strike: 100 -> 80
        assert_eq!(test.health(target), Some(80.0));
        assert!(test.cooldown(attacker).is_some());

        // Half a cooldown later (0.5 s at a 1.0 s cadence): still only one hit.
        test.steps(10);
        assert_eq!(test.health(target), Some(80.0), "cooldown gates strikes");

        // After a full cooldown elapses: second hit lands.
        test.steps(10);
        assert_eq!(test.health(target), Some(60.0));
    }

    #[test]
    fn cooldown_scale_is_data_driven() {
        let mut test = TestApp::with_combat(CombatDefinition {
            cooldown_scale: 2.0,
            ..default()
        });
        let _attacker = test.spawn_unit(UnitKind::Warrior, 0, Vec2::ZERO, true);
        let target = test.spawn_unit(UnitKind::Warrior, 1, Vec2::X * 10.0, false);

        test.send(PlayerCommand::Attack(target));
        test.steps(2); // first strike at 20 damage
        test.steps(20); // one default cooldown would have allowed a second hit
        assert_eq!(
            test.health(target),
            Some(80.0),
            "scaled cooldown halves rate"
        );
        test.steps(20); // the doubled cooldown (2 s) now allows the second hit
        assert_eq!(test.health(target), Some(60.0));
    }

    #[test]
    fn per_kind_damage_override_changes_lethality() {
        let mut test = TestApp::with_combat(CombatDefinition {
            warrior: KindCombat {
                damage: Some(40.0),
                ..default()
            },
            ..default()
        });
        let _attacker = test.spawn_unit(UnitKind::Warrior, 0, Vec2::ZERO, true);
        let target = test.spawn_unit(UnitKind::Peasant, 1, Vec2::X * 10.0, false);

        test.send(PlayerCommand::Attack(target));
        test.steps(4); // one overridden 40-damage strike kills the 40 HP peasant
        assert!(
            test.health(target).is_none(),
            "a single lethal blow must kill and despawn"
        );
    }

    #[test]
    fn move_order_cancels_the_attack_order() {
        let mut test = TestApp::new();
        let attacker = test.spawn_unit(UnitKind::Warrior, 0, Vec2::ZERO, true);
        let target = test.spawn_unit(UnitKind::Warrior, 1, Vec2::X * 10.0, false);

        test.send(PlayerCommand::Attack(target));
        test.step();
        assert!(test.has_attack_order(attacker));

        test.send(PlayerCommand::MoveSelected(Vec2::new(32.0, -8.0)));
        test.step();
        assert!(
            !test.has_attack_order(attacker),
            "move cancels attack order"
        );
        test.steps(40);
        assert_eq!(
            test.health(target),
            Some(100.0),
            "cancelled attacker never strikes"
        );
    }

    #[test]
    fn invalid_targets_are_rejected_or_dropped() {
        let mut test = TestApp::new();
        let attacker = test.spawn_unit(UnitKind::Warrior, 0, Vec2::ZERO, true);
        let friendly = test.spawn_unit(UnitKind::Archer, 0, Vec2::X * 10.0, false);

        // Friendly fire and non-unit entities are refused outright.
        test.send(PlayerCommand::Attack(friendly));
        test.send(PlayerCommand::Attack(Entity::from_raw(9_999)));
        test.steps(4);
        assert!(
            !test.has_attack_order(attacker),
            "invalid orders never attach"
        );
        assert_eq!(test.health(friendly), Some(60.0));

        // An order whose target despawns mid-combat is dropped cleanly.
        let victim = test.spawn_unit(UnitKind::Peasant, 1, Vec2::X * 10.0, false);
        test.send(PlayerCommand::Attack(victim));
        test.step();
        assert_eq!(test.attack_target(attacker), Some(victim));
        test.despawn(victim);
        test.step();
        assert!(
            !test.has_attack_order(attacker),
            "stale orders drop when target vanishes"
        );
    }

    #[test]
    fn non_attacking_units_ignore_attack_orders() {
        let mut test = TestApp::new();
        let peasant = test.spawn_unit(UnitKind::Peasant, 0, Vec2::ZERO, true);
        let target = test.spawn_unit(UnitKind::Warrior, 1, Vec2::X * 10.0, false);

        test.send(PlayerCommand::Attack(target));
        test.steps(10);
        assert!(!test.has_attack_order(peasant), "peasants cannot fight");
        assert_eq!(test.health(target), Some(100.0));
    }

    #[test]
    fn opponent_ai_attacks_back_and_retargets_after_death() {
        let mut test = TestApp::new();
        // Archer (range 90) vs enemy Warrior (melee 30): the enemy picks the
        // closest player-0 unit on its own and starts attacking it.
        let archer = test.spawn_unit(UnitKind::Archer, 0, Vec2::ZERO, false);
        let enemy = test.spawn_unit(UnitKind::Warrior, 1, Vec2::X * 25.0, false);

        test.steps(2); // AI assigns the order on the first step, strikes after cooldown
        assert_eq!(test.attack_target(enemy), Some(archer));
        assert_eq!(test.health(archer), Some(60.0 - 20.0));

        // Kill the archer through the sim; the enemy must drop the stale order.
        test.set_health(archer, 0.0);
        test.mark_dead(archer);
        test.steps(3);
        assert!(
            test.health(archer).is_none(),
            "death_system removes the corpse"
        );
        assert_ne!(
            test.attack_target(enemy),
            Some(archer),
            "AI drops orders against dead targets"
        );
        assert_eq!(test.alive_count(), 1);
    }

    #[test]
    fn opponent_ai_moves_into_range_of_far_targets() {
        let mut test = TestApp::new();
        let victim = test.spawn_unit(UnitKind::Peasant, 0, Vec2::ZERO, false);
        let enemy = test.spawn_unit(UnitKind::Warrior, 1, Vec2::X * 200.0, false);

        test.steps(1);
        assert_eq!(test.attack_target(enemy), Some(victim));
        let moved = test.position(enemy).unwrap();
        test.steps(20);
        assert!(
            test.position(enemy).unwrap().distance(moved) > 1.0,
            "enemy walks toward its target"
        );
    }
}
