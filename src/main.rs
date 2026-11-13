use crate::time_range_parser::TimeRangeParser;
use chrono::{DateTime, Local, Timelike, Utc};
use huelib2::resource::group::StateModifier;
use huelib2::resource::light::StateModifier as LightStateModifier;
use huelib2::resource::{Light, Scene};
use huelib2::Bridge;
use std::collections::HashMap;
use std::fs::File;
use std::io::{Seek, Write};
use std::time::Instant;

mod config;
mod manual_scene;
mod scene_lights;
mod time_range_parser;
mod utils;

#[derive(Clone, PartialEq, Debug)]
struct StateChange {
    pub timestamp: Option<Instant>,
    pub reachable: bool,
}

struct PendingConfirm {
    scene_id: String,
    lights_hash: u64,
    lights: Vec<String>,
    due: Instant,
}

impl PendingConfirm {
    fn new(
        scene_id: &str,
        lights_hash: u64,
        all_scenes: &[Scene],
        delay: std::time::Duration,
    ) -> Self {
        PendingConfirm {
            scene_id: scene_id.to_string(),
            lights_hash,
            lights: all_scenes
                .iter()
                .find(|s| s.id == scene_id)
                .and_then(|s| s.lights.clone())
                .unwrap_or_default(),
            due: Instant::now() + delay,
        }
    }
}

