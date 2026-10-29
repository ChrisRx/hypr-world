//! When the app draws a frame: only in an update that changed what is on
//! screen, and never while it is out of view. Every update still runs, for
//! the plugin's lines and the copies, but one that changed nothing leaves the
//! camera inactive: then Bevy acquires, draws and presents nothing, and
//! Hyprland keeps showing the last frame.
//!
//! What counts as a change is ECS change detection over everything the
//! renderer reads. A system that writes a component every update whether or
//! not the value changed would mark it changed every time, so those compare
//! first (player.rs `eyes`).

use std::time::Duration;

use bevy::{
    asset::AssetEventSystems,
    camera::visibility::VisibilitySystems,
    ecs::system::SystemParam,
    light::Skybox,
    prelude::*,
    render::{
        ExtractSchedule, MainWorld, RenderApp,
        render_resource::PipelineCache,
        view::{Msaa, screenshot::Screenshot},
    },
    transform::TransformSystems,
    window::{RequestRedraw, WindowResized},
};

use crate::{FlyCamera, glue::Desktop, stats::Stats, windows::WindowQuad};

/// How long frames go on after something new appears. Bevy prepares a new
/// mesh, image or material for the GPU in the background, and one may come
/// out a few updates after it was added.
const SETTLE: Duration = Duration::from_millis(500);

pub fn plugin(app: &mut App) {
    // After the transforms and the asset events of the update are in, and
    // before the visibility pass: Bevy finds what a camera sees only while
    // it is active, so a camera switched on any later would draw an empty
    // frame, everything hidden.
    app.init_resource::<PipelinesReady>().add_systems(
        PostUpdate,
        draw_on_change
            .after(TransformSystems::Propagate)
            .after(AssetEventSystems)
            .before(VisibilitySystems::CheckVisibility),
    );

    if let Some(render_app) = app.get_sub_app_mut(RenderApp) {
        render_app.add_systems(ExtractSchedule, report_pipelines);
    }
}

/// Every pipeline the render world asked for has compiled, as of the last
/// extraction. Until then the frames go on, or the last one drawn could lack
/// whatever was still compiling. As in Bevy's examples/showcase/loading_screen.rs.
#[derive(Resource, Default, PartialEq)]
struct PipelinesReady(bool);

/// In the render world, at extraction: writes the main world's
/// `PipelinesReady`.
fn report_pipelines(mut main_world: ResMut<MainWorld>, pipelines: Res<PipelineCache>) {
    if let Some(mut ready) = main_world.get_resource_mut::<PipelinesReady>() {
        ready.set_if_neq(PipelinesReady(
            pipelines.waiting_pipelines().next().is_none(),
        ));
    }
}

/// What is drawn: meshes, and the camera.
type Drawn = Or<(With<Mesh3d>, With<Camera>)>;
/// Moved, came, or was shown or hidden.
type Moved = Or<(Changed<GlobalTransform>, Changed<Visibility>)>;
/// A mesh given another material, or the camera other settings.
type Restyled = Or<(
    Changed<MeshMaterial3d<StandardMaterial>>,
    Changed<Skybox>,
    Changed<Msaa>,
    Changed<Projection>,
)>;

/// Everything the renderer reads that the app changes.
#[derive(SystemParam)]
struct Picture<'w, 's> {
    moved: Query<'w, 's, (), (Drawn, Moved)>,
    restyled: Query<'w, 's, (), (Restyled, Without<WindowQuad>)>,
    /// New meshes, for the settling.
    added: Query<'w, 's, (), Added<Mesh3d>>,
    /// Meshes gone, and the sky.
    removed: RemovedComponents<'w, 's, Mesh3d>,
    skies: RemovedComponents<'w, 's, Skybox>,
    /// The hint.
    text: Query<'w, 's, (), Changed<Text>>,
    /// A screenshot the control pipe asked for, until it is saved.
    screenshots: Query<'w, 's, (), With<Screenshot>>,
    /// A window's new dmabuf frame: its quad on the material of the buffer it
    /// came in (windows.rs `show_slot`). Pixels copied out of shared memory
    /// fill an image, an asset change.
    framed: Query<'w, 's, (), (With<WindowQuad>, Changed<MeshMaterial3d<StandardMaterial>>)>,
    clear: Res<'w, ClearColor>,
    resized: MessageReader<'w, 's, WindowResized>,
    images: MessageReader<'w, 's, AssetEvent<Image>>,
    standard: MessageReader<'w, 's, AssetEvent<StandardMaterial>>,
}

