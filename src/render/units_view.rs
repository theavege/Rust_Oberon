//! Project simulation-owned unit positions and selection state into visuals.

use bevy::prelude::*;

use crate::core::units::{
    Player, PlayerCommand, PlayerEconomy, ResourceNode, Selected, Unit, WorldPosition,
};

/// Marker for unit visuals so we can find unsynced entities.
#[derive(Component)]
pub struct UnitSprite;

pub fn unit_input(
    buttons: Res<ButtonInput<MouseButton>>,
    windows: Query<&Window>,
    cameras: Query<(&Camera, &GlobalTransform), With<Camera2d>>,
    mut commands: EventWriter<PlayerCommand>,
    units: Query<(Entity, &Transform), (With<Unit>, With<Player>)>,
    resource_nodes: Query<(Entity, &Transform), With<ResourceNode>>,
) {
    let Ok(window) = windows.single() else { return };
    let Some(cursor) = window.cursor_position() else {
        return;
    };
    let Ok((camera, camera_transform)) = cameras.single() else {
        return;
    };
    let Ok(world_pos) = camera.viewport_to_world_2d(camera_transform, cursor) else {
        return;
    };

    if buttons.just_pressed(MouseButton::Left) {
        let target = units
            .iter()
            .map(|(entity, transform)| {
                (entity, transform.translation.truncate().distance(world_pos))
            })
            .filter(|(_, distance)| *distance <= 22.0)
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(entity, _)| entity);
        commands.write(PlayerCommand::Select(target));
    }
    if buttons.just_pressed(MouseButton::Right) {
        let resource = resource_nodes
            .iter()
            .map(|(entity, transform)| {
                (entity, transform.translation.truncate().distance(world_pos))
            })
            .filter(|(_, distance)| *distance <= 52.0)
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(entity, _)| entity);
        if let Some(resource) = resource {
            commands.write(PlayerCommand::Harvest(resource));
        } else {
            commands.write(PlayerCommand::MoveSelected(world_pos));
        }
    }
}

#[derive(Component)]
pub struct ResourceNodeSprite;

#[derive(Component)]
pub struct EconomyHud;

pub fn sync_resource_nodes(
    mut commands: Commands,
    asset_server: Res<AssetServer>,
    nodes: Query<(Entity, &WorldPosition), (With<ResourceNode>, Without<ResourceNodeSprite>)>,
) {
    for (entity, position) in &nodes {
        commands.entity(entity).insert((
            Sprite {
                image: asset_server.load("do/scheme_plastic/forest_stay/forest_stay1.png"),
                custom_size: Some(Vec2::splat(64.0)),
                ..default()
            },
            Transform::from_xyz(position.0.x, position.0.y, 0.8),
            ResourceNodeSprite,
        ));
    }
}

pub fn sync_resource_transforms(
    mut nodes: Query<(&WorldPosition, &mut Transform), With<ResourceNode>>,
) {
    for (position, mut transform) in &mut nodes {
        transform.translation.x = position.0.x;
        transform.translation.y = position.0.y;
    }
}

pub fn sync_economy_hud(
    economy: Res<PlayerEconomy>,
    mut text: Query<&mut Text2d, With<EconomyHud>>,
    mut last_value: Local<Option<u32>>,
) {
    if *last_value == Some(economy.wood) {
        return;
    }
    *last_value = Some(economy.wood);
    for mut text in &mut text {
        text.0 = format!("Wood: {}", economy.wood);
    }
}

pub fn sync_unit_sprites(
    mut commands: Commands,
    q: Query<(Entity, &Unit, &Player, &WorldPosition), Without<UnitSprite>>,
) {
    for (entity, unit, _player, position) in q.iter() {
        // Transform is a presentation copy of the simulation's world position.
        commands.entity(entity).insert((
            Sprite {
                color: unit.kind.color(),
                custom_size: Some(Vec2::splat(26.0)),
                ..default()
            },
            UnitSprite,
            Transform::from_xyz(position.0.x, position.0.y, 1.0),
        ));
    }
}

pub fn sync_unit_transforms(mut q: Query<(&WorldPosition, &mut Transform), With<Unit>>) {
    for (position, mut transform) in &mut q {
        transform.translation.x = position.0.x;
        transform.translation.y = position.0.y;
    }
}

pub fn show_selection(mut q: Query<(&Unit, Option<&Selected>, &mut Sprite)>) {
    for (unit, selected, mut sprite) in &mut q {
        sprite.color = if selected.is_some() {
            Color::srgb(1.0, 1.0, 0.55)
        } else {
            unit.kind.color()
        };
        sprite.custom_size = Some(Vec2::splat(if selected.is_some() { 34.0 } else { 26.0 }));
    }
}