fn main() {
    let mut light_states = HashMap::<String, StateChange>::new();
    // Tracks the last scene applied per light-group (keyed by sorted-lights hash).
    // Used to detect time-slot changes and avoid redundant transitions.
    let mut active_scenes = HashMap::<u64, String>::new();
    let mut last_minute: Option<u32> = None;
    // Scenes to send again once their transition is over, to catch lights
    // (typically non-Hue bulbs) that missed or aborted the first command.
    let mut pending_confirms = Vec::<PendingConfirm>::new();
    // Time of our last scene command per light-group. A scene recalled after it was chosen
    // by hand (app, remote) and is kept until the next time slot.
    let mut last_commands = HashMap::<u64, DateTime<Utc>>::new();
    // Last known on-state of attached lights, used to detect them being turned on
    let mut attached_on_states = HashMap::<String, bool>::new();
    let mut conf = config::load_config();
    let bridge = Bridge::new(conf.bridge_ip.clone(), &conf.bridge_username);

    println!(
        "Starting hue-scheduler at {}",
        DateTime::<Utc>::from(Local::now())
            .with_timezone(&conf.home_timezone)
            .format("%Y-%m-%d %H:%M:%S %Z")
    );

    loop {
        std::thread::sleep(conf.ping_interval);

        let all_lights = match bridge.get_all_lights() {
            Ok(result) => result,
            Err(error) => {
                eprintln!("Failed to retrieve lights: {:?}", error);
                continue;
            }
        };

        match conf.debug_file {
            Some(ref mut file) => write_debug_file(&all_lights, file),
            None => (),
        };

        // Re-send scenes whose transition is over, if still active and lights still on
        let (due, waiting): (Vec<PendingConfirm>, Vec<PendingConfirm>) = pending_confirms
            .drain(..)
            .partition(|p| p.due <= Instant::now());
        pending_confirms = waiting;
        for confirm in due.iter() {
            let still_active = active_scenes.values().any(|id| id == &confirm.scene_id);
            let lights_on = confirm.lights.iter().any(|lid| {
                all_lights
                    .iter()
                    .find(|l| &l.id == lid)
                    .map(|l| l.state.on.unwrap_or(false))
                    .unwrap_or(false)
            });
            if still_active && lights_on {
                let manual = manual_scene_name(
                    &conf,
                    confirm.lights_hash,
                    &confirm.scene_id,
                    &all_lights,
                    last_commands
                        .get(&confirm.lights_hash)
                        .copied()
                        .unwrap_or_else(Utc::now),
                );
                if let Some(name) = manual {
                    println!("Scene \"{}\" was chosen by hand, not re-applying", name);
                    continue;
                }
                println!("Re-applying scene to catch lights that missed the transition");
                if let Err(err) = bridge.set_group_state(
                    "0",
                    &StateModifier::new()
                        .with_scene(confirm.scene_id.clone())
                        .with_transition_time(10),
                ) {
                    eprintln!("Failed to re-apply scene: {}", err);
                }
                scene_lights::apply_to_non_hue_lights(
                    &conf.bridge_ip,
                    &conf.bridge_username,
                    &confirm.scene_id,
                    &all_lights,
                    10,
                );
                last_commands.insert(confirm.lights_hash, Utc::now());
            }
        }

        let changed_lights = all_lights
            .iter()
            .filter(|light| {
                !utils::is_attached_light(light)
                    && light_states
                        .get(&light.id)
                        .map(|last| last.reachable != light.state.reachable)
                        .unwrap_or(true)
            })
            .collect::<Vec<&Light>>();

        // Initialize light_states on first run
        if light_states.is_empty() {
            for light in changed_lights.iter() {
                light_states.insert(
                    light.id.clone(),
                    StateChange {
                        timestamp: None,
                        reachable: light.state.reachable,
                    },
                );
            }
            println!("Initialized reachable lights.");
            continue;
        }

        // Update state for lights that changed reachability
        for light in changed_lights.iter() {
            if let Some(last) = light_states.get(&light.id) {
                if last.reachable && !light.state.reachable {
                    println!("Light \"{}\" is not reachable anymore", light.name);
                } else {
                    println!("Light \"{}\" is reachable again", light.name);
                }
            }
            light_states.insert(
                light.id.clone(),
                StateChange {
                    timestamp: Some(Instant::now()),
                    reachable: light.state.reachable,
                },
            );
        }

        let ignored_light_ids = attached_light_ids(&all_lights);

        let turned_on_attached_ids = all_lights
            .iter()
            .filter(|light| {
                utils::is_attached_light(light)
                    && light.state.on.unwrap_or(false)
                    && attached_on_states.get(&light.id) == Some(&false)
            })
            .map(|light| &light.id)
            .collect::<Vec<&String>>();

        for light in all_lights
            .iter()
            .filter(|light| utils::is_attached_light(light))
        {
            attached_on_states.insert(light.id.clone(), light.state.on.unwrap_or(false));
        }

        let light_trigger_ids = light_states
            .iter()
            .filter(|(_, state)| {
                state.reachable
                    && state
                        .timestamp
                        .map(|t| t.elapsed() < conf.reachability_window)
                        .unwrap_or(false)
            })
            .map(|(id, _)| id)
            .collect::<Vec<&String>>();

        let date_time = DateTime::<Utc>::from(Local::now()).with_timezone(&conf.home_timezone);
        let current_minute = date_time.hour() * 60 + date_time.minute();
        let minute_changed = last_minute != Some(current_minute);

        // Run scene logic when lights are triggered (Case 1), when the minute changes (Case 2)
        // or when attached lights were turned on (Case 3)
        if !light_trigger_ids.is_empty() || minute_changed || !turned_on_attached_ids.is_empty() {
            let Ok(all_scenes) = bridge.get_all_scenes() else {
                eprintln!("Failed to retrieve scenes");
                continue;
            };

            let Some((sunrise, sunset)) =
                utils::get_sunrise_sunset(conf.home_latitude, conf.home_longitude)
            else {
                eprintln!("Failed to retrieve sunrise/sunset");
                continue;
            };

            let mut parser = TimeRangeParser::new();
            parser.define_variables(HashMap::from([
                ("sunrise".to_string(), sunrise),
                ("sunset".to_string(), sunset),
            ]));

            // Case 3: attached light turned on (e.g. with a remote) — set it to the scheduled scene
            // including it, as it's not set while it's off
            if !turned_on_attached_ids.is_empty() {
                let scheduled_scenes =
                    utils::get_scheduled_scenes(&conf, &parser, &all_scenes, &ignored_light_ids);

                for light_id in turned_on_attached_ids.iter() {
                    let scheduled_scene = scheduled_scenes.iter().find(|scheduled| {
                        all_scenes
                            .iter()
                            .find(|scene| scene.id == scheduled.scene_id)
                            .and_then(|scene| scene.lights.as_ref())
                            .map(|lights| lights.contains(light_id))
                            .unwrap_or(false)
                    });

                    if let Some(scheduled) = scheduled_scene {
                        apply_scene_light_state(&conf, &scheduled.scene_id, light_id);
                    }
                }
            }

            // Case 1: light was just switched on — apply the current scheduled scene immediately
            if !light_trigger_ids.is_empty() {
                let triggered_scenes = all_scenes
                    .iter()
                    .filter(|scene| {
                        scene
                            .lights
                            .clone()
                            .map(|ids| {
                                ids.iter().all(|id| {
                                    ignored_light_ids.contains(&id)
                                        || light_trigger_ids.contains(&id)
                                })
                            })
                            .unwrap_or(false)
                    })
                    .cloned()
                    .collect::<Vec<Scene>>();

                for changed_scene in triggered_scenes.iter() {
                    if let Some(lights) = &changed_scene.lights {
                        for light_id in lights.clone() {
                            light_states.insert(
                                light_id,
                                StateChange {
                                    timestamp: None,
                                    reachable: true,
                                },
                            );
                        }
                    }
                }

                for scheduled in utils::get_scheduled_scenes(
                    &conf,
                    &parser,
                    &triggered_scenes,
                    &ignored_light_ids,
                )
                .iter()
                {
                    let manual = manual_scene::manual_scene(
                        scheduled.lights_hash,
                        &scheduled.scene_id,
                        &all_scenes,
                        &ignored_light_ids,
                        &active_recalls(&conf),
                        last_command_time(&conf, &last_commands, scheduled),
                    );
                    if let Some(name) = manual {
                        println!(
                            "Lights reachable again but scene \"{}\" was chosen by hand, keeping it",
                            name
                        );
                        continue;
                    }
                    println!("Lights switched on, applying scheduled scene");
                    if let Err(err) = bridge.set_group_state(
                        "0",
                        &StateModifier::new()
                            .with_scene(scheduled.scene_id.clone())
                            .with_transition_time(10),
                    ) {
                        eprintln!("Failed to set scene: {}", err);
                    }
                    scene_lights::apply_to_non_hue_lights(
                        &conf.bridge_ip,
                        &conf.bridge_username,
                        &scheduled.scene_id,
                        &all_lights,
                        10,
                    );
                    turn_off_orphaned_attached_lights(
                        &bridge,
                        &scheduled.scene_id,
                        &all_scenes,
                        &all_lights,
                        &conf,
                    );
                    active_scenes.insert(scheduled.lights_hash, scheduled.scene_id.clone());
                    last_commands.insert(scheduled.lights_hash, Utc::now());
                    pending_confirms.push(PendingConfirm::new(
                        &scheduled.scene_id,
                        scheduled.lights_hash,
                        &all_scenes,
                        conf.scene_confirm_delay,
                    ));
                }
            }

            // Case 2: time slot changed — transition lights that are already ON
            if minute_changed {
                last_minute = Some(current_minute);

                let scheduled_scenes =
                    utils::get_scheduled_scenes(&conf, &parser, &all_scenes, &ignored_light_ids);

                // Scenes whose time slot ended without a following one: turn off the attached
                // lights they set, the other lights are left as they are
                for (lights_hash, last_id) in active_scenes.iter_mut() {
                    if last_id.is_empty()
                        || scheduled_scenes
                            .iter()
                            .any(|scheduled| &scheduled.lights_hash == lights_hash)
                    {
                        continue;
                    }

                    if let Some(lights) = all_scenes
                        .iter()
                        .find(|scene| &scene.id == last_id)
                        .and_then(|scene| scene.lights.as_ref())
                    {
                        for light in all_lights.iter().filter(|light| {
                            utils::is_attached_light(light) && lights.contains(&light.id)
                        }) {
                            if get_scene_light_state(&conf, last_id, &light.id)
                                .map(|state| utils::matches_scene_state(light, &state))
                                .unwrap_or(false)
                            {
                                println!(
                                    "Scene ended, turning off attached light \"{}\"",
                                    light.name
                                );
                                if let Err(err) = bridge.set_light_state(
                                    &light.id,
                                    &LightStateModifier::new().with_on(false),
                                ) {
                                    eprintln!("Failed to turn off attached light: {}", err);
                                }
                            }
                        }
                    }

                    // Empty marks "no scene", so the next scene for these lights transitions
                    last_id.clear();
                }

                for scheduled in scheduled_scenes.iter() {
                    // First encounter for this light group (startup): the lights may still
                    // hold a scene from an earlier slot, so the current one is applied too
                    let startup = match active_scenes.get(&scheduled.lights_hash) {
                        None => true,
                        Some(last_id) if last_id == &scheduled.scene_id => continue,
                        Some(_) => false,
                    };
                    active_scenes.insert(scheduled.lights_hash, scheduled.scene_id.clone());

                    let manual = manual_scene::manual_scene(
                        scheduled.lights_hash,
                        &scheduled.scene_id,
                        &all_scenes,
                        &ignored_light_ids,
                        &active_recalls(&conf),
                        last_command_time(&conf, &last_commands, scheduled),
                    );
                    if let Some(name) = manual {
                        println!("Scene \"{}\" was chosen by hand, keeping it", name);
                        continue;
                    }

                    // Scene changed for this light group — apply only if lights are ON
                    let lights_on = all_scenes
                        .iter()
                        .find(|s| s.id == scheduled.scene_id)
                        .and_then(|s| s.lights.as_ref())
                        .map(|lights| {
                            // Reachability is ignored: non-Hue bulbs are often reported
                            // unreachable while still accepting commands
                            lights.iter().any(|lid| {
                                !ignored_light_ids.contains(&lid)
                                    && all_lights
                                        .iter()
                                        .find(|l| &l.id == lid)
                                        .map(|l| l.state.on.unwrap_or(false))
                                        .unwrap_or(false)
                            })
                        })
                        .unwrap_or(false);

                    if lights_on {
                        if startup {
                            println!("Startup, applying scheduled scene to lights already on");
                        } else {
                            println!("Time slot changed, transitioning to new scene");
                        }
                        if let Err(err) = bridge.set_group_state(
                            "0",
                            &StateModifier::new()
                                .with_scene(scheduled.scene_id.clone())
                                .with_transition_time(
                                    (conf.scene_transition.as_millis() / 100) as u16,
                                ),
                        ) {
                            eprintln!("Failed to auto-transition scene: {}", err);
                        }
                        scene_lights::apply_to_non_hue_lights(
                            &conf.bridge_ip,
                            &conf.bridge_username,
                            &scheduled.scene_id,
                            &all_lights,
                            (conf.scene_transition.as_millis() / 100) as u16,
                        );
                        last_commands.insert(scheduled.lights_hash, Utc::now());
                        pending_confirms.push(PendingConfirm::new(
                            &scheduled.scene_id,
                            scheduled.lights_hash,
                            &all_scenes,
                            conf.scene_transition + conf.scene_confirm_delay,
                        ));
                        turn_off_orphaned_attached_lights(
                            &bridge,
                            &scheduled.scene_id,
                            &all_scenes,
                            &all_lights,
                            &conf,
                        );
                    } else if !startup {
                        // Other lights are switched off: turn the attached lights of the new
                        // scene off too, they're set to it once turned on again (Case 3)
                        turn_off_attached_lights(
                            &bridge,
                            &scheduled.scene_id,
                            &all_scenes,
                            &all_lights,
                        );
                        turn_off_orphaned_attached_lights(
                            &bridge,
                            &scheduled.scene_id,
                            &all_scenes,
                            &all_lights,
                            &conf,
                        );
                    }
                }
            }
        }

        // Turn off attached lights when all non-attached lights in a group are unreachable
        if !changed_lights.is_empty() {
            let Ok(all_groups) = bridge.get_all_groups() else {
                eprintln!("Failed to retrieve groups");
                continue;
            };

            for group in all_groups.iter() {
                let some_lights_on = group.lights.iter().any(|light_id| {
                    all_lights
                        .iter()
                        .find(|light| light.id == *light_id)
                        .map(|light| light.state.on.unwrap_or(false))
                        .unwrap_or(false)
                });

                let all_non_attached_turned_off = group.lights.iter().all(|light_id| {
                    ignored_light_ids.contains(&light_id)
                        || (light_states
                            .get(light_id)
                            .map(|state| !state.reachable)
                            .unwrap_or(false))
                });

                // Only groups with attached lights are concerned, otherwise a group of
                // flaky non-Hue bulbs would get turned off while they are still on
                let has_attached_lights = group
                    .lights
                    .iter()
                    .any(|light_id| ignored_light_ids.contains(&light_id));

                if has_attached_lights && some_lights_on && all_non_attached_turned_off {
                    println!(
                        "All non-attached lights are unreachable, turning off group: {}",
                        group.name
                    );

                    if let Err(err) =
                        bridge.set_group_state(&group.id, &StateModifier::new().with_on(false))
                    {
                        eprintln!("Failed to turn off attached lights: {}", err);
                        continue;
                    }
                }
            }
        }
    }
}

