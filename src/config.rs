//! The app's settings, typed. The plugin sends them as one JSON object
//! (glue.rs `WorldSettings`), read here with serde into the one
//! `WorldConfig` the systems read whenever they change. A setting left out
//! takes its default. Settings that do not read, or make no sense, are
//! refused whole with an error in the log, and the last good ones stay. The
//! config is only marked changed when a value did, so the same settings
//! sent again change nothing downstream.

use std::path::PathBuf;

use bevy::{prelude::*, render::view::Msaa};
use serde::{Deserialize, Deserializer};

use crate::{WorldSystems, glue::WorldSettings, layout, player::PlayerSettings};

pub fn plugin(app: &mut App) {
    app.init_resource::<WorldConfig>().add_systems(
        PreUpdate,
        parse
            .after(WorldSystems::Feed)
            .run_if(resource_changed::<WorldSettings>),
    );
}

#[derive(Resource, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct WorldConfig {
    /// `panorama`, with `~` expanded: the image all around (sky.rs).
    #[serde(deserialize_with = "path")]
    pub panorama: Option<PathBuf>,
    /// `msaa`: 1 (or 0) is off, 2 the default, 4 or 8 samples a pixel
    /// (camera.rs). Two smooth the house's edges nearly as well as four, at
    /// 0.1 to 0.2 ms a frame.
    #[serde(deserialize_with = "msaa")]
    pub msaa: Msaa,
    /// `wall_scale` and `room_size` (layout.rs).
    #[serde(flatten)]
    pub layout: layout::Settings,
    /// `player.*` (player.rs).
    pub player: PlayerSettings,
    /// `stats`: false stops the log (stats.rs).
    pub stats: bool,
    /// `sound`, with `~` expanded: a sound played as the app flies in and
    /// out (sound.rs). None without.
    #[serde(deserialize_with = "path")]
    pub sound: Option<PathBuf>,
    /// `other_workspace_fps`: the most a second that a window on a workspace
    /// other than the one underneath the app draws (windows.rs `Sight`),
    /// 0 for no cap.
    pub other_workspace_fps: f32,
    /// `force_close_within`: seconds within which a second rocket on a
    /// window still there tells its app to end (launcher.rs `Hits`), 0
    /// never.
    pub force_close_within: f32,
}

impl Default for WorldConfig {
    fn default() -> Self {
        Self {
            panorama: None,
            msaa: Msaa::Sample2,
            layout: layout::Settings::default(),
            player: PlayerSettings::default(),
            stats: true,
            sound: None,
            other_workspace_fps: 15.0,
            force_close_within: 5.0,
        }
    }
}

impl WorldConfig {
    /// The settings read, or why they cannot be.
    pub fn from_settings(settings: &serde_json::Value) -> Result<Self, String> {
        let config = Self::deserialize(settings).map_err(|e| e.to_string())?;
        let player = &config.player;
        let amounts = [
            ("player.move_speed", player.move_speed),
            ("player.move_inertia", player.move_inertia),
            ("player.look_sensitivity", player.look_sensitivity),
            ("player.look_inertia", player.look_inertia),
            ("room_size.x", config.layout.room_size.x),
            ("room_size.y", config.layout.room_size.y),
            ("room_size.z", config.layout.room_size.z),
            ("other_workspace_fps", config.other_workspace_fps),
            ("force_close_within", config.force_close_within),
        ];
        if let Some((key, value)) = amounts.iter().find(|(_, v)| !(v.is_finite() && *v >= 0.0)) {
            return Err(format!("{key} is {value}, not a number of 0 or more"));
        }
        // A scale of zero would leave no layout. JSON has no NaN.
        if config.layout.wall_scale <= 0.0 {
            return Err(format!(
                "wall_scale is {}, not more than 0",
                config.layout.wall_scale
            ));
        }
        Ok(config)
    }
}

/// After the drain, whenever the plugin sent the settings.
fn parse(settings: Res<WorldSettings>, mut config: ResMut<WorldConfig>) {
    match WorldConfig::from_settings(&settings.0) {
        Ok(read) => {
            config.set_if_neq(read);
        }
        Err(e) => error!("settings refused, keeping the last: {e}"),
    }
}

/// A path, `~` expanded, or none for an empty one.
fn path<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<PathBuf>, D::Error> {
    let path = String::deserialize(deserializer)?;
    Ok((!path.is_empty()).then(|| expand(&path)))
}

fn msaa<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Msaa, D::Error> {
    Ok(match u32::deserialize(deserializer)? {
        0 | 1 => Msaa::Off,
        2 => Msaa::Sample2,
        4 => Msaa::Sample4,
        8 => Msaa::Sample8,
        n => {
            return Err(serde::de::Error::custom(format!(
                "msaa is {n}, not 1, 2, 4 or 8"
            )));
        }
    })
}

fn expand(path: &str) -> PathBuf {
    match (path.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => PathBuf::from(path),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn settings_are_read_over_the_defaults() {
        let config = WorldConfig::from_settings(&json!({
            "msaa": 1,
            "wall_scale": 0.5,
            "room_size": { "x": 16 },
            "player": { "flying": true, "move_speed": 6 },
            "stats": false,
            "sound": "",
            "other_workspace_fps": 30,
            "unknown": "ignored",
        }))
        .unwrap();
        assert_eq!(config.msaa, Msaa::Off);
        assert_eq!(config.layout.wall_scale, 0.5);
        assert_eq!(config.layout.room_size, Vec3::new(16.0, 0.0, 0.0));
        assert!(config.player.flying);
        assert_eq!(config.player.move_speed, 6.0);
        assert_eq!(
            config.player.move_inertia,
            PlayerSettings::default().move_inertia
        );
        assert!(!config.stats);
        assert_eq!(config.sound, None, "an empty path is none");
        assert_eq!(config.other_workspace_fps, 30.0);
        assert_eq!(
            WorldConfig::from_settings(&json!({})).unwrap(),
            WorldConfig::default()
        );
    }

    #[test]
    fn settings_that_make_no_sense_are_refused() {
        for odd in [
            json!({ "msaa": 3 }),
            json!({ "wall_scale": 0 }),
            json!({ "room_size": { "y": -2 } }),
            json!({ "player": { "flying": "yes" } }),
            json!({ "player": { "move_speed": -4 } }),
            json!({ "other_workspace_fps": -5 }),
            json!({ "player": 1 }),
            json!("world"),
        ] {
            assert!(WorldConfig::from_settings(&odd).is_err(), "{odd}");
        }
    }
}
