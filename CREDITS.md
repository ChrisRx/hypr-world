# Credits

## Hypr3D

hypr-world began as an experiment in reimplementing
[Hypr3D](https://github.com/samine825/Hypr3D) by samine825, a Hyprland
plugin that renders the desktop as a 3D room, as a separate Bevy process.
It has since gone its own way and does not aim to match Hypr3D's behavior,
but its starting points came from there: the house around the workspace
frames and their layout on its walls, the lighting model, the player's
movement and eased look, the camera's flight in and out of the active
workspace, and the panorama mapping behind the house.

Hypr3D is distributed under the MIT License:

```
MIT License

Copyright (c) 2026 samine825

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

## font8x8

`src/font8x8.rs`, the bitmap font the workspace labels are drawn in, is
font8x8_basic by Daniel Hepper, after Marcel Sondaar's and IBM's VGA fonts.
Public domain.

## hyprland-toplevel-export-v1

`protocols/hyprland-toplevel-export-v1.xml` is from
[hyprland-protocols](https://github.com/hyprwm/hyprland-protocols),
Copyright © 2022 Vaxry, under the BSD 3-Clause license reproduced in the
file itself.
