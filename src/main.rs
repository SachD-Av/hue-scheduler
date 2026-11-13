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
mod time_range_parser;
mod utils;

#[derive(Clone, PartialEq, Debug)]
struct StateChange {
    pub timestamp: Option<Instant>,
    pub reachable: bool,
}

fn main() {
    let mut light_states = HashMap::<String, StateChange>::new();
    // Tracks the last scene applied per light-group (keyed by sorted-lights hash).
    // Used to detect time-slot changes and avoid redundant transitions.
    let mut active_scenes = HashMap::<u64, String>::new();
    let mut last_minute: Option<u32> = None;
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

        let ignored_light_ids = all_lights
            .iter()
            .filter(|light| utils::is_attached_light(light))
            .map(|light| &light.id)
            .collect::<Vec<&String>>();

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

        // Run scene logic when lights are triggered (Case 1) or when the minute changes (Case 2)
        if !light_trigger_ids.is_empty() || minute_changed {
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
                    if let Err(err) = bridge.set_group_state(
                        &scheduled.scene_id,
                        &StateModifier::new()
                            .with_scene(scheduled.scene_id.clone())
                            .with_transition_time(10),
                    ) {
                        eprintln!("Failed to set scene: {}", err);
                    }
                    turn_off_orphaned_attached_lights(
                        &bridge,
                        &scheduled.scene_id,
                        &all_scenes,
                        &all_lights,
                        &conf,
                    );
                    active_scenes.insert(scheduled.lights_hash, scheduled.scene_id.clone());
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
                    match active_scenes.get(&scheduled.lights_hash) {
                        None => {
                            // First encounter for this light group — record without applying
                            // to avoid triggering a slow transition on startup
                            active_scenes.insert(scheduled.lights_hash, scheduled.scene_id.clone());
                            continue;
                        }
                        Some(last_id) if last_id == &scheduled.scene_id => continue,
                        Some(_) => {}
                    }

                    // Scene changed for this light group — apply only if lights are ON
                    let lights_on = all_scenes
                        .iter()
                        .find(|s| s.id == scheduled.scene_id)
                        .and_then(|s| s.lights.as_ref())
                        .map(|lights| {
                            lights.iter().any(|lid| {
                                !ignored_light_ids.contains(&lid)
                                    && light_states.get(lid).map(|s| s.reachable).unwrap_or(false)
                                    && all_lights
                                        .iter()
                                        .find(|l| &l.id == lid)
                                        .map(|l| l.state.on.unwrap_or(false))
                                        .unwrap_or(false)
                            })
                        })
                        .unwrap_or(false);

                    if lights_on {
                        println!(
                            "Time slot changed, transitioning to new scene with 5-min transition"
                        );
                        if let Err(err) = bridge.set_group_state(
                            &scheduled.scene_id,
                            &StateModifier::new()
                                .with_scene(scheduled.scene_id.clone())
                                .with_transition_time(
                                    (conf.scene_transition.as_millis() / 100) as u16,
                                ),
                        ) {
                            eprintln!("Failed to auto-transition scene: {}", err);
                        }
                        turn_off_orphaned_attached_lights(
                            &bridge,
                            &scheduled.scene_id,
                            &all_scenes,
                            &all_lights,
                            &conf,
                        );
                    } else {
                        // Other lights are switched off: preset the attached lights to the new
                        // scene and leave them off, so they show it once turned on again
                        preset_attached_lights_scene_state(
                            &scheduled.scene_id,
                            &all_scenes,
                            &all_lights,
                            &conf,
                        );
                        turn_off_orphaned_attached_lights(
                            &bridge,
                            &scheduled.scene_id,
                            &all_scenes,
                            &all_lights,
                            &conf,
                        );
                    }

                    active_scenes.insert(scheduled.lights_hash, scheduled.scene_id.clone());
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

                if some_lights_on && all_non_attached_turned_off {
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

/// Sets the attached lights of a scene to the state the scene defines for them and turns them
/// off right after, as the bridge doesn't accept a color for a light that is off
fn preset_attached_lights_scene_state(
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

    for light in all_lights
        .iter()
        .filter(|light| utils::is_attached_light(light) && scene_lights.contains(&light.id))
    {
        let Some(mut state) = get_scene_light_state(conf, scene_id, &light.id) else {
            continue;
        };

        state["on"] = serde_json::json!(true);
        state["transitiontime"] = serde_json::json!(0);
        println!(
            "Time slot changed, presetting attached light \"{}\" and turning it off",
            light.name
        );

        let url = format!(
            "http://{}/api/{}/lights/{}/state",
            conf.bridge_ip, conf.bridge_username, light.id
        );
        if let Err(err) = ureq::put(&url).send_json(state) {
            eprintln!("Failed to preset attached light: {}", err);
            continue;
        }

        // Give the light a moment to take the new state before turning it off
        std::thread::sleep(std::time::Duration::from_secs(1));

        if let Err(err) =
            ureq::put(&url).send_json(serde_json::json!({ "on": false, "transitiontime": 0 }))
        {
            eprintln!("Failed to turn off attached light: {}", err);
        }
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
