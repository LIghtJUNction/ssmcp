# SSMCP for Omarchy

This Omarchy Shell widget embeds a foreground SSMCP terminal in its popup. It needs Omarchy with Quickshell plugins and the Qt 6 `qmltermwidget` package.

From the SSMCP checkout, build and install the CLI and enable the widget.

```sh
sudo pacman -S --needed qmltermwidget
cargo build --locked --release --bin ssmcp --target-dir target
install -Dm755 target/release/ssmcp "${XDG_DATA_HOME:-$HOME/.local/share}/ssmcp-omarchy/bin/ssmcp"
plugin_dir="${XDG_CONFIG_HOME:-$HOME/.config}/omarchy/plugins/io.github.lightjunction.ssmcp"
mkdir -p "$plugin_dir"
cp -R packaging/omarchy/. "$plugin_dir/"
omarchy plugin validate "$plugin_dir"
omarchy-shell shell rescanPlugins
omarchy plugin enable io.github.lightjunction.ssmcp
```

After copying the plugin, Omarchy may take a moment to discover it. Check `omarchy plugin list` before enabling it.

Left-click the status bar widget to configure it. Enter a working directory (absolute or `~/…`) and replace `username@server` in `ssmcp username@server --control-only`, then save. Edit the entire command to add flags or remove `--control-only` for the regular interactive session. Use shell quoting for arguments containing spaces and `env KEY=value ssmcp …` for environment overrides. Changes apply to the next launch.

Left-click to show or collapse the terminal. The first open starts the configured session. Collapsing the popup keeps the session running, including when you click outside it, and the next open returns to the same terminal. Use **Close session** in the terminal popup or right-click the widget to open settings and close the session there.

The number in the bar counts native tool calls admitted by that session over the last 10 minutes, including the initial authorized capability probe and calls whose execution returned an error. Tool listings, relay maintenance frames, and unauthorized calls are excluded. A dash indicates that the widget has no session statistics yet.

The widget uses its dedicated CLI install if available, then searches `PATH`. A CLI built before widget support can open a terminal but cannot publish its count. To remove the widget from the bar, first close the session, then run `omarchy plugin disable io.github.lightjunction.ssmcp`.
