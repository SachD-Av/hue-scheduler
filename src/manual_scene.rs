use chrono::{DateTime, Duration, Utc};
use huelib2::resource::Scene;
use serde_json::Value;
use std::collections::HashMap;
use std::net::IpAddr;
use std::process::Command;

/// Returns the currently active scenes (v1 scene id -> time of their last recall).
///
/// Only the bridge's v2 API knows which scene is active and when it was recalled. It is
/// served over HTTPS with a self-signed certificate, so curl is used instead of ureq.
pub fn active_scene_recalls(ip: &IpAddr, username: &str) -> Option<HashMap<String, DateTime<Utc>>> {
    let output = Command::new("curl")
        .args([
            "-sk",
            "-m",
            "5",
            "-H",
            &format!("hue-application-key: {}", username),
            &format!("https://{}/clip/v2/resource/scene", ip),
        ])
        .output()
        .map_err(|err| eprintln!("Failed to run curl: {}", err))
        .ok()?;

    let json: Value = serde_json::from_slice(&output.stdout)
        .map_err(|err| eprintln!("Failed to parse active scenes: {}", err))
        .ok()?;

    let recalls = json
        .get("data")?
        .as_array()?
        .iter()
        .filter_map(|scene| {
            let status = scene.get("status")?;
            if status.get("active")?.as_str()? == "inactive" {
                return None;
            }
            let id = scene.get("id_v1")?.as_str()?.strip_prefix("/scenes/")?;
            let at = DateTime::parse_from_rfc3339(status.get("last_recall")?.as_str()?).ok()?;
            Some((id.to_string(), at.with_timezone(&Utc)))
        })
        .collect();

    Some(recalls)
}

/// Returns the name of a scene the user recalled by hand (app, remote, voice) on these lights
/// after our last command, in which case the scheduled scene must not override it.
pub fn manual_scene<'a>(
    lights_hash: u64,
    scheduled_scene_id: &str,
    all_scenes: &'a [Scene],
    recalls: &HashMap<String, DateTime<Utc>>,
    last_command: DateTime<Utc>,
) -> Option<&'a str> {
    // Margin for clock differences between the bridge and this machine
    let threshold = last_command + Duration::seconds(5);

    all_scenes
        .iter()
        .filter(|scene| scene.id != scheduled_scene_id)
        // A scene from another time slot of the schedule is left over, not chosen by hand
        .filter(|scene| !is_scheduled(&scene.name))
        .filter(|scene| {
            scene.lights.as_ref().map(crate::utils::lights_hash) == Some(lights_hash)
        })
        .find(|scene| recalls.get(&scene.id).map(|at| *at > threshold).unwrap_or(false))
        .map(|scene| scene.name.as_str())
}

/// Whether a scene name holds a time range, e.g. "Jour (10h-sunset)"
fn is_scheduled(name: &str) -> bool {
    name.split('(')
        .skip(1)
        .any(|part| part.split(')').next().map(|r| r.contains('-')).unwrap_or(false) && part.contains(')'))
}

#[cfg(test)]
mod tests {
    use super::is_scheduled;

    #[test]
    fn detects_scheduled_names() {
        assert!(is_scheduled("Jour (10h-sunset)"));
        assert!(is_scheduled("Lecture (sunset-23h)"));
        assert!(!is_scheduled("Lecture"));
        assert!(!is_scheduled("Film (soir)"));
    }
}
