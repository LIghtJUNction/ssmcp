-- Load this from the user's Hyprland Lua config after installing the updated ssmcp.
-- Use an absolute executable path if ssmcp is not on the compositor's PATH.
-- A consuming, non-repeating binding prevents a second toggle in the terminal.
-- Do not set locked=true: this switch should not enable input from the lockscreen.
hl.bind("SUPER + ALT + C", hl.dsp.exec_cmd("ssmcp computer toggle"), {
    description = "ssmcp foreground computer use",
    bypass = true,
    submap_universal = true,
})
