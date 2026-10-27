//! Simulation layer: deterministic game logic. Runs in `FixedUpdate`.
//! This module must never spawn sprites/meshes — only mutate sim state.

pub mod setup;

use bevy::ecs::schedule::EntityWorldMut;
use bevy::prelude::*;

use crate::core::{
    map::NavigationGrid,
    units::{
        AttackCooldown, AttackOrder, Building, CombatDefinition, ConstructionSite, Dead,
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
            // schedule deterministically with a virtual clock.
            .insert_resource(TimeUpdateStrategy::ManualDuration(
                Duration::from_secs_f64(SIM_STEP_SECONDS),
            ))
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
                    commands
                        .entity(entity)
                        .insert((AttackOrder { target }, AttackCooldown::default()));
                    // Move into range if we are not already there. Approach one
                    // cell short of the target so units settle adjacent to it.
                    let distance = position.0.distance(target_position.0);
                    if distance > combat.range_for(unit.kind) {
                        let start = navigation.world_to_cell(position.0);
                        let goal_cell = navigation.world_to_cell(target_position.0);
                        let path = approach_path(&navigation, start, goal_cell);
                        commands.entity(entity).insert(MovePath(path.into()));
                    }
                }
            }
        }
    }
}

/// Path an attacking unit walks to reach a target standing on `goal`: aim for
/// the walkable neighbor cell closest to the attacker so it stops adjacent to
/// (and in range of) the target instead of walking onto its cell.
fn approach_path(navigation: &NavigationGrid, start: IVec2, goal: IVec2) -> Vec<IVec2> {
    const NEIGHBORS: [(i32, i32); 8] = [
        (-1, 0),
        (1, 0),
        (0, -1),
        (0, 1),
        (-1, -1),
        (-1, 1),
        (1, -1),
        (1, 1),
    ];
    let best = NEIGHBORS
        .iter()
        .map(|&(dx, dy)| goal + IVec2::new(dx, dy))
        .filter(|&cell| navigation.is_walkable(cell))
        .min_by_key(|&cell| (cell.x - start.x).unsigned_abs() + (cell.y - start.y).unsigned_abs());
    match best.or_else(|| navigation.nearest_walkable(goal)) {
        Some(cell) => navigation.find_path(start, cell).unwrap_or_default(),
        None => Vec::new(),
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
/// All state changes go through deferred `Commands`, so this system reads one
/// consistent snapshot per tick and never conflicts with itself mid-iteration.
fn attack_system(
    time: Res<Time<Fixed>>,
    combat: Res<CombatDefinition>,
    mut commands: Commands,
    attackers: Query<
        (
            Entity,
            &Unit,
            &WorldPosition,
            &Player,
            &AttackOrder,
            Option<&AttackCooldown>,
            Option<&Health>,
        ),
        With<Unit>,
    >,
    everyone: Query<(Entity, &Player, &WorldPosition, &Health), With<Unit>>,
) {
    let dt = time.delta_secs();
    // Snapshot every unit up front: a lethal strike inside this same step
    // must not stop another attacker from also landing its blow, and damage
    // application below never mutates state while a query is iterating.
    #[allow(clippy::type_complexity)]
    let snapshot: Vec<(Entity, u8, Vec2, f32)> = everyone
        .iter()
        .map(|(entity, player, position, health)| (entity, player.0, position.0, health.current))
        .collect();
    let living: Vec<(Entity, u8, Vec2)> = snapshot
        .iter()
        .filter(|(_, _, _, current)| *current > 0.0)
        .map(|&(entity, player, position, _)| (entity, player, position))
        .collect();

    // Accumulate damage per victim for this tick, then apply it in one pass.
    // Later strikes see the reduced health of earlier ones (damage is applied
    // in order), so a unit cannot absorb more total damage than it has HP.
    let mut current_health: Vec<(Entity, f32)> = snapshot
        .iter()
        .map(|&(entity, _, _, health)| (entity, health))
        .collect();
    let mut pending_damage: Vec<(Entity, f32)> = Vec::new();

    for (entity, unit, position, owner, order, cooldown, attacker_health) in &attackers {
        // A dead attacker no longer fights; death_system despawns it anyway.
        if attacker_health.is_some_and(|health| health.current <= 0.0) {
            continue;
        }
        // Drop stale orders: the target must still exist, be alive, and not
        // belong to the attacker's own player (no friendly fire).
        let target_row = living.iter().find(|(id, ..)| *id == order.target);
        let target_ok = target_row.is_some_and(|(_, player, _)| *player != owner.0);
        if !target_ok {
            commands.entity(entity).remove::<AttackOrder>();
            continue;
        }

        // Ensure the cooldown component exists even while still out of range,
        // so the first eligible step in range always lands a hit immediately.
        let distance = target_row.map_or(f32::INFINITY, |(_, _, target_position)| {
            position.0.distance(*target_position)
        });
        // A freshly assigned order starts ready: treat a missing component as
        // "0.0, ready to strike" instead of skipping this step.
        let ready = cooldown.is_none_or(|cooldown| cooldown.0 <= 0.0);
        if distance > combat.range_for(unit.kind) {
            if cooldown.is_none() {
                commands.entity(entity).insert(AttackCooldown(0.0));
            }
            continue;
        }
        if !ready {
            // Still on cooldown: tick it down (deferred write keeps the whole
            // system free of conflicting borrows).
            let remaining = cooldown.map_or(0.0, |cooldown| (cooldown.0 - dt).max(0.0));
            commands.entity(entity).insert(AttackCooldown(remaining));
            continue;
        }

        pending_damage.push((order.target, combat.damage_for(unit.kind)));
        commands
            .entity(entity)
            .insert(AttackCooldown(combat.cooldown_for(unit.kind)));
    }

    // Apply the accumulated damage in strike order and mark anyone who
    // reaches zero as dead; `death_system` despawns them.
    for (victim, amount) in pending_damage {
        let kills = match current_health.iter_mut().find(|(id, _)| *id == victim) {
            Some((_, health)) => {
                *health -= amount;
                *health <= 0.0
            }
            None => false,
        };
        if kills {
            commands.entity(victim).insert(Dead);
        } else {
            commands.entity(victim).queue(move |mut world: EntityWorldMut| {
                if let Some(mut health) = world.get_mut::<Health>() {
                    health.take_damage(amount);
                }
            });
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
        commands
            .entity(entity)
            .insert((AttackOrder { target }, AttackCooldown::default()));
        if position.0.distance(target_position.0) > combat.range_for(unit.kind) {
            let start = navigation.world_to_cell(position.0);
            let goal_cell = navigation.world_to_cell(target_position.0);
            let path = approach_path(&navigation, start, goal_cell);
            commands.entity(entity).insert(MovePath(path.into()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::units::KindCombat;
    use bevy::time::Virtual;
    use std::time::Duration;

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
        // clock resource is added here.
        app.add_plugins(MinimalPlugins)
            .insert_resource(Time::<Virtual>::default())
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
                FixedUpdate,
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
            // Prime the fixed timestep so the first `step()` below behaves
            // exactly like every later one (one event flush + one sim tick).
            app.update();
            app.world_mut()
                .resource_mut::<Time<Virtual>>()
                .advance_by(Duration::from_secs_f32(DT));
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
        /// Bevy's virtual clock advances at the END of `app.update()`, so the
        /// tick due from the PREVIOUS advance runs during the first call here:
        /// pending events flush through `apply_player_commands` on `Update`
        /// before the production fixed-step systems run against that state.
        /// The second advance arms exactly one more tick for the next `step()`.
        fn step(&mut self) {
            self.app.update();
            self.app
                .world_mut()
                .resource_mut::<Time<Virtual>>()
                .advance_by(Duration::from_secs_f32(DT));
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
            self.app
                .world()
                .get::<WorldPosition>(entity)
                .map(|p| p.0)
        }

        fn set_position(&mut self, entity: Entity, position: Vec2) {
            self.app
                .world_mut()
                .entity_mut(entity)
                .insert(WorldPosition(position));
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
        test.steps(3); // one overridden 40-damage strike kills the 40 HP peasant
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


