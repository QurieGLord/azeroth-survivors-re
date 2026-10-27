//! The hero: its body, WASD movement over the terrain, the top-down camera, and its health.

use benilla_world::view::WorldCamera;
use benilla_world::world_point::WorldPoint;
use bevy::input::mouse::{AccumulatedMouseMotion, MouseScrollUnit, MouseWheel};
use bevy::prelude::*;

use super::data::{self, ClassDef};
use super::units::{self, flat, wow_to_bevy, yaw_toward, Fx, Marks, UnitSpec};
use super::{Phase, Run, Selection};
use crate::creature_anim::{
    ease_strafe_yaw, move_flags, strafe_body_offset, wrap_pi, BodyTwist, MovementState,
};
use crate::net::{FieldChanged, Guid, ObjectStore};
use bevy::window::{CursorGrabMode, CursorOptions, PrimaryWindow};

#[derive(Component)]
pub(super) struct Hero {
    /// Vertical velocity in yards/second while airborne; 0 on the ground.
    pub(super) vy: f32,
    /// Height above the ground while airborne. Kept here, not in the transform: the creature
    /// ground clamp (`net::motion::ground_clamp_creatures`) re-seats every unit on the floor at
    /// the top of the frame, so the arc is re-applied over the ground each frame.
    pub(super) air: f32,
    /// Facing yaw (radians) the hero is aiming towards.
    pub(super) face_yaw: f32,
    /// Rendered body heading yaw (radians), accounting for strafe body offsets.
    pub(super) model_yaw: f32,
}

/// The body radius enemies stop at.
pub(super) const HERO_RADIUS: f32 = 0.7;

/// The hop's initial upward speed and the gravity that brings it back down — the reference
/// `CMovement` jump seed and gravity (`player/state.rs`'s `JUMP_SPEED` / `GRAVITY`).
const JUMP_SPEED: f32 = 7.955_547;
const GRAVITY: f32 = 19.291_105;

/// Radians of orbit per raw mouse unit while right-dragging.
const ORBIT_RATE: f32 = 0.008;
/// Elevation clamp (radians): ~10°..~83° above the horizon, so the camera never dives underground.
const PITCH_MIN: f32 = 0.18;
const PITCH_MAX: f32 = 1.45;
/// The locked view's tilt.
const FIXED_PITCH: f32 = 61.0 * std::f32::consts::PI / 180.0;
/// Wheel zoom range, yards from the hero's shoulders.
const ZOOM_MIN: f32 = 1.5;
const ZOOM_MAX: f32 = 70.0;

/// Whether the camera is locked; `C` toggles it.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum CamMode {
    /// Locked top-down (58°), laptop-friendly — the original feel.
    #[default]
    Fixed,
    /// Unlocked: a mouse drag swings and tilts it around the hero.
    Orbit,
}

#[derive(Resource)]
pub(super) struct CamRig {
    pub dist: f32,
    pub orbit: f32,
    /// Orbit azimuth (radians) around the hero; right-drag swings it during play.
    pub yaw: f32,
    /// Elevation above the horizon (radians), clamped so the camera never dives underground.
    pub pitch: f32,
    pub mode: CamMode,
    pub cursor_stash: Option<Vec2>,
    pub right_down: bool,
}

impl Default for CamRig {
    fn default() -> Self {
        Self {
            dist: 30.0,
            orbit: 0.6,
            yaw: 0.0,
            pitch: FIXED_PITCH,
            mode: CamMode::Fixed,
            cursor_stash: None,
            right_down: false,
        }
    }
}

/// Where a battleground's runs start.
pub(super) fn arena(map: &data::MapDef) -> Vec3 {
    wow_to_bevy(map.center)
}

/// The world is the battleground being stood in ([`Selection::stage`]): entering one on another
/// continent worldports, which raises the loading screen until the new ground is in.
pub(super) fn follow_map(
    selection: Res<Selection>,
    current: Option<ResMut<benilla_world::world_map::CurrentMap>>,
) {
    let Some(mut current) = current else {
        return;
    };
    let want = selection.stage_def().map;
    if current.0 != want {
        current.0 = want;
    }
}

