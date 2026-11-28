# App icon

A dark (`#0C0C0E`) tile with a desk: white legs under a lemon (`#EFFF3A`) top
that is also a pen, with a nib and a cap. The pen's upper edge and the desk top
are flush, and the nib and cap are no thicker than the legs.

`icon-small.svg` is used up to 32 px: it has a thicker desk top and wider gaps.
`icon-large.svg` is used for larger sizes and for the Android vector.

After editing either SVG, regenerate the checked-in assets from the repository
root. The script needs the Python standard library and the librsvg and cairo
shared libraries.

```sh
python3 -I crates/fuzukue/assets/app-icon/generate.py
```

| Output | Use |
| --- | --- |
| `windows/fuzukue.ico` (16–256 px) | `crates/fuzukue/build.rs` embeds it as icon resource 1, which serves the executable, window, and taskbar |
| `linux/window-icon.png` | The X11 window icon. Wayland takes icons from desktop entries, which the installers do not create |
| `macos/fuzukue.iconset/` | `scripts/release/package-macos.sh` turns it into `fuzukue.icns` with `iconutil` |
| `android/app/src/main/res/drawable/ic_launcher_foreground.xml` | The adaptive launcher icon's foreground |
| `android/app/src/main/res/mipmap-*dpi/ic_launcher.png` | The legacy launcher icon |

The adaptive icon's background color and layer list are maintained by hand in
`values/ic_launcher_background.xml` and `mipmap-anydpi-v26/ic_launcher.xml`
under the Android resource directory.
