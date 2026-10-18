# aiTrack app icon

`aitrack.png` (512 px), `aitrack-<size>.png` and the multi-size `aitrack.ico` come from the icon the project owner
supplied (a navy rounded square with an orange / white / teal gauge). Corners were made transparent and the
artwork was scaled to a true square. Use `aitrack.ico` for the Windows executable and `aitrack-*.png` for the
window icon, the header logo and notifications.

## Dial only (transparent background)

`dial.png` / `dial-<size>.png` are the gauge alone on a transparent background, for dark UI backgrounds
(the white parts stay white). `dial-light.png` / `dial-light-<size>.png` are the same dial for light
backgrounds (the white parts are navy). Extracted from the supplied artwork by unmixing the four artwork
colours (white, orange, teal, ring blue-grey) from the navy background. Use these for the in-window header logo;
keep the full rounded-square `aitrack.ico` for the executable and taskbar.

## 1024 px source

`aitrack-1024.png` is redrawn from geometry by `scripts/draw-icon.py` (same design, vector-sharp) and is the
`ic10` chunk of `aitrack.icns`. The smaller PNGs and the `.ico` are still the original supplied artwork.