/// The hero in the class trainer's look, weapons drawn.
pub(super) fn spawn_hero(
    commands: &mut Commands,
    class: &'static ClassDef,
    display: u32,
    race: u8,
    sex: u8,
    serial: u64,
    pos: Vec3,
) -> Entity {
    let bytes0 = u32::from(race)
        | u32::from(class.id) << 8
        | u32::from(sex) << 16
        | u32::from(class.power) << 24;
    let e = units::spawn_unit(
        commands,
        &UnitSpec {
            serial,
            display,
            scale: 1.0,
            level: 60,
            faction: units::FACTION_FRIENDLY,
            bytes0,
            weapons: class.weapons,
            sheath: class.sheath,
        },
        pos,
        0.0,
    );
    commands
        .entity(e)
        .insert((
            Hero {
                vy: 0.0,
                air: 0.0,
                face_yaw: 0.0,
                model_yaw: 0.0,
            },
            Marks::buffs(),
        ));
    e
}

/// Midday over the battleground, whatever the clock says.
pub(super) const MINUTE_OF_DAY: u32 = 780;

pub(super) fn daylight(debug: Option<ResMut<benilla_world::dev_state::DebugState>>) {
    let Some(mut debug) = debug else { return };
    if debug.lighting.follow_server_time || debug.lighting.manual_minute != MINUTE_OF_DAY {
        debug.lighting.follow_server_time = false;
        debug.lighting.manual_minute = MINUTE_OF_DAY;
    }
}

/// The ground under the mouse cursor, `None` with the cursor off the window. The locked camera's
/// hero faces it, and the aimed mobility spells (Blink, Charge) go toward it.
#[derive(Resource, Default)]
pub(super) struct AimPoint(pub(super) Option<Vec3>);

/// Cast the cursor onto the terrain: the camera ray meets the plane at the hero's height, then the
/// terrain height there, a few times over, which settles on hills within a frame.
pub(super) fn aim_cursor(
    windows: Query<&Window, With<bevy::window::PrimaryWindow>>,
    cams: Query<(&Camera, &Transform), With<WorldCamera>>,
    hero: Query<&Transform, (With<Hero>, Without<WorldCamera>)>,
    world: WorldPoint,
    mut aim: ResMut<AimPoint>,
) {
    let point = (|| {
        let cursor = windows.single().ok()?.cursor_position()?;
        let (cam, tf) = cams.single().ok()?;
        let ray = cam
            .viewport_to_world(&GlobalTransform::from(*tf), cursor)
            .ok()?;
        let mut y = hero.single().ok()?.translation.y;
        let mut at = None;
        for _ in 0..4 {
            if ray.direction.y.abs() < 1e-4 {
                break;
            }
            let t = (y - ray.origin.y) / ray.direction.y;
            if t <= 0.0 {
                break;
            }
            let p = ray.origin + *ray.direction * t;
            at = Some(p);
            match world.terrain_height_under(p) {
                Some(h) if (h - y).abs() > 0.05 => y = h,
                _ => break,
            }
        }
        at
    })();
    if aim.0 != point {
        aim.0 = point;
    }
}

