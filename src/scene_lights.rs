use huelib2::resource::Light;
use serde_json::Value;
use std::net::IpAddr;

/// Whether the light is a genuine Philips Hue (Signify) bulb.
pub fn is_hue_light(light: &Light) -> bool {
    light
        .manufacturer_name
        .as_deref()
        .map(|m| m.contains("Signify") || m.contains("Philips"))
        .unwrap_or(false)
}

/// Sends each non-Hue light of a scene its target state individually.
///
/// A group scene recall relies on the scene being stored inside each bulb, which non-Hue
/// bulbs often handle badly (scene missing or outdated, long transitions ignored). Sending
/// the scene's light states one by one, like the Hue app does, works with those bulbs too.
/// Only lights that are currently on are touched.
pub fn apply_to_non_hue_lights(
    ip: &IpAddr,
    username: &str,
    scene_id: &str,
    all_lights: &[Light],
    transition_time: u16,
) {
    let base = format!("http://{}/api/{}", ip, username);

    let scene: Value = match ureq::get(&format!("{}/scenes/{}", base, scene_id)).call() {
        Ok(response) => match response.into_json() {
            Ok(json) => json,
            Err(err) => {
                eprintln!("Failed to parse scene {}: {}", scene_id, err);
                return;
            }
        },
        Err(err) => {
            eprintln!("Failed to retrieve scene {}: {}", scene_id, err);
            return;
        }
    };

    let Some(light_states) = scene.get("lightstates").and_then(|v| v.as_object()) else {
        eprintln!("Scene {} has no light states", scene_id);
        return;
    };

    for (light_id, state) in light_states {
        let Some(light) = all_lights.iter().find(|l| &l.id == light_id) else {
            continue;
        };
        if is_hue_light(light) || !light.state.on.unwrap_or(false) {
            continue;
        }
        let Some(state) = state.as_object() else {
            continue;
        };

        let mut body = state.clone();
        body.insert("transitiontime".to_string(), Value::from(transition_time));

        match ureq::put(&format!("{}/lights/{}/state", base, light_id))
            .send_json(Value::Object(body))
        {
            Ok(response) => {
                let errors = response
                    .into_json::<Value>()
                    .ok()
                    .and_then(|v| v.as_array().cloned())
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|r| r.get("error").cloned())
                    .collect::<Vec<Value>>();
                if errors.is_empty() {
                    println!("Sent scene state to non-Hue light \"{}\"", light.name);
                } else {
                    eprintln!(
                        "Bridge rejected state for light \"{}\": {:?}",
                        light.name, errors
                    );
                }
            }
            Err(err) => eprintln!("Failed to set state of light \"{}\": {}", light.name, err),
        }
    }
}