/// Time after which a recalled scene counts as chosen by hand: our last command for these
/// lights, but never before the start of the current time slot (a choice made during a
/// previous slot no longer applies).
fn last_command_time(
    conf: &config::Config,
    last_commands: &HashMap<u64, DateTime<Utc>>,
    scheduled: &utils::ScheduledScene,
) -> DateTime<Utc> {
    let now = Utc::now().with_timezone(&conf.home_timezone);
    let now_minute = now.hour() * 60 + now.minute();
    let minutes_since_start = (now_minute + 24 * 60 - scheduled.start) % (24 * 60);
    let slot_start = (now
        - chrono::Duration::minutes(minutes_since_start as i64)
        - chrono::Duration::seconds(now.second() as i64))
    .with_timezone(&Utc);

    last_commands
        .get(&scheduled.lights_hash)
        .copied()
        .map(|t| t.max(slot_start))
        .unwrap_or(slot_start)
}

fn active_recalls(conf: &config::Config) -> HashMap<String, DateTime<Utc>> {
    manual_scene::active_scene_recalls(&conf.bridge_ip, &conf.bridge_username).unwrap_or_default()
}

fn manual_scene_name(
    conf: &config::Config,
    lights_hash: u64,
    scene_id: &str,
    all_lights: &Vec<Light>,
    last_command: DateTime<Utc>,
) -> Option<String> {
    let bridge = Bridge::new(conf.bridge_ip.clone(), &conf.bridge_username);
    let all_scenes = bridge.get_all_scenes().ok()?;
    manual_scene::manual_scene(
        lights_hash,
        scene_id,
        &all_scenes,
        &attached_light_ids(all_lights),
        &active_recalls(conf),
        last_command,
    )
    .map(str::to_string)
}