/// WASD over the terrain within the battleground's leash, and the streaming focus on the hero.
#[allow(clippy::too_many_arguments)]
pub(super) fn drive_hero(
    time: Res<Time>,
    keys: Res<ButtonInput<KeyCode>>,
    mouse: Res<ButtonInput<MouseButton>>,
    phase: Res<State<Phase>>,
    run: Res<Run>,
    selection: Res<Selection>,
    rig: Res<CamRig>,
    aim: Res<AimPoint>,
    world: WorldPoint,
    auto: Option<Res<super::auto::Auto>>,
    mut player: ResMut<crate::player::Player>,
    mut hero: Query<(&mut Transform, &mut MovementState, &mut Hero, Option<&mut BodyTwist>)>,
) {
    let focus = match hero.single_mut() {
        Ok((mut tf, mut mv, mut hm, twist)) => {
            let dt = time.delta_secs();
            let mut move_input = Vec3::ZERO;
            let playing = *phase.get() == Phase::Playing && run.dead_t.is_none();
            let rmb = playing && (mouse.pressed(MouseButton::Right) || rig.right_down);

            let (sy, cy) = rig.yaw.sin_cos();
            // Camera horizontal forward and right vectors (yaw = 0 faces -Z)
            let cam_fwd = Vec3::new(-sy, 0.0, -cy);
            let cam_right = Vec3::new(cy, 0.0, -sy);

            let mut dir = Vec3::ZERO;
            let mut anim_flags = 0u32;
            let mut face = None;

            if let Some(auto) = auto
                .as_deref()
                .filter(|_| playing)
            {
                dir = super::auto::steer(auto);
                if dir != Vec3::ZERO {
                    face = Some(dir.normalize());
                    anim_flags = move_flags::FORWARD;
                }
            } else if playing {
                if keys.any_pressed([KeyCode::KeyW, KeyCode::ArrowUp]) {
                    move_input.z -= 1.0;
                }
                if keys.any_pressed([KeyCode::KeyS, KeyCode::ArrowDown]) {
                    move_input.z += 1.0;
                }
                if keys.any_pressed([KeyCode::KeyA, KeyCode::ArrowLeft]) {
                    move_input.x -= 1.0;
                }
                if keys.any_pressed([KeyCode::KeyD, KeyCode::ArrowRight]) {
                    move_input.x += 1.0;
                }

                if rmb || rig.mode == CamMode::Orbit {
                    // Camera-relative movement (authentic WoW mouse-look / third-person)
                    dir = cam_fwd * -move_input.z + cam_right * move_input.x;
                    face = Some(cam_fwd);

                    if move_input.z < 0.0 {
                        anim_flags |= move_flags::FORWARD;
                    }
                    if move_input.z > 0.0 {
                        anim_flags |= move_flags::BACKWARD;
                    }
                    if move_input.x < 0.0 {
                        anim_flags |= move_flags::STRAFE_LEFT;
                    }
                    if move_input.x > 0.0 {
                        anim_flags |= move_flags::STRAFE_RIGHT;
                    }
                } else {
                    // Top-down fixed view: world-relative movement, hero faces cursor
                    dir = Vec3::new(move_input.x, 0.0, move_input.z);
                    let pos = tf.translation;
                    face = aim.0.map(|a| flat(a - pos))
                        .filter(|d| d.length() > 0.5)
                        .map(|d| d.normalize());

                    if let Some(look) = face {
                        if dir != Vec3::ZERO {
                            anim_flags = gait(look, dir.normalize());
                        }
                    } else if dir != Vec3::ZERO {
                        anim_flags = move_flags::FORWARD;
                        face = Some(dir.normalize());
                    }
                }
            }

            let mut pos = tf.translation;

            if let Some(look) = face {
                hm.face_yaw = yaw_toward(look);
            }

            if dir != Vec3::ZERO && dt > 0.0 {
                let dir_norm = dir.normalize();
                // Backpedaling in WoW is slightly slower (~70% of run speed)
                let backpedal_scale = if (anim_flags & move_flags::BACKWARD != 0)
                    && (anim_flags & move_flags::FORWARD == 0)
                {
                    0.7
                } else {
                    1.0
                };
                let speed = run.stats().speed * run.haste_mult() * backpedal_scale;
                pos += dir_norm * speed * dt;
                mv.speed = speed;
                mv.flags = anim_flags;

                let strafe_offset = strafe_body_offset(anim_flags);
                if strafe_offset != 0.0 {
                    hm.model_yaw = ease_strafe_yaw(hm.model_yaw, hm.face_yaw, strafe_offset, dt);
                } else {
                    hm.model_yaw = hm.face_yaw;
                }
            } else {
                if mv.speed != 0.0 || mv.flags != 0 {
                    mv.speed = 0.0;
                    mv.flags = 0;
                }
                hm.model_yaw = hm.face_yaw;
            }

            tf.rotation = Quat::from_rotation_y(hm.model_yaw);

            if let Some(mut twist) = twist {
                twist.yaw_gap = wrap_pi(hm.face_yaw - hm.model_yaw);
            }
            let map = selection.stage_def();
            let center = arena(map);
            let off = flat(pos - center);
            if off.length() > map.leash * 2.0 {
                // Just entered another battleground: start at its centre.
                pos = center;
            } else if off.length() > map.leash {
                let held = center + off.normalize() * map.leash;
                pos.x = held.x;
                pos.z = held.z;
            }
            let ground = world.terrain_height_under(pos).unwrap_or(pos.y);
            let playing = *phase.get() == Phase::Playing && run.dead_t.is_none();
            // Space hops: lift off the terrain, then gravity brings the hero back down.
            if playing && keys.just_pressed(KeyCode::Space) && hm.vy == 0.0 && hm.air == 0.0 {
                hm.vy = JUMP_SPEED;
            }
            if playing && (hm.vy != 0.0 || hm.air > 0.0) {
                hm.vy -= GRAVITY * dt;
                hm.air += hm.vy * dt;
                if hm.air <= 0.0 {
                    hm.air = 0.0;
                    hm.vy = 0.0;
                }
            } else {
                hm.air = 0.0;
                hm.vy = 0.0;
            }
            pos.y = ground + hm.air;
            // Feed the animation driver: `vertical_speed`'s sign and the `FALLING` flag are what
            // select the Jump (37/38) / Fall (40) animation (`creature_anim::select`).
            mv.vertical_speed = hm.vy;
            if hm.vy != 0.0 {
                mv.flags |= move_flags::FALLING;
            }
            if tf.translation != pos {
                tf.translation = pos;
            }
            pos
        }
        Err(_) => {
            let mut p = arena(selection.stage_def());
            if let Some(h) = world.terrain_height_under(p) {
                p.y = h;
            }
            p
        }
    };
    // The terrain streams and the loading screen waits on the ground under this.
    player.active = true;
    player.detached = false;
    player.settling = false;
    player.world_stale = false;
    player.pos = focus;
}

