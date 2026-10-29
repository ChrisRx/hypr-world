-- The nested test session's Hyprland (plugin/nested.sh): a compositor of its own in
-- a window on the desktop, for trying hypr-world plugin builds away from the
-- session's. Nothing starts with it: nested.sh loads the plugin into it and
-- starts a test client.

hl.config({
  misc = {
    force_default_wallpaper = 0,
    disable_hyprland_logo = true,
    disable_splash_rendering = true,
  },
  -- Nothing in it needs X.
  xwayland = {
    enabled = false,
  },
  -- No dialogs of Hyprland's own among the windows on the room's walls.
  ecosystem = {
    no_donation_nag = true,
    no_update_news = true,
  },
  -- Its log, in its instance's folder, for when a plugin build misbehaves.
  debug = {
    disable_logs = false,
  },
})

-- The app's own settings, as the session's plugins.lua sets them. The world
-- key launches the app the plugin was built with (target/release), its
-- pipe and log in $HYPR_WORLD_DIR. Loading the plugin reloads this file, so
-- this runs again once hypr-world is loaded.
if hl.plugin.hypr_world then
  hl.plugin.hypr_world.config({
    world = {
      panorama = "~/.config/hypr/panorama.jpg",
    },
  })
end