impl Picture<'_, '_> {
    /// What changed on screen in this update, each by the name the stats
    /// count an update drawn for it by, and whether something new came: a
    /// mesh, an image or a material. Reads every message through, so each
    /// counts once.
    fn changes(&mut self) -> ([(&'static str, bool); 8], bool) {
        let (images, new_image) = drain(&mut self.images);
        let (standard, new_standard) = drain(&mut self.standard);

        let changes = [
            ("moved", !self.moved.is_empty()),
            ("restyled", !self.restyled.is_empty()),
            (
                "removed",
                self.removed.read().count() + self.skies.read().count() > 0,
            ),
            ("text", !self.text.is_empty()),
            ("frame", !self.framed.is_empty()),
            ("assets", images || standard),
            (
                "window",
                self.resized.read().count() > 0 || self.clear.is_changed(),
            ),
            ("screenshot", !self.screenshots.is_empty()),
        ];
        let new = !self.added.is_empty() || new_image || new_standard;
        (changes, new)
    }
}

/// Whether there were any asset events, and whether one was a new asset.
fn drain<A: Asset>(events: &mut MessageReader<AssetEvent<A>>) -> (bool, bool) {
    events.read().fold((false, false), |(_, new), event| {
        (true, new || matches!(event, AssetEvent::Added { .. }))
    })
}

/// Decides whether this update draws a frame. Extraction, at the end of the
/// update, reads `is_active` as this leaves it.
#[allow(clippy::too_many_arguments)]
fn draw_on_change(
    time: Res<Time<Real>>,
    desktop: Res<Desktop>,
    ready: Res<PipelinesReady>,
    mut picture: Picture,
    mut camera: Single<&mut Camera, With<FlyCamera>>,
    mut stats: ResMut<Stats>,
    mut redraw: MessageWriter<RequestRedraw>,
    mut settling: Local<Duration>,
) {
    let (changes, new) = picture.changes();
    *settling = if new {
        SETTLE
    } else {
        settling.saturating_sub(time.delta())
    };
    let compiling = !ready.0;

    // Out of view nothing on screen is the app's. Without the plugin's
    // feed (no monitor) the app is a plain window, always in view.
    let in_view = desktop.in_view();
    let reasons = changes
        .into_iter()
        .chain([("settling", !settling.is_zero()), ("compiling", compiling)]);
    let draw = in_view && reasons.clone().any(|(_, why)| why);

    if draw {
        for (name, _) in reasons.filter(|&(_, why)| why) {
            stats.drawn_for(name);
        }
    }
    let screenshot = !picture.screenshots.is_empty();
    // Settling, compiling and a screenshot end on their own, with nothing to
    // wake the app for them: they keep the updates coming until they do. A
    // screenshot is saved an update or two after its frame comes back.
    if in_view && (!settling.is_zero() || compiling || screenshot) {
        redraw.write(RequestRedraw);
    }
    if camera.is_active != draw {
        camera.is_active = draw;
    }
}

#[cfg(test)]
mod tests {
    use bevy::{
        asset::AssetPlugin, camera::CameraPlugin, mesh::MeshPlugin, time::TimeUpdateStrategy,
    };

    use super::*;
    use crate::glue::Monitor;

    /// The plugin as the app runs it, with Bevy's own visibility pass,
    /// headless: a camera, one quad in front of it, and the resources the
    /// decision reads. No glue: its thread would connect to a running plugin.
    fn app() -> (App, Entity) {
        let mut app = App::new();
        app.add_plugins((
            MinimalPlugins,
            TransformPlugin,
            AssetPlugin::default(),
            MeshPlugin,
            CameraPlugin,
            plugin,
        ))
        .init_asset::<Image>()
        .init_asset::<StandardMaterial>()
        .add_message::<WindowResized>()
        .add_message::<RequestRedraw>()
        .init_resource::<Desktop>()
        .init_resource::<Stats>()
        .insert_resource(PipelinesReady(true))
        .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(
            100,
        )));

        app.world_mut().spawn((
            Camera3d::default(),
            FlyCamera {
                yaw: 0.0,
                pitch: 0.0,
            },
            Transform::default(),
        ));
        let quad = app
            .world_mut()
            .resource_mut::<Assets<Mesh>>()
            .add(Rectangle::new(1.0, 1.0));
        let mesh = app
            .world_mut()
            .spawn((Mesh3d(quad), Transform::from_xyz(0.0, 0.0, -3.0)))
            .id();
        (app, mesh)
    }

    /// Whether the camera draws this update, and whether it sees the quad.
    fn frame(app: &mut App, mesh: Entity) -> (bool, bool) {
        let mut cameras = app.world_mut().query::<&Camera>();
        let drawing = cameras.single(app.world()).expect("one camera").is_active;
        let seen = app
            .world()
            .get::<ViewVisibility>(mesh)
            .expect("the quad")
            .get();
        (drawing, seen)
    }

    fn nudge(app: &mut App, mesh: Entity) {
        app.world_mut()
            .get_mut::<Transform>(mesh)
            .expect("the quad")
            .translation
            .x += 0.1;
    }

    #[test]
    fn draws_only_when_something_changed() {
        let (mut app, mesh) = app();

        // Something new: drawn until it has settled, then no more.
        app.update();
        assert_eq!(
            frame(&mut app, mesh),
            (true, true),
            "a new quad is drawn, and seen"
        );
        for _ in 0..8 {
            app.update();
        }
        assert!(!frame(&mut app, mesh).0, "a still scene is not drawn");

        // The first frame after a pause sees what is there: the camera came
        // on before the visibility pass, not after it.
        nudge(&mut app, mesh);
        app.update();
        assert_eq!(
            frame(&mut app, mesh),
            (true, true),
            "a quad that moved is drawn, and seen"
        );
        app.update();
        assert!(!frame(&mut app, mesh).0, "and drawn only once");

        // The plugin's feed says the app is out of view: nothing is drawn.
        app.world_mut().resource_mut::<Desktop>().monitor = Some(Monitor {
            size: Vec2::new(1440.0, 960.0),
            scale: 2.0,
            work: Rect::new(0.0, 40.0, 1440.0, 960.0),
        });
        nudge(&mut app, mesh);
        app.update();
        assert!(!frame(&mut app, mesh).0, "out of view nothing is drawn");
    }
}
