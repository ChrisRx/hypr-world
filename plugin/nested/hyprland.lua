-- The nested test session's Hyprland (plugin/nested.sh): a compositor of its own in
-- a window on the desktop, for trying hypr-world plugin builds away from the
-- session's. Nothing starts with it: nested.sh loads the plugin into it and
-- starts a test client.

hl.config({
  input = {
    kb_options = "caps:ctrl_modifier",

    -- Natural scrolling: content follows the fingers, macOS-style. This is the
    -- touchpad knob only; input.natural_scroll covers mice and wheels and is
    -- left alone on purpose.
    touchpad = {
      natural_scroll = true,

      -- [1.0] two-finger scroll rate. Half speed; the default overshot every
      -- short flick.
      --
      -- Note apps that curve scrolling themselves (electron, anything with its
      -- own momentum) respond to this less than native gtk/qt ones do.
      scroll_factor = 0.8,
    },
  },
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

-- The headless output `nested.sh backend` draws on, the size of the
-- session's monitor, which nested.sh says when it starts this.
local mode = os.getenv("HYPR_WORLD_MODE")
if mode and mode ~= "" then
  hl.monitor({
    output = "hypr-world",
    mode = mode,
    position = "0x0",
    scale = tonumber(os.getenv("HYPR_WORLD_SCALE")) or 1,
  })
end

-- The app's own settings, as the session's plugins.lua sets them. The world
-- key launches the app the plugin was built with (target/release), its
-- pipe and log in $HYPR_WORLD_DIR. Loading the plugin reloads this file, so
-- this runs again once hypr-world is loaded.
if hl.plugin.hypr_world then
  -- The world key. Not Super+L: the session binds that to its own world,
  -- and takes it before the nested window sees it. Not with Alt either:
  -- Super+Left Alt switches between looking and typing.
  hl.bind("SUPER + CTRL + L", hl.plugin.hypr_world.toggle)

  -- A terminal, from inside the world (nested.sh backend): Super+Return is
  -- the session's own.
  hl.bind("SUPER + CTRL + Return", hl.dsp.exec_cmd("alacritty"))

  -- rofi over the app in the session, launching in here (nested.sh rofi):
  -- in here it would be on the overlay layer, which the walls leave out.
  local nested = os.getenv("HYPR_WORLD_NESTED_SH")
  if nested then
    hl.bind("SUPER + CTRL + Space", hl.dsp.exec_cmd(nested .. " rofi"))
  end

  hl.plugin.hypr_world.config({
    world = {
      panorama = "~/.config/hypr/panorama.jpg",
    },
  })
end
