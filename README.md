# Rust Oberon

Rust Oberon is an open-source real-time strategy game written in Rust, inspired by Dark Oberon. The goal is to recover the original game's readable, fast RTS feel while building a maintainable codebase with a deterministic simulation and a modern presentation. Original game assets and data are used with attribution; see [asset credits](assets/CREDITS.md).

## Project plan

### Principles

- Keep simulation rules independent from rendering and UI so game logic stays testable and can support replays or multiplayer later.
- Use a modular monolith: `core` owns shared data, `sim` owns game rules and commands, and `render` owns presentation and camera controls.
- Keep content data-driven, validate imported map and scheme data, and preserve source attribution and license information for every asset set.
- Build complete playable slices before expanding breadth. Prefer simple, inspectable systems over premature framework layers.
- Treat asset legality as a release requirement: the repository records its current understanding in `assets/CREDITS.md`; verify upstream licensing and provenance before publishing redistributed assets.

### Milestones

1. **Playable movement slice (complete)** — load an original map and terrain art; show a controllable camera and a small player squad; select a unit and issue a move order. Done when the game opens into a legible map, WASD/arrows pan the view, left-click selects a unit, and right-click sends it toward the destination.
2. **Simulation foundation (complete)** — represent positions and orders in a fixed-step simulation; derive walkability from the map's Earth layer; route units around blocked terrain with A*; keep simulation state separate from sprites.
3. **First RTS loop (in progress)** — add one resource and harvesting, then one buildable structure and construction placement. Keep the map and gameplay definitions data-driven; add a basic enemy opponent after the loop is playable.
4. **Core combat and visibility** — add attack/health/death, unit roles, fog of war and a useful combat feedback loop.
5. **Persistence and content tools** — add save/load, map/content validation, error reporting and contributor workflows for maps and definitions.
6. **Polish and release** — improve input/UI/audio, performance on larger battles, packaging, documentation, accessibility and release/license notices.

### Next step: build a Shed

Add one complete construction flow that uses the wood harvested in the current prototype. Use the original Shed build and idle sprites (`assets/do/human/shed_build/` and `assets/do/human/shed_stay/`); keep its prototype cost, footprint and build duration in data so they can be tuned without changing simulation systems.

1. Add a building definition with owner, footprint, wood cost and construction duration.
2. Press **B** to enter placement mode. Show a Shed preview snapped to the map grid; color it green on a valid site and red on an invalid site. **Escape** or right-click cancels placement.
3. Validate the whole footprint against map bounds, walkable terrain, existing buildings and units. Deduct wood only after a valid placement is accepted.
4. Create a construction site with visible progress, then replace it with the completed Shed using the idle sprite.
5. Reserve the footprint for navigation as soon as construction begins; units must not route through a site or completed building.

Acceptance: a player with enough wood can place a Shed on clear land, see it finish construction, and move units around its reserved footprint. Invalid placement must spend no wood; insufficient wood must prevent placement.

### First milestone acceptance checklist

- [x] Original map and terrain are visible at a useful scale.
- [x] Camera starts over the playable area and pans with WASD or arrow keys.
- [x] A small squad spawns at the map's player start point.
- [x] Left-click selects a unit and gives visible feedback; clicking empty ground clears selection.
- [x] Right-click issues a move order and the selected unit moves toward it.
- [x] The startup/run instructions and controls are documented here.

## Running

Requires Rust and Cargo. From the repository root:

```sh
cargo run
```

Controls: **WASD / arrow keys** to pan, **left-click** a unit to select (empty ground clears selection), **right-click** open ground to move, and **right-click** a tree with a Peasant selected to harvest wood.

## Architecture

- `src/core/` — shared types and imported map/scheme data.
- `src/sim/` — fixed-rate game rules, initial setup and player commands.
- `src/render/` — terrain/unit presentation and camera input.
- `assets/` — game assets and maps; see `assets/CREDITS.md` for provenance.

## Contributing

The project is early and APIs are expected to change. For substantial changes, keep simulation and rendering responsibilities separate, document imported assets, and include a concise explanation of how to try the change.
