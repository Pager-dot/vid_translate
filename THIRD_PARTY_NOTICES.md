# Third-party notices

This file records license notices for third-party components whose source is
included in, or adapted into, this repository.

## Mimi

The user interface in `src/ui/`, `src/overlay.css`, `src/settings.css` and the
base layer of `src/App.css` is derived from **Mimi** by yuxino
(<https://github.com/yuxino/Mimi>), used under the MIT license.

Components ported with little or no change: `PulseRing` and its stylesheet (the
"sound light" activity indicator), `Select` and `select.css`, `Tooltip`,
`tooltipPosition`, `Switch`, `ControlButton` and `control-button.css`, and the
`Icon` wrapper around Lucide. The overlay chrome, the settings console layout
and their design tokens follow Mimi's `src/windows/overlay/` and
`src/windows/settings/settings.css`.

Copyright (c) 2026 yuxino

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

## Lucide

Icons are rendered with `lucide-react` (<https://lucide.dev>), under the ISC
license. See `node_modules/lucide-react/LICENSE` in a built checkout for the
full text.
