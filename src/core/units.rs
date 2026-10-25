//! Unit definitions and shared component types.

use bevy::prelude::*;
use std::collections::VecDeque;

#[derive(Component, Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnitKind {
    Peasant,
    Warrior,
    Archer,
}

impl UnitKind {
    pub fn speed(self) -> f32 {
        match self {
            UnitKind::Peasant => 2.0,
            UnitKind::Warrior => 2.5,
            UnitKind::Archer => 2.2,
        }
    }

    pub fn max_hp(self) -> f32 {
        match self {
            UnitKind::Peasant => 40.0,
            UnitKind::Warrior => 100.0,
            UnitKind::Archer => 60.0,
        }
    }

    pub fn color(self) -> Color {
        match self {
            UnitKind::Peasant => Color::srgb(0.9, 0.8, 0.3),
            UnitKind::Warrior => Color::srgb(0.8, 0.2, 0.2),
            UnitKind::Archer => Color::srgb(0.3, 0.7, 0.9),
        }
    }
}

/// Current hit points of a unit.
#[derive(Component, Debug)]
pub struct Health {
    pub current: f32,
    pub max: f32,
}

/// Grid route generated for the unit's current movement order.
#[derive(Component, Debug, Default)]
pub struct MovePath(pub VecDeque<IVec2>);

#[derive(Component, Debug)]
pub struct ResourceNode {
    pub wood: u32,
}

#[derive(Component, Debug)]
pub struct HarvestOrder {
    pub target: Entity,
    pub elapsed: f32,
}

#[derive(Resource, Debug, Default)]
pub struct PlayerEconomy {
    pub wood: u32,
}

/// Unit position in isometric world coordinates, owned by the simulation.
#[derive(Component, Debug, Clone, Copy)]
pub struct WorldPosition(pub Vec2);

/// Player ownership (0 = human, 1 = AI for now).
#[derive(Component, Debug)]
pub struct Player(pub u8);

/// Marks an entity as a selectable unit with a kind.
#[derive(Component, Debug)]
pub struct Unit {
    pub kind: UnitKind,
}

#[derive(Component, Debug)]
pub struct Selected;

#[derive(Event, Debug, Clone, Copy)]
pub enum PlayerCommand {
    Select(Option<Entity>),
    MoveSelected(Vec2),
    Harvest(Entity),
}