/// Turns off attached lights in the scene's room that the scene itself doesn't control, as long
/// as they still look like another scene set them (i.e. they weren't changed manually)
fn turn_off_orphaned_attached_lights(
    bridge: &Bridge,
    scene_id: &String,
    all_scenes: &Vec<Scene>,
    all_lights: &Vec<Light>,
    conf: &config::Config,
) {
    let Some(scene_lights) = all_scenes
        .iter()
        .find(|scene| &scene.id == scene_id)
        .and_then(|scene| scene.lights.as_ref())
    else {
        return;
    };

    let Ok(all_groups) = bridge.get_all_groups() else {
        eprintln!("Failed to retrieve groups");
        return;
    };

    for light_id in utils::get_orphaned_attached_lights(scene_lights, &all_groups, all_lights) {
        let Some(light) = all_lights.iter().find(|light| light.id == light_id) else {
            continue;
        };

        let set_by_scene = all_scenes
            .iter()
            .filter(|scene| {
                &scene.id != scene_id
                    && scene
                        .lights
                        .as_ref()
                        .map(|lights| lights.contains(&light_id))
                        .unwrap_or(false)
            })
            .filter_map(|scene| get_scene_light_state(conf, &scene.id, &light_id))
            .any(|scene_state| utils::matches_scene_state(light, &scene_state));

        if !set_by_scene {
            println!(
                "Leaving attached light {} on, it doesn't match any of its scenes",
                light_id
            );
            continue;
        }

        println!(
            "Turning off attached light {} not part of the new scene",
            light_id
        );
        if let Err(err) =
            bridge.set_light_state(&light_id, &LightStateModifier::new().with_on(false))
        {
            eprintln!("Failed to turn off attached light: {}", err);
        }
    }
}