/// Let go of the mouse buttons when the page loses focus or the pointer leaves it. A release that
/// lands elsewhere (alt-tab mid-drag, a browser or OS popup) never reaches the game, so the button
/// would read as held and the unlocked camera would swing with every move until both buttons were
/// clicked again. Survivors only: the engine's mouse-look keeps its buttons across a deactivate,
/// as 1.12.1 does.
pub(super) fn drop_stray_buttons(
    mut focus: MessageReader<bevy::window::WindowFocused>,
    mut left: MessageReader<bevy::window::CursorLeft>,
    mut mouse: ResMut<ButtonInput<MouseButton>>,
    mut windows: Query<(&mut Window, &mut CursorOptions), With<PrimaryWindow>>,
    rig: Option<ResMut<CamRig>>,
) {
    let lost = focus.read().any(|f| !f.focused);
    if lost | (left.read().count() > 0) {
        mouse.release_all();
        if let Ok((mut window, mut cursor_opts)) = windows.single_mut() {
            if cursor_opts.grab_mode == CursorGrabMode::Locked {
                cursor_opts.grab_mode = CursorGrabMode::None;
                cursor_opts.visible = true;
                if let Some(mut rig) = rig {
                    rig.right_down = false;
                    if let Some(pos) = rig.cursor_stash.take() {
                        window.set_cursor_position(Some(pos));
                    }
                }
            }
        }
    }
}

