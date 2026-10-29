//! The sound of going into a workspace and out of it, as Myst's linking
//! book: a sound file of your own (the `sound` setting),
//! played once as the app flies in and as it flies out, not as the dwell
//! switches workspace. None without the setting.

use bevy::prelude::*;

use crate::{WorldSystems, config::WorldConfig, flight::WorldState};

pub fn plugin(app: &mut App) {
    app.init_resource::<Chime>()
        .add_systems(
            Update,
            load.run_if(resource_changed::<WorldConfig>)
                .in_set(WorldSystems::Input),
        )
        .add_systems(OnEnter(WorldState::FlyingIn), play)
        .add_systems(OnEnter(WorldState::FlyingOut), play);
}

/// The sound, loaded, if the settings name one.
#[derive(Resource, Default)]
pub struct Chime(Option<Handle<AudioSource>>);

/// The file the settings name, wherever it is on disk, as the panorama is
/// loaded (sky.rs): the load says it may read outside any asset folder.
fn load(config: Res<WorldConfig>, assets: Res<AssetServer>, mut chime: ResMut<Chime>) {
    chime.0 = config.sound.as_ref().map(|path| {
        assets
            .load_builder()
            .override_unapproved()
            .load(path.clone())
    });
}

/// Once, on its own entity, which goes when it is done.
fn play(mut commands: Commands, chime: Res<Chime>) {
    if let Some(sound) = &chime.0 {
        commands.spawn((AudioPlayer(sound.clone()), PlaybackSettings::DESPAWN));
    }
}

#[cfg(test)]
mod tests {
    use bevy::state::app::StatesPlugin;

    use super::*;

    fn players(app: &mut App) -> usize {
        app.world_mut()
            .query::<&AudioPlayer>()
            .iter(app.world())
            .count()
    }

    /// Played once on each flight in and out, with a sound set, and never
    /// without one.
    #[test]
    fn the_sound_plays_going_in_and_out() {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, StatesPlugin, AssetPlugin::default()))
            .init_asset::<AudioSource>()
            .init_state::<WorldState>()
            .init_resource::<WorldConfig>()
            .add_plugins(plugin);
        app.update();
        assert!(
            app.world().resource::<Chime>().0.is_none(),
            "no `sound` set"
        );
        // The sound as if a setting named one, with no file to read.
        app.world_mut().resource_mut::<Chime>().0 = Some(Handle::default());
        app.update();
        assert_eq!(players(&mut app), 0, "nothing yet");

        app.world_mut()
            .resource_mut::<NextState<WorldState>>()
            .set(WorldState::FlyingOut);
        app.update();
        assert_eq!(players(&mut app), 1, "flying out");
        app.world_mut()
            .resource_mut::<NextState<WorldState>>()
            .set(WorldState::FlyingIn);
        app.update();
        assert_eq!(players(&mut app), 2, "flying in");

        app.update();
        assert_eq!(players(&mut app), 2, "once each");

        app.world_mut().resource_mut::<Chime>().0 = None;
        app.world_mut()
            .resource_mut::<NextState<WorldState>>()
            .set(WorldState::FlyingOut);
        app.update();
        assert_eq!(players(&mut app), 2, "no sound set, none played");
    }
}