/// Turns off the attached lights of a scene that are on
fn turn_off_attached_lights(
    bridge: &Bridge,
    scene_id: &String,
    all_scenes: &Vec<Scene>,
    all_lights: &Vec<Light>,
) {
    let Some(scene_lights) = all_scenes
        .iter()
        .find(|scene| &scene.id == scene_id)
        .and_then(|scene| scene.lights.as_ref())
    else {
        return;
    };

    for light in all_lights.iter().filter(|light| {
        utils::is_attached_light(light)
            && scene_lights.contains(&light.id)
            && light.state.on.unwrap_or(false)
    }) {
        println!(
            "Time slot changed, turning off attached light \"{}\"",
            light.name
        );
        if let Err(err) =
            bridge.set_light_state(&light.id, &LightStateModifier::new().with_on(false))
        {
            eprintln!("Failed to turn off attached light: {}", err);
        }
    }
}

/// Sets a light to the state a scene defines for it
fn apply_scene_light_state(conf: &config::Config, scene_id: &String, light_id: &String) {
    let Some(mut state) = get_scene_light_state(conf, scene_id, light_id) else {
        return;
    };

    state["transitiontime"] = serde_json::json!(4);
    println!(
        "Attached light {} turned on, setting it to its scene",
        light_id
    );

    let url = format!(
        "http://{}/api/{}/lights/{}/state",
        conf.bridge_ip, conf.bridge_username, light_id
    );
    if let Err(err) = ureq::put(&url).send_json(state) {
        eprintln!("Failed to set attached light: {}", err);
    }
}

