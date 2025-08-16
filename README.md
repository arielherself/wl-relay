# wl-relay

This is a Wayland socket forwarder that hopefully prevents applications from knowing if the window is focussed, by hijacking the communication between the Wayland compositor and any clients.

## Usage

```
WAYLAND_DISPLAY=wayland-100 <your-command>
```

## Implementation

This proxy provides a byte-to-byte forwarding, but blocks two types of Wayland events: `wl_keyboard.leave` and `xdg_toplevel.configure`.

### `wl_keyboard.leave`

Here is the complete process of receiving a `wl_keyboard.leave` event.

```
client: wl_display.get_registry
server: wl_registry.global (provides the handle of "wl_seat")
client: wl_registry.bind (bind "wl_seat" to object id)
client: wl_seat.get_keyboard (bind "wl_keyboard" to object id)
server: wl_keyboard.leave
```

### `xdg_toplevel.configure`

Here is the complete process of receiving a `xdg_toplevel.configure` event.

```
client: wl_display.get_registry
server: wl_registry.global (provides the handle of "xdg_wm_base")
client: wl_registry.bind (bind "xdg_wm_base" to object id)
client: xdg_wm_base.get_xdg_surface (bind "xdg_surface" to object id)
client: xdg_surface.get_toplevel (bind "xdg_toplevel" to object id)
server: xdg_toplevel.configure
```
The compositor triggers a `xdg_toplevel.configure` event every time the status of the window is changed (e.g. resize, fullscreen, minimized). MPV uses the `activated` property to determine if the window is focussed.

`wl-relay` blocks all `configure` events with `activated == false` after the first time `activated` is set to `true`. To the application, this behavior is like that the user opens up the window and never "leaves" it.