/// A slow orbit at the menu; in play, top-down until `C` unlocks it to swing and tilt.
/// When holding Right Mouse Button, the cursor is locked and hidden like in WoW, and mouse motion
/// orbits the camera while turning the character.
pub(super) fn place_camera(
    time: Res<Time<Real>>,
    phase: Res<State<Phase>>,
    mut rig: ResMut<CamRig>,
    mut wheel: MessageReader<MouseWheel>,
    mouse: Res<ButtonInput<MouseButton>>,
    motion: Res<AccumulatedMouseMotion>,
    keys: Res<ButtonInput<KeyCode>>,
    player: Res<crate::player::Player>,
    world: WorldPoint,
    hero: Query<&Transform, (With<Hero>, Without<WorldCamera>)>,
    mut cams: Query<&mut Transform, With<WorldCamera>>,
    settings: Option<Res<super::SurvivorsSettings>>,
    mut windows: Query<(&mut Window, &mut CursorOptions), With<PrimaryWindow>>,
) {
    let playing = *phase.get() == Phase::Playing;
    let rmb = playing && mouse.pressed(MouseButton::Right);
    let lmb = playing && mouse.pressed(MouseButton::Left);
    rig.right_down = rmb;

    if let Ok((mut window, mut cursor_opts)) = windows.single_mut() {
        if playing && mouse.just_pressed(MouseButton::Right) {
            rig.cursor_stash = window.cursor_position();
            cursor_opts.grab_mode = CursorGrabMode::Locked;
            cursor_opts.visible = false;
        } else if !rmb && cursor_opts.grab_mode == CursorGrabMode::Locked {
            cursor_opts.grab_mode = CursorGrabMode::None;
            cursor_opts.visible = true;
            if let Some(pos) = rig.cursor_stash.take() {
                window.set_cursor_position(Some(pos));
            }
        }
    }

    // `C` unlocks the camera to swing and tilt; locking it again snaps back to the top-down view,
    // so world-relative WASD lines up with the screen again.
    if keys.just_pressed(KeyCode::KeyC) {
        rig.mode = match rig.mode {
            CamMode::Fixed => CamMode::Orbit,
            CamMode::Orbit => {
                rig.yaw = 0.0;
                rig.pitch = FIXED_PITCH;
                CamMode::Fixed
            }
        };
    }
    // With no hero yet, the focus `drive_hero` grounded.
    let focus = hero.single().map_or(player.pos, |t| t.translation);
    // Multiplicative, so a notch moves as far by eye close in as far out; all the way down to an
    // over-the-shoulder view.
    for w in wheel.read() {
        let notches = match w.unit {
            MouseScrollUnit::Line => w.y,
            MouseScrollUnit::Pixel => w.y * 0.02,
        };
        rig.dist = (rig.dist * (1.0 - 0.12 * notches).clamp(0.5, 1.5)).clamp(ZOOM_MIN, ZOOM_MAX);
    }
    let (eye, look) = if *phase.get() == Phase::Menu {
        rig.orbit += time.delta_secs() * 0.2;
        let d = 7.0;
        (
            focus + Vec3::new(rig.orbit.sin() * d, 2.4, rig.orbit.cos() * d),
            focus + Vec3::Y * 1.1,
        )
    } else {
        // Dragging with RMB (or LMB while unlocked) swings camera around hero, as in WoW.
        if rmb || (rig.mode == CamMode::Orbit && lmb) {
            let sens = settings.as_ref().map_or(1.0, |s| s.mouse_sensitivity);
            rig.yaw -= motion.delta.x * (ORBIT_RATE * sens);
            rig.pitch = (rig.pitch + motion.delta.y * (ORBIT_RATE * sens)).clamp(PITCH_MIN, PITCH_MAX);
        }
        let (sy, cy) = rig.yaw.sin_cos();
        let (sp, cp) = rig.pitch.sin_cos();
        let look = focus + Vec3::Y * 1.4;
        let mut eye = look + Vec3::new(cp * sy, sp, cp * cy) * rig.dist;
        // Never under the ground: a low tilt on a slope would bury the lens.
        if let Some(h) = world.terrain_height_under(eye) {
            eye.y = eye.y.max(h + 0.6);
        }
        (eye, look)
    };
    let placed = Transform::from_translation(eye).looking_at(look, Vec3::Y);
    for mut cam in &mut cams {
        *cam = placed;
    }
}

/// Damage the hero takes from one blow, after immunity, armour and the shield; returns what got
/// through.
pub(super) fn hurt_hero(run: &mut Run, raw: f32) -> f32 {
    if run.immune_t > 0.0 || run.dead_t.is_some() {
        return 0.0;
    }
    let mut dmg = (raw - run.stats().armor).max(1.0);
    if run.shield_t > 0.0 && run.shield > 0.0 {
        let absorbed = dmg.min(run.shield);
        run.shield -= absorbed;
        dmg -= absorbed;
    }
    run.hp -= dmg;
    dmg
}