/// Fetches the state a scene sets for one of its lights, which huelib2 doesn't expose
fn get_scene_light_state(
    conf: &config::Config,
    scene_id: &String,
    light_id: &String,
) -> Option<serde_json::Value> {
    let url = format!(
        "http://{}/api/{}/scenes/{}",
        conf.bridge_ip, conf.bridge_username, scene_id
    );

    match ureq::get(&url)
        .call()
        .ok()
        .and_then(|response| response.into_json::<serde_json::Value>().ok())
    {
        Some(scene) => scene["lightstates"].get(light_id).cloned(),
        None => {
            eprintln!("Failed to retrieve scene {}", scene_id);
            None
        }
    }
}

fn attached_light_ids(all_lights: &Vec<Light>) -> Vec<&String> {
    all_lights
        .iter()
        .filter(|light| utils::is_attached_light(light))
        .map(|light| &light.id)
        .collect::<Vec<&String>>()
}

fn write_debug_file(lights: &Vec<Light>, file: &mut File) {
    let mut light_stats = lights
        .iter()
        .map(|light| {
            format!(
                "light_{} = {{ name = \"{}\", reachable = {}, on = {} }}",
                format!("{:0>3}", light.id),
                light.name,
                light.state.reachable,
                light.state.on.unwrap_or(false)
            )
        })
        .collect::<Vec<String>>();

    light_stats.sort_by(|a, b| a.cmp(b));

    if let Err(err) = file.seek(std::io::SeekFrom::Start(0)) {
        eprintln!("Failed to seek to beginning of debug file: {}", err);
    }

    if let Err(err) = file.write_all(light_stats.join("\n").as_bytes()) {
        eprintln!("Failed to write to debug file: {}", err);
    }
}
