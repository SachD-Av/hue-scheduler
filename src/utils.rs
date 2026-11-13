use crate::config::Config;
use crate::time_range_parser::TimeRangeParser;
use chrono::{DateTime, Local, Timelike, Utc};
use huelib2::resource::group::{CreatableKind, Kind};
use huelib2::resource::{Group, Light, Scene};
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};

#[derive(Clone, PartialEq, Debug)]
pub struct ScheduledScene {
    pub scene_id: String,
    pub lights_hash: u64,
    pub start: u32,
    pub end: u32,
}

/// Returns all scheduled scenes that are active right now
pub fn get_scheduled_scenes(
    conf: &Config,
    parser: &TimeRangeParser,
    scenes: &Vec<Scene>,
    attached_light_ids: &Vec<&String>,
) -> Vec<ScheduledScene> {
    let mut scheduled_scenes = HashMap::<u64, ScheduledScene>::new();
    let date_time = DateTime::<Utc>::from(Local::now()).with_timezone(&conf.home_timezone);
    let now = date_time.hour() * 60 + date_time.minute();

    // Group scenes by their lights
    for scene in scenes {
        let time_ranges = parser.extract_time_ranges(&scene.name);

        let Some(time_range) = time_ranges
            .iter()
            .find(|range| parser.matches_time_range(range, now))
        else {
            continue;
        };

        let Some(lights) = &scene.lights else {
            continue;
        };

        // Group by non-attached lights only, so a scene that additionally includes an
        // attached light competes with (and transitions to) the scenes without it
        let mut sorted_lights = lights
            .iter()
            .filter(|id| !attached_light_ids.contains(id))
            .cloned()
            .collect::<Vec<String>>();
        if sorted_lights.is_empty() {
            sorted_lights = lights.clone();
        }
        sorted_lights.sort();

        let mut hash = DefaultHasher::new();
        sorted_lights.hash(&mut hash);

        let scene_id = hash.finish();

        // Check if scene is closer to now than this one
        if let Some(last_scene) = scheduled_scenes.get(&scene_id) {
            if last_scene.start > time_range.0 {
                continue;
            }
        }

        scheduled_scenes.insert(
            scene_id,
            ScheduledScene {
                scene_id: scene.id.clone(),
                lights_hash: scene_id,
                start: time_range.0,
                end: time_range.1,
            },
        );
    }

    scheduled_scenes
        .values()
        .cloned()
        .collect::<Vec<ScheduledScene>>()
}

pub fn get_sunrise_sunset(latitude: f64, longitude: f64) -> Option<(u32, u32)> {
    let (sunrise, sunset) =
        sun_times::sun_times(Local::now().date_naive(), latitude, longitude, 0f64)?;
    // sun_times returns UTC; convert to local time before comparing with local slots
    let sunrise = sunrise.with_timezone(&Local);
    let sunset = sunset.with_timezone(&Local);

    Some((
        sunrise.hour() * 60 + sunrise.minute(),
        sunset.hour() * 60 + sunset.minute(),
    ))
}

pub fn is_attached_light(light: &Light) -> bool {
    light.name.ends_with("(att)")
}

/// Returns attached lights that share a room with the scene's lights but are not part of the
/// scene itself and are currently on. Recalling the scene leaves them untouched otherwise.
pub fn get_orphaned_attached_lights(
    scene_lights: &Vec<String>,
    groups: &Vec<Group>,
    lights: &Vec<Light>,
) -> Vec<String> {
    groups
        .iter()
        .filter(|group| {
            group.kind == Kind::Creatable(CreatableKind::Room)
                && group.lights.iter().any(|id| scene_lights.contains(id))
        })
        .flat_map(|group| group.lights.iter())
        .filter(|id| !scene_lights.contains(id))
        .filter(|id| {
            lights
                .iter()
                .find(|light| &light.id == *id)
                .map(|light| is_attached_light(light) && light.state.on.unwrap_or(false))
                .unwrap_or(false)
        })
        .cloned()
        .collect::<Vec<String>>()
}
