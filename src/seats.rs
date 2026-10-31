//! How many seats there are upstairs (layout.rs): the `workspaces` setting
//! if it is set, else as many as Hyprland's binds and rules say, else ten.
//! Hyprland makes a workspace when it is first used and drops it once it is
//! empty, so it has no fixed number of them, but a setup has one in effect:
//! the number keys bound to them. A bind made in Lua shows only as `__lua`,
//! with no workspace, so a run of 1, 2, 3 and on (0 for ten) under one set of
//! modifiers counts as that many. A bind written the old way to `workspace
//! N`, and a persistent workspace rule for N, count as N. Hyprland's IPC
//! socket is asked off the main thread at the start and whenever the
//! settings come again, which the plugin sends on every config reload.

use std::collections::HashMap;

use bevy::{
    prelude::*,
    tasks::{IoTaskPool, Task, futures::check_ready},
};
use serde_json::Value;

use crate::{
    WorldSystems,
    glue::{self, WorldSettings},
    wake::Wake,
};

/// With nothing set and nothing found: the number row.
const DEFAULT: u32 = 10;
/// Fewer number keys in a row than this are bound for something else.
const LEAST_RUN: u32 = 3;

pub fn plugin(app: &mut App) {
    app.init_resource::<Seats>()
        .add_systems(Update, ask.before(WorldSystems::House));
}

/// The seats Hyprland's binds and rules make, once it has said.
#[derive(Resource, Default, PartialEq, Debug)]
pub struct Seats(Option<u32>);

impl Seats {
    /// `set`, the `workspaces` setting, unless it is 0, else what Hyprland
    /// said, else ten.
    pub fn count(&self, set: u32) -> u32 {
        if set > 0 {
            set
        } else {
            self.0.unwrap_or(DEFAULT)
        }
    }
}

/// Asks Hyprland whenever the settings come, and takes in what it says.
/// Nothing heard keeps what was.
fn ask(
    settings: Res<WorldSettings>,
    wake: Res<Wake>,
    mut seats: ResMut<Seats>,
    mut asking: Local<Option<Task<Option<u32>>>>,
) {
    if settings.is_changed() {
        let wake = wake.clone();
        *asking = Some(IoTaskPool::get().spawn(async move {
            let found = from_hyprland();
            wake.wake();
            found
        }));
    }
    let Some(found) = asking.as_mut().and_then(check_ready) else {
        return;
    };
    *asking = None;
    if found.is_some() {
        seats.set_if_neq(Seats(found));
    }
}

fn from_hyprland() -> Option<u32> {
    let binds = glue::ask_hyprland("j/binds")?;
    let rules = glue::ask_hyprland("j/workspacerules").unwrap_or(Value::Null);
    let found = bound(&binds).max(persistent(&rules));
    (found > 0).then_some(found)
}

/// The seats `binds` (`j/binds`) make: the longest run of number keys from
/// 1 under one set of modifiers, and the highest workspace bound by number.
fn bound(binds: &Value) -> u32 {
    let mut rows: HashMap<i64, [bool; 10]> = HashMap::new();
    let mut highest = 0;
    for bind in binds.as_array().into_iter().flatten() {
        let text = |key: &str| bind.get(key).and_then(Value::as_str).unwrap_or_default();
        if matches!(
            text("dispatcher"),
            "workspace" | "movetoworkspace" | "movetoworkspacesilent"
        ) && let Ok(n) = text("arg").trim().parse::<u32>()
        {
            highest = highest.max(n);
        }
        // 1 to 9, then 0 for ten.
        if let Ok(digit) = text("key").parse::<usize>()
            && digit < 10
        {
            let modmask = bind.get("modmask").and_then(Value::as_i64).unwrap_or(0);
            rows.entry(modmask).or_default()[(digit + 9) % 10] = true;
        }
    }
    let run = rows
        .values()
        .map(|row| row.iter().take_while(|&&bound| bound).count() as u32)
        .filter(|&run| run >= LEAST_RUN)
        .max()
        .unwrap_or(0);
    run.max(highest)
}

/// The highest workspace a rule of `rules` (`j/workspacerules`) keeps
/// whether it is used or not.
fn persistent(rules: &Value) -> u32 {
    rules
        .as_array()
        .into_iter()
        .flatten()
        .filter(|rule| rule.get("persistent").and_then(Value::as_bool) == Some(true))
        .filter_map(|rule| rule.get("workspaceString")?.as_str()?.parse::<u32>().ok())
        .max()
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn lua(key: &str, modmask: i64) -> Value {
        json!({ "key": key, "modmask": modmask, "dispatcher": "__lua", "arg": "51" })
    }

    #[test]
    fn the_number_keys_bound_in_lua_are_the_seats() {
        let row = |keys: &[&str], modmask| keys.iter().map(|k| lua(k, modmask)).collect::<Vec<_>>();
        let all = ["1", "2", "3", "4", "5", "6", "7", "8", "9", "0"];
        let mut binds = row(&all, 64);
        binds.extend(row(&all[..4], 65));
        binds.push(lua("Q", 64));
        assert_eq!(bound(&Value::Array(binds)), 10, "1 to 0");

        assert_eq!(bound(&Value::Array(row(&all[..6], 8))), 6);
        assert_eq!(
            bound(&Value::Array(row(&["1", "2", "4", "5"], 64))),
            0,
            "two in a row are not the number row"
        );
        assert_eq!(
            bound(&Value::Array(row(&["2", "3", "4"], 64))),
            0,
            "nor ones that do not start at 1"
        );
    }

    #[test]
    fn workspaces_bound_or_kept_by_number_are_seats() {
        let binds = json!([
            { "key": "F1", "modmask": 64, "dispatcher": "workspace", "arg": "7" },
            { "key": "F2", "modmask": 64, "dispatcher": "workspace", "arg": "name:web" },
            { "key": "F3", "modmask": 64, "dispatcher": "exec", "arg": "12" },
        ]);
        assert_eq!(bound(&binds), 7);

        let rules = json!([
            { "workspaceString": "4", "persistent": true },
            { "workspaceString": "12", "persistent": false },
            { "workspaceString": "name:mail", "persistent": true },
        ]);
        assert_eq!(persistent(&rules), 4);
        assert_eq!(persistent(&Value::Null), 0);
    }

    #[test]
    fn a_setting_wins_and_ten_is_the_last_word() {
        assert_eq!(Seats(Some(6)).count(8), 8);
        assert_eq!(Seats(Some(6)).count(0), 6);
        assert_eq!(Seats(None).count(0), 10);
    }
}