/// The clock, regeneration, the timed buffs and their visuals, death and victory.
pub(super) fn hero_vitals(
    time: Res<Time>,
    mut run: ResMut<Run>,
    mut next: ResMut<NextState<Phase>>,
    mut hero: Query<(Entity, &Guid, &mut ObjectStore, &mut Marks), With<Hero>>,
    mut fields: MessageWriter<FieldChanged>,
    mut fx: Fx,
) {
    let dt = time.delta_secs();
    let stats = run.stats();
    let Ok((entity, guid, mut store, mut marks)) = hero.single_mut() else {
        return;
    };
    if let Some(t) = run.dead_t.as_mut() {
        *t += dt;
        if *t > 3.0 {
            next.set(Phase::GameOver);
        }
        return;
    }
    run.elapsed += dt;
    let r = &mut *run;
    for t in [&mut r.shield_t, &mut r.immune_t, &mut r.thorns_t] {
        *t = (*t - dt).max(0.0);
    }
    let mut heal = stats.regen * dt;
    if run.heal_t > 0.0 {
        heal += run.heal_rate * dt.min(run.heal_t);
        run.heal_t = (run.heal_t - dt).max(0.0);
    }
    run.hp = (run.hp + heal).min(stats.max_hp);

    // The buffs show as their own state kits: the shield bubble, the immunity glow, the orbs.
    let mut shown: Vec<u32> = Vec::new();
    if run.shield_t > 0.0 && run.shield > 0.0 {
        shown.push(17);
    }
    if run.immune_t > 0.0 {
        shown.push(642);
    }
    if run.thorns_t > 0.0 {
        shown.push(run.thorns_spell);
    }
    if run.heal_t > 0.0 {
        shown.push(774);
    }
    for owned in &run.abilities {
        if owned.def.key == "lightning_shield" {
            shown.push(owned.def.spell);
        }
    }
    if run.passives.iter().any(|(p, _)| p.key == "renew") {
        shown.push(139);
    }
    marks.set(&shown);
    marks.flush(entity, guid.0, &mut store, &mut fields);

    if run.hp <= 0.0 {
        run.hp = 0.0;
        run.dead_t = Some(0.0);
        units::kill_unit(&mut store);
        marks.clear();
        marks.flush(entity, guid.0, &mut store, &mut fields);
    } else if run.elapsed >= data::VICTORY_SECS {
        run.won = true;
        // A last flourish before the results.
        fx.cast(entity, 642, &[entity], None);
        next.set(Phase::GameOver);
    }
}

/// The ground-plane distance from the hero, the measure every range in the mode uses.
/// The movement flags for running `dir` while facing `look`: forward, backpedal or strafe,
/// including diagonal combinations (forward+strafe, backward+strafe).
fn gait(look: Vec3, dir: Vec3) -> u32 {
    let ahead = look.dot(dir);
    // Positive when `dir` is to the hero's left: WoW faces −Z with +X on the right.
    let left = look.cross(dir).y;
    let mut f = 0u32;
    if ahead > 0.38 {
        f |= move_flags::FORWARD;
    } else if ahead < -0.38 {
        f |= move_flags::BACKWARD;
    }
    if left > 0.38 {
        f |= move_flags::STRAFE_LEFT;
    } else if left < -0.38 {
        f |= move_flags::STRAFE_RIGHT;
    }
    if f == 0 {
        if ahead >= 0.0 {
            move_flags::FORWARD
        } else {
            move_flags::BACKWARD
        }
    } else {
        f
    }
}

pub(super) fn reach(hero: Vec3, other: Vec3) -> f32 {
    flat(other - hero).length()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::ecs::system::RunSystemOnce;
    use bevy::window::{CursorLeft, WindowFocused};

    fn held_left() -> World {
        let mut world = World::new();
        world.init_resource::<Messages<WindowFocused>>();
        world.init_resource::<Messages<CursorLeft>>();
        let mut mouse = ButtonInput::<MouseButton>::default();
        mouse.press(MouseButton::Left);
        world.insert_resource(mouse);
        world
    }

    fn left_held(world: &World) -> bool {
        world.resource::<ButtonInput<MouseButton>>().pressed(MouseButton::Left)
    }

    #[test]
    fn a_held_button_is_dropped_when_the_page_loses_focus() {
        let mut world = held_left();
        world.run_system_once(drop_stray_buttons).unwrap();
        assert!(left_held(&world), "nothing happened: the drag goes on");
        world.write_message(WindowFocused { window: Entity::PLACEHOLDER, focused: true });
        world.run_system_once(drop_stray_buttons).unwrap();
        assert!(left_held(&world), "gaining focus is no release");
        world.write_message(WindowFocused { window: Entity::PLACEHOLDER, focused: false });
        world.run_system_once(drop_stray_buttons).unwrap();
        assert!(!left_held(&world));
    }

    #[test]
    fn a_held_button_is_dropped_when_the_pointer_leaves_the_page() {
        let mut world = held_left();
        world.write_message(CursorLeft { window: Entity::PLACEHOLDER });
        world.run_system_once(drop_stray_buttons).unwrap();
        assert!(!left_held(&world));
    }
}
